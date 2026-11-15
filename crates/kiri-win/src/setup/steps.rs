use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::app::Handle;
use crate::engine::{tr, trf};

use super::download::{download, elevated};
use super::{
    CANCEL, KIRI_DISTRO, STATE, UBUNTU, UBUNTU_DISTRO, WSL_MSI, bundle, dir, distros, existing_ubuntu, log_line,
    now_secs, quiet, save, saved, task, tasks, update, verified, wsl_installed, wsl_text,
};

/// Step 1: download WSL, then in one elevated run turn on Virtual Machine
/// Platform and install WSL. Exit 3010 from either means a restart.
pub(super) fn install_wsl() -> io::Result<()> {
    update(|s| s.tasks = tasks(&[tr("Download WSL"), tr("Administrator approval"), tr("Turn on Virtual Machine Platform"), tr("Install WSL")]));
    // WSL_MSI is this build's architecture's; on another PC it would not install.
    if let Some(e) = crate::win::arch_mismatch() {
        return Err(io::Error::other(e));
    }
    let msi = download(&WSL_MSI, 0)?;
    update(|s| s.progress = Some(json!({"pct": null, "done": null, "total": null, "rate": null, "eta": null, "label": tr("Waiting for Windows' approval")})));
    task(1, "active", &tr("Windows is asking"));
    // The downloaded MSI lives in a user-writable folder, so a process at
    // the user's own integrity could swap it between its SHA-256 check above
    // and the elevated install. kiri-admin.ps1 -Wsl re-verifies it, as admin,
    // on a copy the user cannot touch, and requires it to be Microsoft-signed,
    // before running it; its exit codes are read below.
    let msi = msi.to_string_lossy();
    let code = elevated(&["-Wsl", "-Msi", &msi, "-Sha256", WSL_MSI.sha256])?;
    task(1, "done", &tr("approved"));
    match code {
        22 => return Err(io::Error::other("the downloaded WSL installer changed on disk; try again")),
        23 => return Err(io::Error::other("the WSL installer is not validly signed by Microsoft")),
        20 => {
            task(2, "error", "");
            return Err(io::Error::other("Windows could not turn on Virtual Machine Platform (is virtualization on in the BIOS?)"));
        }
        21 => {
            task(2, "done", "");
            task(3, "error", "");
            return Err(io::Error::other("the WSL installer failed"));
        }
        _ => {}
    }
    task(2, "done", "");
    task(3, "done", "");
    if code == 3010 || !wsl_installed() {
        let mut s = saved();
        s.restart_after = Some(now_secs());
        save(&s);
        // After the restart, Kiri opens once by itself and carries on.
        if let Ok(exe) = std::env::current_exe() {
            let registered = quiet("reg.exe")
                .args(["add", r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce", "/v", "KiriSetup", "/t", "REG_SZ", "/d", &format!("\"{}\"", exe.display()), "/f"])
                .status()
                .is_ok_and(|s| s.success());
                if !registered {
                    return Err(io::Error::other("Windows would not register Kiri to carry on after the restart"));
                }
        }
        update(|s| {
            s.step = "restart";
            s.progress = None;
        });
    } else {
        update(|s| {
            s.step = "choose";
            s.progress = None;
        });
    }
    Ok(())
}

/// Step 2: Kiri Linux from the bundle, or Ubuntu (already here, or downloaded).
pub(super) fn install_linux(kind: &str) -> io::Result<()> {
    // The engine and Kiri Linux in the bundle are this build's architecture.
    if let Some(e) = crate::win::arch_mismatch() {
        return Err(io::Error::other(e));
    }
    let base = dir().join("wsl");
    std::fs::create_dir_all(&base)?;
    let distro = if kind == "kiri" {
        update(|s| s.tasks = tasks(&[tr("Unpack Kiri Linux"), tr("Add it to WSL"), tr("Start it")]));
        task(0, "active", "");
        let tar = verified("kiri-linux.tar.gz")?;
        task(0, "done", &format!("{} MB", std::fs::metadata(&tar)?.len().div_ceil(1_000_000)));
        task(1, "active", "");
        if !distros().iter().any(|d| d == KIRI_DISTRO) {
            log_line(&format!("wsl --import {KIRI_DISTRO} {} kiri-linux.tar.gz --version 2", base.join(KIRI_DISTRO).display()));
            wsl_ok(quiet("wsl.exe").args(["--import", KIRI_DISTRO]).arg(base.join(KIRI_DISTRO)).arg(&tar).args(["--version", "2"]))?;
        }
        task(1, "done", "");
        task(2, "active", "");
        wsl_ok(quiet("wsl.exe").args(["-d", KIRI_DISTRO, "-u", "root", "--", "/usr/local/bin/kiri-start"]))?;
        task(2, "done", "");
        KIRI_DISTRO.to_owned()
    } else {
        let existing = existing_ubuntu();
        let distro = match existing {
            Some(d) => {
                update(|s| s.tasks = tasks(&[tr("Use the Ubuntu already here"), tr("Install Kiri's engine in it"), tr("Start it")]));
                task(0, "done", &d);
                d
            }
            None => {
                update(|s| s.tasks = tasks(&[tr("Download Ubuntu"), tr("Unpack it into WSL"), tr("Install Kiri's engine in it"), tr("Start it")]));
                let file = download(&UBUNTU, 0)?;
                update(|s| s.progress = None);
                task(1, "active", "≈ 1.6 GB");
                // A real Ubuntu, in Start too; older WSL lacks --from-file.
                let installed = quiet("wsl.exe").args(["--install", "--from-file"]).arg(&file).args(["--name", UBUNTU_DISTRO, "--no-launch"]).status().is_ok_and(|s| s.success());
                if !installed {
                    wsl_ok(quiet("wsl.exe").args(["--import", UBUNTU_DISTRO]).arg(base.join(UBUNTU_DISTRO)).arg(&file).args(["--version", "2"]))?;
                }
                task(1, "done", "");
                update(|s| {
                    s.tasks.remove(0);
                });
                UBUNTU_DISTRO.to_owned()
            }
        };
        task(1, "active", "");
        // The engine binaries install.sh copies are checked too.
        verified("kiri")?;
        verified("kiri-init")?;
        let install = to_wsl(&distro, &verified("install.sh")?)?;
        wsl_ok(quiet("wsl.exe").args(["-d", &distro, "-u", "root", "--", "sh", &install]))?;
        task(1, "done", "");
        task(2, "done", "");
        distro
    };
    let mut s = saved();
    s.linux = Some(kind.to_owned());
    s.distro = Some(distro.clone());
    save(&s);
    crate::serve::set_distro(&distro);
    update(|st| st.linux = Some(kind.to_owned()));
    engine()
}

/// A Windows path as `distro` sees it.
pub(crate) fn to_wsl(distro: &str, path: &Path) -> io::Result<String> {
    let out = quiet("wsl.exe").args(["-d", distro, "--", "wslpath", "-a"]).arg(path.to_string_lossy().replace('\\', "/")).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn wsl_ok(cmd: &mut Command) -> io::Result<()> {
    let out = cmd.output()?;
    if out.status.success() {
        return Ok(());
    }
    let text = wsl_text(&out.stdout) + &wsl_text(&out.stderr);
    Err(io::Error::other(text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("WSL failed").to_owned()))
}

fn docker() -> PathBuf {
    let ours = dir().join("bin").join("docker.exe");
    if ours.is_file() { ours } else { bundle().join("bin").join("docker.exe") }
}

/// Step 3: the engine answers through the pipe and runs a container.
pub(super) fn engine() -> io::Result<()> {
    update(|s| {
        s.step = "engine";
        s.progress = None;
        s.tasks = tasks(&[tr("Start the engine"), tr("Connect Windows to it"), tr("Run a test container")]);
    });
    task(0, "active", "");
    let t0 = Instant::now();
    // Started here, not only waited for: setup resumed after Kiri or
    // Windows restarted finds Kiri's Linux there but its engine not running,
    // and nothing else starts it while the window is on setup.
    if !crate::client::alive()
        && let Err(e) = crate::engine::ensure_daemon()
    {
        crate::serve::log(&format!("setup engine: {e}; still waiting for it"));
    }
    while !crate::client::alive() {
        if CANCEL.load(Ordering::SeqCst) {
            return Err(io::Error::other("cancelled"));
        }
        if t0.elapsed() > Duration::from_secs(60) {
            return Err(io::Error::other("Kiri's engine did not answer within a minute"));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    task(0, "done", &trf("answered in {ms} ms", &[("ms", &t0.elapsed().as_millis())]));
    let pipe = crate::client::pipe();
    task(1, "done", &pipe);
    // Windows' proxy before the first pull, through the relay when it is on
    // Windows' loopback: the minute's refresh may not have come yet.
    crate::engine::send_windows_proxy();
    task(2, "active", &tr("pulling alpine"));
    log_line("$ docker run --rm alpine echo hello");
    // Before the installed copy exists, this runs the bundle's docker.exe:
    // check it first, as the tools step does for the copy it installs.
    if !dir().join("bin").join("docker.exe").is_file() {
        verified("bin/docker.exe")?;
    }
    let out = quiet(&docker().to_string_lossy()).args(["--host", &format!("npipe://{}", pipe.replace('\\', "/")), "run", "--rm", "alpine", "echo", "hello"]).output()?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    for l in text.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev() {
        log_line(l);
    }
    if !out.status.success() || !text.contains("hello") {
        // All of docker's words in the log, where the setup page shows a few.
        for l in text.lines().map(str::trim).filter(|l| !l.is_empty()).take(20) {
            crate::serve::log(&format!("setup engine: docker: {l}"));
        }
        return Err(io::Error::other(test_failure(&text)));
    }
    task(2, "done", "hello");
    tools()
}

/// Why `docker run alpine` failed, from what docker printed: the engine's
/// own words, and whether it was the pull.
pub(super) fn test_failure(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let said = lines
        .iter()
        .find_map(|l| l.split_once("Error response from daemon:").map(|(_, r)| r))
        .or_else(|| lines.iter().find_map(|l| l.strip_prefix("docker: ")))
        .or_else(|| lines.iter().rev().find(|l| !l.starts_with("See '") && !l.starts_with("Run '")).copied())
        .map(|r| r.trim().trim_end_matches('.').to_owned());
    let pulling = text.contains("Unable to find image") && !text.contains("Status: Downloaded");
    match (said, pulling) {
        (Some(why), true) => trf("couldn't pull alpine: {why}", &[("why", &why)]),
        (Some(why), false) => trf("the test container did not run: {why}", &[("why", &why)]),
        (None, _) => tr("the test container did not run"),
    }
}

/// Step 4: the Docker tools (setup-tools.ps1), task by task.
pub(super) fn tools() -> io::Result<()> {
    update(|s| {
        s.step = "tools";
        s.progress = None;
        s.tasks = tasks(&["docker 29.9.2".to_owned(), "Compose v5.6.1".to_owned(), "buildx v0.38.1".to_owned(), tr("docker build on Kiri"), tr("Make Kiri the default")]);
    });
    task(0, "active", "");
    for f in ["bin/docker.exe", "bin/kiri.exe", "bin/cli-plugins/docker-compose.exe", "bin/cli-plugins/docker-buildx.exe"] {
        verified(f)?;
    }
    let make_default = STATE.lock().as_ref().is_some_and(|s| s.make_default);
    // setup-tools.ps1 (contrib/windows), checked like the bundle's other
    // files and run from its file with its parameters as arguments. Not a
    // copy in this binary run through [scriptblock]::Create from Base64:
    // that is the textbook dropper, and Defender's heuristics flagged
    // kiri-win.exe 0.1.7 for it (Trojan:Win32/Bearfoos.A!ml).
    let script = verified("setup-tools.ps1")?;
    let mut cmd = quiet("powershell.exe");
    cmd.args(crate::win::PS_FILE).arg(&script).arg("-Bundle").arg(bundle());
    if make_default {
        cmd.arg("-Default");
    }
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("no output"))?;
    // stderr is drained as it comes: docker writes progress there, and a
    // full pipe would stall the script. It is only a fallback for the error.
    let stderr = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            s
        })
    });
    // The script reports its own failure as "ERROR <why>" on stdout, so
    // the message is the real one, not whatever docker said on stderr first.
    let mut error = None;
    for line in io::BufRead::lines(io::BufReader::new(stdout)).map_while(Result::ok) {
        let line = line.trim();
        if let Some(n) = line.strip_prefix("TASK ").and_then(|n| n.parse::<usize>().ok()) {
            let detail = if n == 4 && !make_default { tr("skipped") } else { String::new() };
            task(n, "done", &detail);
            task(n + 1, "active", "");
        } else if let Some(why) = line.strip_prefix("ERROR ") {
            error = Some(why.trim().to_owned());
        }
    }
    let status = child.wait()?;
    let err = stderr.and_then(|t| t.join().ok()).unwrap_or_default();
    if !status.success() {
        let why = error
            .filter(|e| !e.is_empty())
            .or_else(|| err.lines().rev().find(|l| !l.trim().is_empty() && !l.contains("Successfully")).map(|l| l.trim().to_owned()))
            .unwrap_or_else(|| "installing the Docker tools failed".to_owned());
        return Err(io::Error::other(why));
    }
    // From here on Kiri runs from %LOCALAPPDATA%\Kiri, not the download folder.
    if let Ok(exe) = std::env::current_exe() {
        let installed = dir().join("kiri-win.exe");
        if exe != installed {
            std::fs::copy(&exe, &installed).map_err(|e| io::Error::other(format!("could not install kiri-win.exe to {}: {e}", installed.display())))?;
        }
    }
    update(|s| {
        s.step = "ready";
        s.progress = None;
    });
    Ok(())
}

/// Ready → Open Kiri: remember setup is done, start at sign-in if asked.
pub(super) fn finish() {
    let autostart = STATE.lock().as_ref().is_none_or(|s| s.autostart);
    // Which builder docker build uses: asked with another engine here, Kiri's without.
    let asked = STATE.lock().as_ref().filter(|s| s.other_docker).map(|s| s.build_kiri);
    let built = match asked {
        Some(kiri) => crate::buildwith::choose(if kiri { crate::buildwith::With::Kiri } else { crate::buildwith::With::Docker }),
        None => crate::buildwith::apply(),
    };
    if let Err(e) = built {
        crate::serve::log(&format!("docker build: could not set which builder it uses ({e})"));
    }
    let mut s = saved();
    s.done = true;
    s.restart_after = None;
    s.last_error = None;
    save(&s);
    crate::serve::set_setup_pending(false);
    let exe = dir().join("kiri-win.exe");
    let run_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    if autostart {
        if exe.is_file() {
            let _ = quiet("reg.exe").args(["add", run_key, "/v", "Kiri", "/t", "REG_SZ", "/d", &format!("\"{}\" --background", exe.display()), "/f"]).status();
        } else {
            crate::serve::log("not set to start at sign-in: kiri-win.exe is not installed");
        }
    } else {
        let _ = quiet("reg.exe").args(["delete", run_key, "/v", "Kiri", "/f"]).status();
    }
    let _ = quiet("reg.exe").args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\RunOnce", "/v", "KiriSetup", "/f"]).status();
    Handle.run(|ui| ui.navigate_main());
}

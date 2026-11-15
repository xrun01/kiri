//! First run on Windows: from a PC with no WSL to `docker run` working, in
//! the window (setup.html; the protocol is in setup-protocol.md). Steps:
//! WSL (downloaded with progress, installed after one administrator
//! approval, then a restart), a Linux for Kiri (Kiri Linux, from the
//! bundle; or Ubuntu, downloaded), the engine, the Docker tools, ready.
//! Progress lives in %LOCALAPPDATA%\Kiri\setup.json, so a restart resumes.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use parking_lot::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use crate::app::Handle;
mod download;
mod steps;

pub(crate) use download::sha256;
pub(crate) use steps::to_wsl;
use steps::{engine, finish, install_linux, install_wsl, tools};

/// A file setup downloads: what it is, from where, and what it must hash to.
#[derive(Debug)]
pub(super) struct Pin {
    pub(super) name: &'static str,
    pub(super) url: &'static str,
    pub(super) sha256: &'static str,
    pub(super) size: u64,
    pub(super) file: &'static str,
}

/// GitHub's asset digest for the release. WSL ships one MSI per
/// architecture; this build's own (see crate::win::arch_mismatch).
#[cfg(not(target_arch = "aarch64"))]
pub(super) const WSL_MSI: Pin = Pin {
    name: "WSL 3.0.3",
    url: "https://github.com/microsoft/WSL/releases/download/3.0.3/wsl.3.0.3.0.x64.msi",
    sha256: "ae977286974255bd26bf925f00f40f5692c6b2084155cbc0563c09aea091f211",
    size: 367_575_040,
    file: "wsl.3.0.3.0.x64.msi",
};
#[cfg(target_arch = "aarch64")]
pub(super) const WSL_MSI: Pin = Pin {
    name: "WSL 3.0.3",
    url: "https://github.com/microsoft/WSL/releases/download/3.0.3/wsl.3.0.3.0.arm64.msi",
    sha256: "2937c7ce840bb35ec6e97f052189bdfff27f8c3463bfdd231df9a958212787a3",
    size: 341_086_208,
    file: "wsl.3.0.3.0.arm64.msi",
};

/// Microsoft's WSL distribution list gives the hash (DistributionInfo.json
/// in microsoft/WSL: Amd64Url and Arm64Url).
#[cfg(not(target_arch = "aarch64"))]
pub(super) const UBUNTU: Pin = Pin {
    name: "Ubuntu 26.04",
    url: "https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-wsl-amd64.wsl",
    sha256: "48d56724b5c8e60f24893e83e73bbb58c60b3ca22fba3da977075420acd54104",
    size: 418_495_746,
    file: "ubuntu-26.04.1-wsl-amd64.wsl",
};
#[cfg(target_arch = "aarch64")]
pub(super) const UBUNTU: Pin = Pin {
    name: "Ubuntu 26.04",
    url: "https://cdimages.ubuntu.com/releases/26.04.1/release/ubuntu-26.04.1-wsl-arm64.wsl",
    sha256: "c7d893d7ccb291009e6096c5a946937337b6fd4c426f11b6541f6cd2bf894f7d",
    size: 412_840_386,
    file: "ubuntu-26.04.1-wsl-arm64.wsl",
};
pub(super) const UBUNTU_DISTRO: &str = "Ubuntu-26.04";
pub(crate) const KIRI_DISTRO: &str = "kiri";

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(super) struct Saved {
    pub(super) done: bool,
    /// "kiri" or "ubuntu".
    pub(super) linux: Option<String>,
    pub(super) distro: Option<String>,
    /// Unix seconds of the WSL install that needs a restart.
    pub(super) restart_after: Option<u64>,
    /// Why setup last stopped, kept so a restart of Kiri doesn't lose it.
    pub(super) last_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Task {
    pub(super) label: String,
    pub(super) state: &'static str,
    pub(super) detail: String,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct State {
    pub(super) step: &'static str,
    pub(super) linux: Option<String>,
    sizes: Value,
    existing_ubuntu: bool,
    pub(super) other_docker: bool,
    /// The engine besides Kiri, by name (crate::buildwith), for the build question.
    other_engine: Option<&'static str>,
    pub(super) make_default: bool,
    /// Builds on Kiri: the answer to "Keep building with Docker / Build
    /// with Kiri", asked only with another engine here.
    pub(super) build_kiri: bool,
    pub(super) autostart: bool,
    /// A first start: the page asks for the language before anything else.
    ask_lang: bool,
    pub(super) progress: Option<Value>,
    pub(super) tasks: Vec<Task>,
    log: Vec<String>,
    error: Option<String>,
}

pub(super) static STATE: Mutex<Option<State>> = Mutex::new(None);
static BUSY: AtomicBool = AtomicBool::new(false);
pub(super) static CANCEL: AtomicBool = AtomicBool::new(false);

pub(super) fn dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri")).unwrap_or_else(|| PathBuf::from("."))
}

pub(super) fn saved() -> Saved {
    std::fs::read(dir().join("setup.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The WSL distro setup put the engine in, if it has.
pub(crate) fn saved_distro() -> Option<String> {
    saved().distro
}

pub(super) fn save(s: &Saved) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(dir().join("setup.json"), serde_json::to_vec_pretty(s).unwrap_or_default());
}

/// The distro setup put Kiri in, for the bridge.
pub fn distro() -> Option<String> {
    saved().distro
}

/// The SHA-256 of each bundle file, embedded at build time by
/// `cargo xtask windows` (`name=hash;…`). A dev build has none.
const BUNDLE_SHA256: Option<&str> = option_env!("KIRI_BUNDLE_SHA256");

/// A file from the bundle, checked against its embedded SHA-256 so a file
/// swapped in the unzipped folder (a drive-by download, another process)
/// is refused rather than imported into WSL or installed on PATH. With no
/// embedded hashes (a dev build) the file is used as is.
pub(crate) fn verified(rel: &str) -> io::Result<PathBuf> {
    let path = bundle().join(rel);
    let Some(want) = BUNDLE_SHA256.and_then(|m| m.split(';').find_map(|e| e.strip_prefix(rel).and_then(|r| r.strip_prefix('=')))) else {
        // A debug build (plain `cargo build`) has no manifest and uses the
        // file as is. A release must have one: otherwise building kiri-win on
        // its own would quietly skip every bundle check.
        if cfg!(debug_assertions) {
            return Ok(path);
        }
        return Err(io::Error::other(format!("this build carries no checksum for {rel}; build the bundle with `cargo xtask windows`")));
    };
    if !path.is_file() {
        return Err(io::Error::other(format!("{rel} is missing from Kiri's folder")));
    }
    let got = sha256(&path)?;
    if got != want {
        return Err(io::Error::other(format!("{rel} in Kiri's folder does not match this build (SHA-256 {got}); download Kiri again")));
    }
    Ok(path)
}

/// Where kiri-win was started from: the unzipped bundle on first run. Debug
/// builds run from target/debug use the bundle made by `cargo xtask windows-dev`.
pub(super) fn bundle() -> PathBuf {
    let local = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_default();
    #[cfg(debug_assertions)]
    if !local.join("kiri-linux.tar.gz").is_file() {
        let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../target/kiri-windows-{}", std::env::consts::ARCH));
        if bundled.join("kiri-linux.tar.gz").is_file() {
            return bundled;
        }
    }
    local
}

pub(super) fn quiet(program: &str) -> Command {
    let mut c = crate::win::tool(program);
    std::os::windows::process::CommandExt::creation_flags(&mut c, CREATE_NO_WINDOW);
    c.stdin(Stdio::null());
    c
}

/// WSL's own output is UTF-16.
pub(super) fn wsl_text(bytes: &[u8]) -> String {
    if bytes.len() >= 2 && bytes.iter().skip(1).step_by(2).take(8).all(|&b| b == 0) {
        let units: Vec<u16> = bytes.as_chunks::<2>().0.iter().copied().map(u16::from_le_bytes).collect();
        String::from_utf16_lossy(&units).replace('\0', "")
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

pub(super) fn wsl_installed() -> bool {
    quiet("wsl.exe").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

pub(crate) fn distros() -> Vec<String> {
    quiet("wsl.exe").args(["-l", "-q"]).output().map(|o| wsl_text(&o.stdout).lines().map(|l| l.trim().to_owned()).filter(|l| !l.is_empty()).collect()).unwrap_or_default()
}

pub(super) fn existing_ubuntu() -> Option<String> {
    distros().into_iter().find(|d| d.starts_with("Ubuntu"))
}

/// Another Docker on this PC: Docker Desktop, or a docker CLI on PATH that is not Kiri's.
fn other_docker() -> bool {
    let desktop = std::env::var_os("ProgramFiles").is_some_and(|p| Path::new(&p).join(r"Docker\Docker\Docker Desktop.exe").is_file());
    let ours = dir().join("bin");
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d != ours && d.join("docker.exe").is_file()));
    desktop || on_path
}

pub(super) fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Whether Windows has restarted since `at`.
fn restarted_since(at: u64) -> bool {
    // SAFETY: reads the system's uptime.
    let uptime = unsafe { windows_sys::Win32::System::SystemInformation::GetTickCount64() } / 1000;
    now_secs().saturating_sub(uptime) > at
}

/// Whether the window should open on setup rather than Kiri.
pub fn needed() -> bool {
    let s = saved();
    !s.done || s.distro.as_deref().is_none_or(|d| !distros().iter().any(|x| x == d))
}

/// The step to show now, from what is on this PC.
fn current_step(s: &Saved) -> &'static str {
    if !wsl_installed() {
        return "welcome";
    }
    if let Some(at) = s.restart_after
        && !restarted_since(at)
    {
        return "restart";
    }
    match &s.distro {
        Some(d) if distros().iter().any(|x| x == d) => {
            if crate::client::alive() {
                "tools"
            } else {
                "engine"
            }
        }
        _ => "choose",
    }
}

fn fresh_state(step: &'static str, s: &Saved) -> State {
    let other = other_docker();
    State {
        step,
        linux: s.linux.clone(),
        sizes: json!({"wsl": WSL_MSI.size, "ubuntu": UBUNTU.size, "kiri": std::fs::metadata(bundle().join("kiri-linux.tar.gz")).map_or(0, |m| m.len())}),
        existing_ubuntu: existing_ubuntu().is_some(),
        other_docker: other,
        other_engine: crate::buildwith::other_engine(),
        make_default: !other,
        build_kiri: !other,
        autostart: true,
        ask_lang: crate::engine::fresh() && crate::engine::prefs()["langAsked"] != true,
        progress: None,
        tasks: Vec::new(),
        log: Vec::new(),
        error: None,
    }
}

pub(super) fn update(f: impl FnOnce(&mut State)) {
    let json = {
        let mut st = STATE.lock();
        let Some(s) = st.as_mut() else { return };
        f(s);
        let mut v = serde_json::to_value(&*s).unwrap_or(Value::Null);
        v["type"] = json!("setup");
        v.to_string()
    };
    Handle.eval(format!("window.kiri && kiri.on({json})"));
}

pub(super) fn tasks(labels: &[String]) -> Vec<Task> {
    labels.iter().map(|l| Task { label: l.clone(), state: "todo", detail: String::new() }).collect()
}

pub(super) fn task(i: usize, state: &'static str, detail: &str) {
    update(|s| {
        if let Some(t) = s.tasks.get_mut(i) {
            t.state = state;
            t.detail = detail.to_owned();
        }
        // An unsized step's progress follows its tasks.
        let n = s.tasks.len().max(1);
        let done = s.tasks.iter().filter(|t| t.state == "done").count();
        if s.progress.as_ref().is_none_or(|p| p["total"].is_null()) {
            let label = s.tasks.iter().find(|t| t.state == "active").map_or(String::new(), |t| t.label.clone());
            s.progress = Some(json!({"pct": done * 100 / n, "done": null, "total": null, "rate": null, "eta": null, "label": label}));
        }
    });
}

pub(super) fn log_line(line: &str) {
    let line = line.trim().to_owned();
    if !line.is_empty() {
        update(|s| {
            s.log.push(line);
            let n = s.log.len();
            if n > 8 {
                s.log.drain(..n - 8);
            }
        });
    }
}

/// Runs a step's work on its own thread; an error stays on its screen with "Try again".
fn run(step: &'static str, work: impl FnOnce() -> io::Result<()> + Send + 'static) {
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    CANCEL.store(false, Ordering::SeqCst);
    update(|s| {
        s.step = step;
        s.error = None;
        s.progress = None;
        s.log.clear();
    });
    std::thread::spawn(move || {
        let r = work();
        BUSY.store(false, Ordering::SeqCst);
        if let Err(e) = r {
            crate::serve::log(&format!("setup {step}: {e}"));
            let cancelled = CANCEL.load(Ordering::SeqCst);
            if !cancelled {
                let mut saved = saved();
                saved.last_error = Some(e.to_string());
                save(&saved);
            }
            update(|s| {
                if cancelled {
                    s.step = if step == "wsl" { "welcome" } else if step == "linux" { "choose" } else { step };
                    s.progress = None;
                    s.tasks.clear();
                } else {
                    s.error = Some(e.to_string());
                    for t in &mut s.tasks {
                        if t.state == "active" {
                            t.state = "error";
                        }
                    }
                }
            });
        }
    });
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut c) = quiet("clip.exe").stdin(Stdio::piped()).spawn() {
        if let Some(mut i) = c.stdin.take() {
            let _ = io::Write::write_all(&mut i, text.as_bytes());
        }
        let _ = c.wait();
    }
}

/// Whether a page message is setup's.
pub fn handles(msg: &str) -> bool {
    serde_json::from_str::<Value>(msg).ok().and_then(|v| v["cmd"].as_str().map(|c| c.starts_with("setup-") || c == "copy")).unwrap_or(false)
}

pub fn on_message(msg: &str) {
    let Ok(v) = serde_json::from_str::<Value>(msg) else { return };
    match v["cmd"].as_str().unwrap_or("") {
        // This arrives on the window's thread. Working out the step asks
        // wsl.exe twice and pings the engine through the pipe, which on a
        // cold WSL can take long (and the ping has no deadline): off the
        // window's thread, so it keeps drawing and answering.
        "setup-ready" => {
            std::thread::spawn(|| {
                let s = saved();
                let step = current_step(&s);
                let mut fresh = fresh_state(step, &s);
                if let Some(e) = &s.last_error {
                    crate::serve::log(&format!("setup: carrying on at {step}; last time it stopped with: {e}"));
                    // A step that carries on by itself shows its own result;
                    // one waiting for a choice shows why setup stopped.
                    if matches!(step, "choose" | "welcome") {
                        fresh.error = Some(e.clone());
                    }
                }
                *STATE.lock() = Some(fresh);
                update(|_| {});
                // A restart finished WSL, or the engine is there already: carry on by itself.
                match step {
                    "engine" => run("engine", engine),
                    "tools" => run("tools", tools),
                    _ => {}
                }
            });
        }
        "setup-install-wsl" => run("wsl", install_wsl),
        "setup-restart" => {
            let _ = quiet("shutdown.exe").args(["/r", "/t", "5", "/c", "Kiri: finishing Windows Subsystem for Linux"]).status();
        }
        "setup-later" => Handle.run(crate::app::Ui::hide),
        "setup-choose" => {
            let kind = if v["value"] == "ubuntu" { "ubuntu" } else { "kiri" };
            update(|s| s.linux = Some(kind.to_owned()));
            run("linux", move || install_linux(kind));
        }
        "setup-switch" => {
            CANCEL.store(true, Ordering::SeqCst);
            std::thread::spawn(|| {
                while BUSY.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                update(|s| s.linux = Some("kiri".into()));
                run("linux", || install_linux("kiri"));
            });
        }
        "setup-cancel" => CANCEL.store(true, Ordering::SeqCst),
        "setup-retry" => {
            let step = STATE.lock().as_ref().map_or("welcome", |s| s.step);
            let linux = STATE.lock().as_ref().and_then(|s| s.linux.clone()).unwrap_or_else(|| "kiri".into());
            match step {
                "wsl" => run("wsl", install_wsl),
                "linux" => run("linux", move || install_linux(&linux)),
                "engine" => run("engine", engine),
                "tools" => run("tools", tools),
                _ => update(|s| s.error = None),
            }
        }
        "setup-default" => update(|s| s.make_default = v["value"] == true),
        "setup-autostart" => update(|s| s.autostart = v["value"] == true),
        "setup-build" => update(|s| s.build_kiri = v["value"] == "kiri"),
        "setup-open" => finish(),
        // SAFETY: ends this thread's message loop; run on the window's thread.
        "setup-quit" => Handle.run(|_| unsafe { windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0) }),
        "copy" => copy_to_clipboard(v["value"].as_str().unwrap_or("")),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_failed_test_container_says_why() {
        let pull = "Unable to find image 'alpine:latest' locally\ndocker: Error response from daemon: the proxy http://127.0.0.1:7892 refused the connection or closed it: is it running? (network error: io: Connection refused (os error 111)).\nSee 'docker run --help'.\n";
        assert_eq!(super::steps::test_failure(pull), "couldn't pull alpine: the proxy http://127.0.0.1:7892 refused the connection or closed it: is it running? (network error: io: Connection refused (os error 111))");
        let stream = "Unable to find image 'alpine:latest' locally\nlatest: Pulling from library/alpine\ndocker: network error: timeout: connect\n\nRun 'docker run --help' for more information\n";
        assert_eq!(super::steps::test_failure(stream), "couldn't pull alpine: network error: timeout: connect");
        let run = "Unable to find image 'alpine:latest' locally\nlatest: Pulling from library/alpine\nStatus: Downloaded newer image for alpine:latest\ndocker: Error response from daemon: no space left on device.\n";
        assert_eq!(super::steps::test_failure(run), "the test container did not run: no space left on device");
        assert_eq!(super::steps::test_failure("something odd\n"), "the test container did not run: something odd");
        assert_eq!(super::steps::test_failure(""), "the test container did not run");
    }
}

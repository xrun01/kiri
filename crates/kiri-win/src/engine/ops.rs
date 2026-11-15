use std::io;
use std::sync::LazyLock;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::app::{Handle, Menu};
use crate::client;

use super::prefs::{send_windows_proxy, tr, trf};
use super::pull::pull;
use super::{Cmd, RUNNING, brand, distro, emit, notice, send, sharing, state};

/// How often the VM panel's figures refresh. The lists refresh on events.
const TICK: Duration = Duration::from_secs(2);
/// The lists also come again this often without an event, so "Up 5 min" and
/// "3 min ago" keep counting.
const LIST_EVERY: Duration = Duration::from_secs(30);
/// When the lists last went to the page, by any route.
static LISTED: LazyLock<Mutex<Instant>> = LazyLock::new(|| Mutex::new(Instant::now()));


pub(super) fn engine(rx: &Receiver<Cmd>, tx: Sender<Cmd>) {
    watch(tx);
    // Windows' proxy for the daemon, now and every minute.
    std::thread::spawn(|| loop {
        send_windows_proxy();
        std::thread::sleep(Duration::from_secs(60));
    });
    let mut pending: Option<Cmd> = None;
    loop {
        let cmd = match pending.take() {
            Some(c) => Ok(c),
            None => rx.recv_timeout(TICK),
        };
        match cmd {
            Ok(Cmd::Start) => {
                emit(state("booting", None));
                match ensure_daemon().and_then(|()| wake()) {
                    Ok(()) => {
                        std::thread::spawn(ensure_builder);
                        refresh();
                    }
                    Err(e) => emit(state("stopped", Some(&e.to_string()))),
                }
            }
            // Stop means off: every container stopped, the engine off until
            // Start (nothing wakes it meanwhile), and this window's shares closed.
            Ok(Cmd::Stop) => {
                emit(state("stopping", None));
                sharing::close_all();
                let stopped = match expect_ok(client::request("POST", "/kiri/stop", None)) {
                    // An engine older than Stop: its sleep.
                    Err(e) if e.to_string().contains("404") => expect_ok(client::request("POST", "/kiri/sleep", None)),
                    r => r,
                };
                if let Err(e) = stopped {
                    emit(notice("error", &e.to_string()));
                }
                refresh();
            }
            Ok(Cmd::Refresh) => {
                // Events come in bursts (create, start, …): one refresh per
                // burst. Anything else that arrives meanwhile runs next.
                loop {
                    match rx.recv_timeout(Duration::from_millis(60)) {
                        Ok(Cmd::Refresh) => {}
                        Ok(other) => {
                            pending = Some(other);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                refresh();
            }
            Ok(Cmd::Pull(name)) => pull(name),
            Ok(Cmd::Run(name, env)) => {
                // The page shows its spinner until this says how it went.
                let result = run_image(&name, &env);
                emit(json!({"type": "ran", "name": name, "error": result.is_err()}).to_string());
                act(result)
            }
            Ok(Cmd::RemoveImage(name)) => match client::request("DELETE", &format!("/images/{}", path_escape(&name)), None) {
                Ok(r) if r.ok() => {
                    // Only untagged when another name still points at it: nothing freed.
                    let deleted = r.json().as_array().is_some_and(|a| a.iter().any(|v| v.get("Deleted").is_some()));
                    emit(json!({"type": "removed", "name": name, "deleted": deleted}).to_string());
                }
                Ok(r) => emit(notice("error", &r.error())),
                Err(e) => emit(notice("error", &e.to_string())),
            },
            Ok(Cmd::Container { action, id }) => {
                let (method, path) = match action.as_str() {
                    "remove" => ("DELETE", format!("/containers/{id}?force=1&v=1")),
                    a @ ("start" | "stop" | "restart" | "pause" | "unpause") => ("POST", format!("/containers/{id}/{a}")),
                    _ => continue,
                };
                let result = expect_ok(client::request(method, &path, None));
                // Refused to start (a port taken): the container's page says why.
                if let (Err(e), "start") = (&result, action.as_str()) {
                    emit(json!({"type": "startfailed", "id": id, "error": e.to_string()}).to_string());
                }
                act(result);
            }
            Ok(Cmd::RemoveVolume(name)) => act(expect_ok(client::request("DELETE", &format!("/volumes/{}", path_escape(&name)), None))),
            Ok(Cmd::RemoveNetwork(id)) => act(expect_ok(client::request("DELETE", &format!("/networks/{id}"), None))),
            Ok(Cmd::Ports { id, ports }) => act(set_ports(&id, ports)),
            Ok(Cmd::Env { id, env }) => act(set_env(&id, env)),
            Ok(Cmd::About) => about(),
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {
                vm_panel();
                if LISTED.lock().elapsed() >= LIST_EVERY {
                    refresh();
                }
            }
        }
    }
}

/// Follows the daemon's event stream on its own thread; each event asks
/// the engine for a refresh. Reconnects when the daemon comes back.
fn watch(tx: Sender<Cmd>) {
    let _ = std::thread::Builder::new().name("kiri-ui-events".into()).spawn(move || loop {
        if client::alive() {
            if tx.send(Cmd::Refresh).is_err() {
                return;
            }
            // A Kubernetes node already running gets its kubectl context.
            crate::k8s::sync_soon();
            let _ = client::events(|_| tx.send(Cmd::Refresh).is_ok());
        }
        std::thread::sleep(Duration::from_secs(1));
    });
}

pub fn expect_ok(r: io::Result<client::Response>) -> io::Result<()> {
    let r = r?;
    if r.ok() { Ok(()) } else { Err(io::Error::other(r.error())) }
}

fn act(result: io::Result<()>) {
    if let Err(e) = result {
        emit(notice("error", &e.to_string()));
    }
}

pub(crate) fn path_escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~:/@".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// The daemon's wake; a Linux host has no VM to wake, which is fine.
fn wake() -> io::Result<()> {
    let r = client::request("POST", "/kiri/wake", None)?;
    if r.ok() || r.status == 404 { Ok(()) } else { Err(io::Error::other(r.error())) }
}

/// Makes sure the daemon answers: Ubuntu uses the `kiri` systemd service;
/// the small Kiri Linux distro starts /usr/local/bin/kiri-start instead.
pub(crate) fn ensure_daemon() -> io::Result<()> {
    if client::alive() {
        return Ok(());
    }
    crate::serve::log("engine: not answering; starting it in WSL");
    // The engine this app installed, not one an update left behind.
    match crate::update::sync_engine() {
        Ok(true) => crate::serve::log("engine: copied this version's engine into WSL"),
        Ok(false) => {}
        Err(e) => crate::serve::log(&format!("engine: could not copy the engine into WSL ({e}); starting the one there")),
    }
    let distro = distro();
    let mut cmd = crate::win::tool("wsl.exe");
    if let Some(d) = &distro {
        cmd.args(["-d", d]);
    }
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let started = cmd.args(["-u", "root", "--", "systemctl", "start", "kiri"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    if !started.is_ok_and(|s| s.success()) {
        let mut cmd = crate::win::tool("wsl.exe");
        if let Some(d) = &distro {
            cmd.args(["-d", d]);
        }
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let _ = cmd.args(["-u", "root", "--", "/usr/local/bin/kiri-start"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    for _ in 0..100 {
        if client::alive() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(io::Error::other("Kiri's engine did not start in WSL: run contrib\\windows\\kiri.ps1 to install it"))
}

/// `docker build` through Kiri, as on the Mac: BuildKit in a Kiri container
/// (buildx's docker-container driver), loading what it builds into Kiri.
/// That container is the daemon's managed builder, made when buildx first
/// looks for it and stopped when idle (crates/kiri/src/daemon/builder.rs);
/// without buildx the CLI's classic POST /build uses the same BuildKit.
/// kiri.ps1 makes it the builder `docker build` uses (BUILDX_BUILDER) when
/// Kiri is the only engine; beside Docker Desktop, `buildx use` makes it the
/// one for the `kiri` context, and `buildx install` makes plain `docker
/// build` use it, as on the Mac (crates/kiri/src/setup.rs `builder`), or
/// it builds with buildx's fallback for the context, which never loads the
/// image into Kiri. Needs the `kiri` context, which kiri.ps1 makes.
fn ensure_builder() {
    const BUILDER: &str = "kiri-build";
    let docker = crate::detail::docker_cli();
    let run = |args: &[&str]| {
        let mut cmd = crate::win::tool(&docker);
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        cmd.args(args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    };
    if !run(&["context", "inspect", "kiri"]) || !run(&["buildx", "version"]) {
        return;
    }
    if !run(&["buildx", "inspect", BUILDER]) && !run(&["buildx", "create", "--name", BUILDER, "--driver", "docker-container", "--driver-opt", "default-load=true", "kiri"]) {
        crate::serve::log("docker buildx create failed");
        return;
    }
    // `--default`: kept per context, so it stays the builder for `kiri`
    // whatever another context selects later.
    if !run(&["--context", "kiri", "buildx", "use", "--default", BUILDER]) || !run(&["buildx", "install"]) {
        crate::serve::log("docker buildx use or install failed");
    }
}

/// Sends the page every list, from the daemon.
fn refresh() {
    *LISTED.lock() = Instant::now();
    let lists = (|| -> io::Result<Value> {
        let containers = client::get("/containers/json?all=1")?;
        let images = client::get("/images/json")?;
        let volumes = client::get("/volumes")?;
        let networks = client::get("/networks")?;
        let add_icon = |mut v: Value, name_of: &dyn Fn(&Value) -> String| {
            if let Some(list) = v.as_array_mut() {
                for item in list {
                    item["icon"] = json!(brand(&name_of(item)));
                }
            }
            v
        };
        let containers = add_icon(containers, &|c| c["Image"].as_str().unwrap_or_default().to_owned());
        let images = add_icon(images, &|i| i["RepoTags"][0].as_str().unwrap_or_default().to_owned());
        Ok(json!({"type": "lists", "containers": containers, "images": images, "volumes": volumes["Volumes"], "networks": networks}))
    })();
    match lists {
        Ok(v) => {
            *RUNNING.lock() = v["containers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["State"] == "running")
                .map(|c| (c["Id"].as_str().unwrap_or("").to_owned(), c["Names"][0].as_str().unwrap_or("").trim_start_matches('/').to_owned()))
                .collect();
            emit(v.to_string());
            vm_panel();
        }
        Err(_) => {
            RUNNING.lock().clear();
            Handle.set_menu(Menu { vm: "Not running".into(), running: Vec::new() });
            emit(state("stopped", None));
        }
    }
}

/// The sidebar's engine panel and the tray menu's state.
fn vm_panel() {
    match client::get("/info") {
        Ok(info) => {
            // A Linux host has no VM to put to sleep: it is always awake.
            let awake = info["KiriVmAwake"].as_bool().unwrap_or(true);
            let mib = |k: &str| info[k].as_u64().unwrap_or(0) >> 20;
            let target = if info["KiriMemoryTarget"].is_null() { info["MemTotal"].as_u64().unwrap_or(0) >> 20 } else { mib("KiriMemoryTarget") };
            let ceiling = if info["KiriMemoryCeiling"].is_null() { info["MemTotal"].as_u64().unwrap_or(0) >> 20 } else { mib("KiriMemoryCeiling") };
            emit(json!({
                "type": "vm", "awake": awake, "target_mib": target, "ceiling_mib": ceiling,
                // Missing from an engine older than the field: 0, not NaN on the page.
                "used_mib": mib("KiriMemoryUsed"),
                "running": info["ContainersRunning"], "cpus": info["NCPU"],
                "daemon_kib": info["KiriAgentMemory"].as_u64().unwrap_or(0) >> 10,
            })
            .to_string());
            // Stopped (Stop in the window): off until Start, unlike asleep.
            let stopped = info["KiriStopped"] == true;
            let awake = awake && !stopped;
            emit(state(if awake { "running" } else if stopped { "stopped" } else { "asleep" }, None));
            let running = RUNNING.lock().clone();
            Handle.set_menu(Menu { vm: if awake { "Running" } else if stopped { "Stopped" } else { "Asleep" }.into(), running });
        }
        Err(_) => {
            Handle.set_menu(Menu { vm: "Not running".into(), running: Vec::new() });
            emit(state("stopped", None));
        }
    }
}

/// Where a published port answers: `localhost:<port>`, the handy URL, noting
/// that the bind is every interface (the default, as Docker's), or else the
/// address it was bound to (`-p 127.0.0.1:8080:80`).
fn published(b: &Value) -> Option<String> {
    let p = b["HostPort"].as_str()?;
    Some(match b["HostIp"].as_str().unwrap_or("") {
        "" | "0.0.0.0" | "::" => format!("localhost:{p} (all interfaces)"),
        ip => format!("{ip}:{p}"),
    })
}

fn host_ports(info: &Value, with_inner: bool) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(map) = info["NetworkSettings"]["Ports"].as_object() {
        for (inner, list) in map {
            for b in list.as_array().into_iter().flatten() {
                if let Some(at) = published(b) {
                    out.push(if with_inner { format!("{at} → {inner}") } else { at });
                }
            }
        }
    }
    out
}

fn set_ports(id: &str, ports: Value) -> io::Result<()> {
    expect_ok(client::request("POST", &format!("/kiri/containers/{id}/ports"), Some(&json!({"ports": ports}))))?;
    let now = host_ports(&client::get(&format!("/containers/{id}/json"))?, true);
    emit(notice("ok", &if now.is_empty() { tr("Ports removed") } else { trf("Ports saved: {ports}", &[("ports", &now.join(", "))]) }));
    Ok(())
}

/// A container's environment, as the Mac's: the engine restarts it with the
/// new one if it was running, or starts it with it if it had stopped.
fn set_env(id: &str, env: Vec<String>) -> io::Result<()> {
    let r = client::request("POST", &format!("/kiri/containers/{id}/env"), Some(&json!({"env": env})))?;
    if r.status == 404 && !r.error().contains("No such container") {
        return Err(io::Error::other("This engine can't change a container's environment yet: update Kiri's engine in WSL and try again."));
    }
    expect_ok(Ok(r))?;
    let info = client::get(&format!("/containers/{id}/json"))?;
    let name = info["Name"].as_str().unwrap_or_default().trim_start_matches('/');
    let text = if info["State"]["Running"].as_bool() == Some(true) { trf("{name} restarted with the new environment", &[("name", &name)]) } else { trf("{name} has the new environment", &[("name", &name)]) };
    emit(notice("ok", &text));
    emit(json!({"type": "inspect", "id": id, "info": info}).to_string());
    Ok(())
}

fn run_image(image: &str, env: &[String]) -> io::Result<()> {
    let body = json!({"Image": image, "Tty": true, "OpenStdin": true, "Env": env, "HostConfig": {"PublishAllPorts": true}});
    let began = std::time::Instant::now();
    let created = client::request("POST", "/containers/create", Some(&body))?;
    if !created.ok() {
        return Err(io::Error::other(created.error()));
    }
    let id = created.json()["Id"].as_str().unwrap_or_default().to_owned();
    expect_ok(client::request("POST", &format!("/containers/{id}/start"), None))?;
    let ms = began.elapsed().as_millis() as u64;
    let info = client::get(&format!("/containers/{id}/json"))?;
    let name = info["Name"].as_str().unwrap_or_default().trim_start_matches('/').to_owned();
    let ports = host_ports(&info, false);
    // View on the toast opens the container's page; the list stays put, as on
    // the Mac, and `image` puts it beside that image's Run button.
    let text = if ports.is_empty() { trf("Started {name}", &[("name", &name)]) } else { trf("Started {name} on {ports}", &[("name", &name), ("ports", &ports.join(", "))]) };
    emit(json!({"type": "notice", "kind": "ok", "text": text, "open": id, "image": image, "ms": ms}).to_string());
    Ok(())
}

fn about() {
    let info = client::get("/info").unwrap_or(Value::Null);
    let version = client::get("/version").unwrap_or(Value::Null);
    emit(
        json!({
            "type": "about", "app": env!("CARGO_PKG_VERSION"), "running": !info.is_null(),
            "engine": client::kiri_version(&version), "api": version["ApiVersion"], "kernel": info["KernelVersion"],
            "cpus": info["NCPU"], "memory": info["MemTotal"], "awake": info["KiriVmAwake"].as_bool().unwrap_or(true), "os": info["OperatingSystem"],
        })
        .to_string(),
    );
}

/// The status bar's left side: this build, and which engine `docker` talks
/// to by default, as `docker context show` says (it knows DOCKER_CONTEXT,
/// DOCKER_HOST and wherever Docker Desktop keeps the CLI config).
pub(super) fn status_event() -> String {
    let context = docker(&["context", "show"]).ok().map(|s| s.trim().to_owned()).filter(|c| !c.is_empty());
    json!({"type": "status", "version": env!("CARGO_PKG_VERSION"), "share": true, "app": "Kiri", "ours": "kiri", "context": context.unwrap_or_else(|| "default".into())}).to_string()
}

/// Runs the docker CLI without a console window: its output, or why it failed.
fn docker(args: &[&str]) -> io::Result<String> {
    let mut cmd = crate::win::tool(&crate::detail::docker_cli());
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(args).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_owned()))
    }
}

pub(super) fn builds_event() -> String {
    match crate::builds::list() {
        Ok(b) => json!({"type": "builds", "builds": b}),
        Err(e) => json!({"type": "builds", "error": e.to_string()}),
    }
    .to_string()
}

/// Which builder docker build uses, as chosen (crate::buildwith). An older
/// Kiri named its builder in the user's BUILDX_BUILDER, which sent every
/// build to Kiri whatever docker used: that goes, as Kiri's.
pub(super) fn build_with() {
    // The user's own value (HKCU\Environment) goes only if it is still
    // Kiri's. By reg.exe, not a PowerShell -Command: kiri-win runs no
    // script text it carries (crate::win says why).
    if std::env::var("BUILDX_BUILDER").is_ok_and(|b| b == crate::builds::BUILDER.get()) {
        let reg = |args: &[&str]| {
            let mut cmd = crate::win::tool("reg.exe");
            std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
            cmd.args(args).output()
        };
        let ours = reg(&["query", r"HKCU\Environment", "/v", "BUILDX_BUILDER"]).is_ok_and(|o| {
            String::from_utf8_lossy(&o.stdout).lines().any(|l| {
                let w: Vec<&str> = l.split_whitespace().collect();
                w.len() == 3 && w[0].eq_ignore_ascii_case("BUILDX_BUILDER") && w[2] == crate::builds::BUILDER.get()
            })
        });
        if ours && reg(&["delete", r"HKCU\Environment", "/v", "BUILDX_BUILDER", "/f"]).is_ok_and(|o| o.status.success()) {
            crate::win::environment_changed();
        }
    }
    if let Err(e) = crate::buildwith::apply() {
        crate::serve::log(&format!("docker build: could not set which builder it uses ({e})"));
    }
}

/// Which engine plain `docker` uses: Kiri, or back to the one before.
pub(super) fn use_context(name: &str) {
    let result = if name.is_empty() { Err(io::Error::other("no context named")) } else { docker(&["context", "use", name]).map(drop) };
    emit(status_event());
    emit(json!({"type": "usedcontext", "name": name, "error": result.as_ref().err().map(|e| e.to_string())}).to_string());
}

/// Which names containers answer at: <name>.kiri.local once its DNS rule
/// is in (crate::dns), else <name>.kiri.localhost, which Windows' browsers
/// resolve to this machine with no setup. HTTPS with Kiri's certificate is
/// the Mac's only.
pub(super) fn domains_event() -> String {
    // The rule alone is not enough: nothing answers if port 53 was taken.
    let on = crate::dns::installed() && crate::dns::serving();
    json!({"type": "domains", "on": on, "suffix": if on { "kiri.local" } else { "kiri.localhost" }, "https": false}).to_string()
}

/// Settings → Domains, or the welcome: adds or removes the DNS rule.
pub(super) fn domains(on: bool) {
    let _ = std::thread::Builder::new().name("kiri-domains".into()).spawn(move || {
        if on && !crate::dns::serving() {
            emit(notice("error", &tr("Another program holds 127.0.0.1:53, so Kiri cannot answer for <name>.kiri.local; containers stay at <name>.kiri.localhost")));
            emit(domains_event());
            return;
        }
        match crate::dns::install(on) {
            // Saying no to Windows' prompt is a choice, not an error.
            Err(e) if e.to_string().contains("canceled") => {}
            Err(e) => emit(notice("error", &trf("Could not change container names: {error}", &[("error", &e)]))),
            Ok(()) if on => emit(notice("ok", &tr("Containers now answer at <name>.kiri.local"))),
            Ok(()) => emit(notice("ok", &tr("Containers answer at <name>.kiri.localhost"))),
        }
        emit(domains_event());
        send(Cmd::Refresh);
    });
}

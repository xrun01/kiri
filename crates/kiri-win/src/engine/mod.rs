//! The window's engine, as crates/kiri/src/ui/mod.rs is on the Mac: a client
//! of the daemon that answers the page's commands and sends it the lists
//! whenever an event says they changed, so the page never polls. The page's
//! protocol is the Mac app's, unchanged; what macOS does itself (Finder,
//! Terminal, the keychain, /etc/resolver) has a Windows equivalent here or
//! is reported as not available yet.
use std::sync::mpsc::{self, Sender};

use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::app::Handle;

#[path = "../../../kiri/src/ui/brands.rs"]
pub(crate) mod brands;

#[path = "../../../kiri/src/ui/sharing.rs"]
pub(crate) mod sharing;

#[path = "../../../kiri/src/ui/needs.rs"]
mod needs;

#[path = "../../../kiri/src/ui/usage.rs"]
mod usage;

#[path = "../../../kiri/src/ui/logins.rs"]
mod logins;

#[path = "../../../kiri/src/ui/weblogin.rs"]
mod weblogin;

mod messages;
mod ops;
mod prefs;
mod pull;

pub use messages::on_message;
pub(crate) use ops::{ensure_daemon, path_escape};
pub use ops::expect_ok;
pub(crate) use prefs::{clipboard, fresh, send_windows_proxy};
pub use prefs::{prefs, tr, trf};

#[derive(Debug, Clone)]
pub(crate) enum Cmd {
    Start,
    Stop,
    Refresh,
    Pull(String),
    RemoveImage(String),
    /// An image, and the environment the Run popover asked for.
    Run(String, Vec<String>),
    Container { action: String, id: String },
    RemoveVolume(String),
    RemoveNetwork(String),
    About,
    Ports { id: String, ports: Value },
    Env { id: String, env: Vec<String> },
}


/// What the engine needs from the rest of kiri-win.
#[derive(Debug, Clone)]
pub struct Env {
    pub distro: Option<String>,
    pub pipe: String,
}

static TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
static ENV: Mutex<Option<Env>> = Mutex::new(None);
/// The tray's view of running containers: id, name.
pub(super) static RUNNING: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

pub(crate) fn send(cmd: Cmd) {
    if let Some(tx) = TX.lock().as_ref() {
        let _ = tx.send(cmd);
    }
}

pub(super) fn env() -> Env {
    ENV.lock().clone().unwrap_or(Env { distro: None, pipe: r"\\.\pipe\kiri".into() })
}

/// The WSL distro Kiri's engine runs in, when not the default one: the
/// one named at start, or the one setup has just made (a first run starts
/// with none, and setup's distro was otherwise left to WSL's default).
pub(crate) fn distro() -> Option<String> {
    env().distro.or_else(crate::setup::saved_distro)
}

/// Starts the engine thread and the event watcher.
pub fn start(e: Env) {
    *ENV.lock() = Some(e);
    let (tx, rx) = mpsc::channel();
    *TX.lock() = Some(tx.clone());
    let _ = std::thread::Builder::new().name("kiri-engine".into()).spawn(move || ops::engine(&rx, tx));
}

/// Delivers one JSON event to the page. Callable from any thread.
pub fn emit(json: String) {
    Handle.eval(format!("window.kiri && kiri.on({json})"));
}

pub fn notice(kind: &str, text: &str) -> String {
    json!({"type": "notice", "kind": kind, "text": text}).to_string()
}

pub(super) fn state(name: &str, error: Option<&str>) -> String {
    match error {
        Some(e) => json!({"type": "state", "state": name, "error": e}),
        None => json!({"type": "state", "state": name}),
    }
    .to_string()
}

/// The brand logo for an image name like `bitnami/redis:7`: first by the
/// whole repository path, then by its last component.
pub(crate) fn brand(name: &str) -> Option<[&'static str; 2]> {
    let name = name.split_once('@').map_or(name, |(n, _)| n);
    let repo = match name.rfind(':') {
        Some(i) if !name[i..].contains('/') => &name[..i],
        _ => name,
    };
    let last = repo.rsplit('/').next().unwrap_or(repo);
    let slug = [repo, last].into_iter().find_map(|key| brands::IMAGES.binary_search_by_key(&key, |(k, _)| k).ok().map(|i| brands::IMAGES[i].1))?;
    let i = brands::BRANDS.binary_search_by_key(&slug, |(s, _, _)| s).ok()?;
    let (_, hex, path) = brands::BRANDS[i];
    Some([hex, path])
}

/// The tray's commands.
pub fn on_tray(cmd: &str) {
    match cmd {
        "start" => send(Cmd::Start),
        "stop" => send(Cmd::Stop),
        "update" => crate::update::check_now(true),
        c => {
            if let Some(id) = c.strip_prefix("container:") {
                emit(json!({"type": "detail", "id": id}).to_string());
            }
        }
    }
}

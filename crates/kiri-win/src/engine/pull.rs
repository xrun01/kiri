use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;

use crate::client;

use super::ops::path_escape;
use super::prefs::trf;
use super::{brand, emit, logins, notice};

/// A pull the page asked for, from the moment it asks: its image, whether
/// its thread has taken it, its connection once open, and whether Cancel
/// was pressed. A pull's entry is the oldest untaken one with its name.
pub(super) struct Pulling {
    pub(super) run: u64,
    pub(super) name: String,
    pub(super) taken: bool,
    pub(super) closer: Option<client::Closer>,
    pub(super) cancelled: bool,
}

pub(super) static PULLING: Mutex<Vec<Pulling>> = Mutex::new(Vec::new());

/// Cancel on a pull's row: its request to the engine is closed, which stops
/// the engine's pull (unless a `docker pull` is following it too). A read
/// is only stopped while it waits, so this goes on stopping until the pull
/// has ended.
pub(super) fn cancel_pull(name: &str) {
    let mut found = false;
    for p in PULLING.lock().iter_mut().filter(|p| p.name == name) {
        p.cancelled = true;
        found = true;
    }
    if !found {
        return;
    }
    let name = name.to_owned();
    let _ = std::thread::Builder::new().name("kiri-pull-cancel".into()).spawn(move || {
        for _ in 0..600 {
            let closers: Vec<client::Closer> = {
                let pulls = PULLING.lock();
                if !pulls.iter().any(|p| p.name == name && p.cancelled) {
                    return;
                }
                pulls.iter().filter(|p| p.name == name && p.cancelled).filter_map(|p| p.closer.clone()).collect()
            };
            for c in closers {
                c.close();
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

/// Pulls through the daemon on its own thread; progress shows as a busy row.
pub(super) fn pull(name: String) {
    let _ = std::thread::Builder::new().name("kiri-pull".into()).spawn(move || {
        let reference = name.trim().to_owned();
        // This pull's entry; dropped when the pull ends, however it ends.
        let mine = PULLING.lock().iter_mut().find(|p| p.name == reference && !p.taken).map(|p| {
            p.taken = true;
            p.run
        });
        let entry = |f: &mut dyn FnMut(&mut Pulling)| {
            if let Some(p) = PULLING.lock().iter_mut().find(|p| Some(p.run) == mine) {
                f(p);
            }
        };
        let cancelled = || {
            let mut c = false;
            entry(&mut |p| c = p.cancelled);
            c
        };
        let forget = || {
            let mut pulls = PULLING.lock();
            pulls.retain(|p| Some(p.run) != mine);
        };
        if reference.is_empty() || reference.contains(char::is_whitespace) {
            forget();
            emit(notice("error", &trf("Not an image name: {reference}", &[("reference", &reference)])));
            return;
        }
        if cancelled() {
            forget();
            emit(json!({"type": "pull", "state": "done", "name": reference, "cancelled": true}).to_string());
            return;
        }
        emit(json!({"type": "pull", "state": "started", "name": reference}).to_string());
        let began = Instant::now();
        let (image, tag) = match reference.rfind(':') {
            Some(i) if !reference[i..].contains('/') => (&reference[..i], &reference[i + 1..]),
            _ => (reference.as_str(), "latest"),
        };
        // The daemon's progress lines, streamed, make one bar over every
        // layer, as the Mac shows; and the "is ready" toast at the end.
        let result = (|| -> io::Result<()> {
            let mut layers: HashMap<String, (u64, u64)> = HashMap::new();
            let mut error = None;
            let mut finished = false;
            let mut told = Instant::now();
            let path = format!("/images/create?fromImage={}&tag={}", path_escape(image), path_escape(tag));
            // Signed in to its registry: the sign-in goes with it, as docker's does.
            let auth = logins::header_for(image);
            // Kept for Cancel; cancelled already, it's closed now.
            let opened = |c: client::Closer| {
                if cancelled() {
                    c.close();
                }
                entry(&mut |p| p.closer = Some(c.clone()));
            };
            let status = client::post_lines_closable(&path, auth.as_deref(), opened, |v| {
                let id = v["id"].as_str().unwrap_or_default().to_owned();
                match v["status"].as_str().unwrap_or_default() {
                    "Pulling fs layer" => {
                        layers.insert(id, (v["size"].as_u64().unwrap_or(0), 0));
                    }
                    "Downloading" => {
                        let d = &v["progressDetail"];
                        layers.insert(id, (d["total"].as_u64().unwrap_or(0), d["current"].as_u64().unwrap_or(0)));
                    }
                    "Pull complete" => {
                        if let Some(l) = layers.get_mut(&id) {
                            l.1 = l.0;
                        }
                    }
                    _ => {}
                }
                if let Some(e) = v["error"].as_str().or(v["message"].as_str()) {
                    error = Some(e.to_owned());
                }
                if v["status"].as_str().is_some_and(|s| s.starts_with("Status: ")) {
                    finished = true;
                }
                // A few updates a second is enough for a bar; none once cancelled.
                if !layers.is_empty() && told.elapsed() >= Duration::from_millis(150) && !cancelled() {
                    told = Instant::now();
                    let (total, got) = layers.values().fold((0, 0), |(t, g), (size, done)| (t + size, g + done));
                    emit(json!({"type": "pull", "state": "progress", "name": reference, "got": got, "total": total}).to_string());
                }
            });
            // Closed by Cancel before the engine said it was done.
            if !finished && cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            let status = status?;
            match error {
                Some(e) => Err(io::Error::other(e)),
                None if (200..300).contains(&status) => Ok(()),
                None => Err(io::Error::other(format!("pull failed (HTTP {status})"))),
            }
        })();
        let stopped = result.as_ref().is_err_and(|e| e.kind() == io::ErrorKind::Interrupted) && cancelled();
        forget();
        // A failed pull says why on its own row in Images, as on the Mac.
        match result {
            Err(_) if stopped => emit(json!({"type": "pull", "state": "done", "name": reference, "cancelled": true}).to_string()),
            Ok(()) => {
                emit(json!({"type": "pull", "state": "done", "name": reference}).to_string());
                emit(json!({"type": "pulled", "name": reference, "ms": began.elapsed().as_millis() as u64, "icon": brand(&reference)}).to_string());
            }
            Err(e) => emit(json!({"type": "pull", "state": "done", "name": reference, "error": e.to_string()}).to_string()),
        }
    });
}

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::app::Handle;
use crate::{client, detail};

use super::ops::{build_with, builds_event, domains, domains_event, status_event, use_context};
use super::prefs::{allowed_url, ask_language, clipboard, prefs, proxy, router_ports, set_pref, tr, trf};
use super::pull::{PULLING, Pulling, cancel_pull};
use super::{Cmd, emit, env, logins, needs, notice, send, sharing, usage, weblogin};

/// A command as the page sends it.
#[derive(serde::Deserialize)]
struct PageCmd {
    cmd: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    action: String,
    #[serde(default)]
    ports: Value,
    #[serde(default)]
    path: String,
    #[serde(default)]
    value: Value,
}


/// Handles one message from the page, on the window's thread: quick things
/// here, the rest on the engine thread or a thread of their own.
pub fn on_message(msg: &str) {
    let Ok(c) = serde_json::from_str::<PageCmd>(msg) else { return };
    // Ids and container names are plain tokens; anything else is not from
    // the page as written, and never reaches a request line.
    let plain = |s: &str| s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b));
    if !plain(&c.id) || (matches!(c.cmd.as_str(), "files" | "preview" | "download" | "terminal") && !plain(&c.name)) {
        return;
    }
    match c.cmd.as_str() {
        // An uncaught error in the page (the shim in app.rs reports it).
        "jserror" => crate::serve::log(&format!("page: {}", c.name.chars().take(300).collect::<String>())),
        "ready" => {
            emit(json!({"type": "prefs", "prefs": prefs()}).to_string());
            // KIRI_STAR_PROMPT=1 shows the star card now, as on the Mac.
            if std::env::var_os("KIRI_STAR_PROMPT").is_some() {
                emit(r#"{"type":"starprompt"}"#.to_owned());
            }
            emit(domains_event());
            emit(status_event());
            ask_language();
            // The first time, offer <name>.kiri.local, as the Mac does.
            if !crate::dns::installed() && crate::dns::serving() && prefs()["domainsAsked"] != true {
                set_pref("domainsAsked", json!(true));
                emit(r#"{"type":"welcome"}"#.to_owned());
            }
            // Which builder docker build uses: made so, and asked once with
            // another engine here (the Mac's welcome comes first, as there).
            std::thread::spawn(|| {
                build_with();
                if let Some(engine) = crate::buildwith::ask() {
                    emit(json!({"type": "buildask", "engine": engine}).to_string());
                }
                emit(crate::buildwith::event());
            });
            send(Cmd::Refresh);
            // Opening the window starts Kiri, unless it was stopped: that waits for Start.
            std::thread::spawn(|| {
                if !client::get("/info").is_ok_and(|i| i["KiriStopped"] == true) {
                    send(Cmd::Start);
                }
            });
            // Anything newer than this build, and a staged update not yet in.
            crate::update::check_now(false);
            // For checking the page by hand in a debug build.
            if cfg!(debug_assertions) && let Ok(js) = std::env::var("KIRI_UI_EVAL") {
                Handle.eval(js);
            }
        }
        "start" => send(Cmd::Start),
        "stop" => send(Cmd::Stop),
        "refresh" => send(Cmd::Refresh),
        "pull" => {
            static RUN: AtomicU64 = AtomicU64::new(0);
            let run = RUN.fetch_add(1, Ordering::Relaxed);
            PULLING.lock().push(Pulling { run, name: c.name.trim().to_owned(), taken: false, closer: None, cancelled: false });
            send(Cmd::Pull(c.name));
        }
        // Cancel on a pull's row, as on the Mac.
        "cancelpull" => cancel_pull(c.name.trim()),
        // Pull image's suggestions, from Docker Hub through a cache.
        "hubsearch" => crate::hub::ask(crate::hub::Kind::Search, c.name, emit),
        "hubtags" => crate::hub::ask(crate::hub::Kind::Tags, c.name, emit),
        "rmi" => send(Cmd::RemoveImage(c.name)),
        "run" => {
            let env = c.value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            send(Cmd::Run(c.name, env));
        }
        "container" => send(Cmd::Container { action: c.action, id: c.id }),
        "ports" => send(Cmd::Ports { id: c.id, ports: c.ports }),
        "env" => {
            let env = c.value.as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            send(Cmd::Env { id: c.id, env });
        }
        "rmvol" => send(Cmd::RemoveVolume(c.name)),
        "rmnet" => send(Cmd::RemoveNetwork(c.id)),
        "about" => send(Cmd::About),
        // Settings → Builds and the first-run question; with a value, switches.
        "buildwith" => {
            let with = c.value.as_str().and_then(crate::buildwith::With::parse);
            std::thread::spawn(move || {
                if let Some(with) = with
                    && let Err(e) = crate::buildwith::choose(with)
                {
                    emit(notice("error", &trf("Couldn't change which builder docker build uses: {error}", &[("error", &e)])));
                }
                emit(crate::buildwith::event());
            });
        }
        // The docker CLI can take a moment: off the window's thread.
        "usecontext" => {
            std::thread::spawn(move || use_context(&c.name));
        }
        // The Kubernetes section: Pods, Services, a pod's logs, Start.
        "k8s" => crate::k8s::handle(&c.action, &c.value),
        // Share: public links for a port or container, and the Share account.
        "shares" => sharing::list(),
        "share" => sharing::open(c.name, c.value.as_str().unwrap_or_default().to_owned()),
        // A Kubernetes service or pod: value {ip, port, label}.
        "k8sshare" => {
            let v = &c.value;
            let port = v["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
            sharing::open_k8s(c.name, v["ip"].as_str().unwrap_or_default().to_owned(), port, v["label"].as_str().unwrap_or_default().to_owned(), v["name"].as_str().unwrap_or_default().to_owned());
        }
        "shareusage" => sharing::usage(),
        // The account's live tunnels from every computer, and closing one this window didn't open.
        "sharetunnels" => sharing::tunnels(),
        "sharetraffic" => sharing::traffic(),
        "usage" => {
            std::thread::spawn(|| emit(usage::event()));
        }
        "sharestop" => sharing::stop_tunnel(c.name),
        "sharebrowse" => sharing::browse_tunnel(&c.name),
        "unshare" => sharing::close(c.id.parse().unwrap_or(0)),
        "shareopen" => sharing::browse(c.id.parse().unwrap_or(0)),
        "sharelogin" => sharing::sign_in(),
        "sharecancel" => sharing::cancel_sign_in(),
        "sharelogout" => sharing::sign_out(),
        // The built-in terminal: its shells, and its own clipboard and links.
        "term" => match c.name.as_str() {
            // A selection can be a whole screen of output.
            "copy" => {
                if let Some(text) = c.value.as_str().filter(|t| !t.is_empty() && t.len() <= crate::term::COPY_MAX)
                    && clipboard(text).is_err()
                {
                    emit(notice("error", &tr("Couldn't copy to the clipboard")));
                }
            }
            "paste" => {
                let id = c.id;
                crate::term::paste(move |text| emit(crate::term::paste_event(&id, &text)));
            }
            // A link in a shell's output: web pages only, never a file or an app's scheme.
            "link" => {
                if let Some(url) = c.value.as_str().filter(|u| crate::term::web_link(u)) {
                    crate::app::open_url(url);
                }
            }
            action => crate::term::handle(action, &c.id, &c.value, emit),
        },
        // The page can't write the clipboard itself; IDs and short text only.
        "copy" => {
            if !c.name.is_empty() && c.name.len() <= 4096 && clipboard(&c.name).is_err() {
                emit(notice("error", &tr("Couldn't copy to the clipboard")));
            }
        }
        // About's Check now.
        "updatecheck" => crate::update::check_now(true),
        // The update card and the status bar's update chip.
        "updatedownload" => crate::update::download_now(),
        "updatecancel" => crate::update::cancel_download(),
        "updaterestart" => crate::update::restart_now(),
        "restartengine" => crate::update::restart_engine_now(),
        // What's new: the feed names the page, so only runkiri.com's.
        "updatenotes" => {
            if c.name.strip_prefix("https://runkiri.com/").is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._~/#".contains(&b))) {
                crate::app::open_url(&c.name);
            }
        }
        "restartvm" => crate::wslconfig::restart(),
        "inspect" => detail::inspect(c.id),
        "logs" => detail::logs(c.id),
        "logsstop" => detail::stop_logs(),
        // Registries signed in to, signing in (checked by the engine first) and out.
        "logins" | "login" | "logout" => logins::handle(&c.cmd, c.name, &c.value, emit, || Ok(())),
        "weblogin" | "webloginopen" | "webloginmfa" | "webloginsso" | "weblogincancel" | "awsprofiles" => weblogin::handle(&c.cmd, c.name, &c.value, emit, || Ok(()), crate::term::captured),
        // Before Run: what the image's start script won't start without.
        "needs" => {
            let image = c.name;
            std::thread::spawn(move || emit(needs::event(&image)));
        }
        // A container that stopped: its last lines, for why.
        "lastlogs" => detail::last_logs(c.id),
        "stats" => detail::stats(c.id),
        "statsstop" => detail::stop_stats(),
        "exec" => detail::exec(c.id, c.path),
        "files" => detail::files(c.name, c.path),
        "preview" => detail::preview(c.name, c.path),
        "download" => detail::download(c.name, c.path),
        "reveal" => emit(notice("error", &tr("Container files are not on Windows' disk: use Save to Downloads"))),
        "terminal" => detail::terminal(&c.name, &env().pipe),
        "open" => {
            if allowed_url(&c.name) {
                crate::app::open_url(&c.name);
            }
        }
        "setpref" => {
            if c.name == "theme" {
                let theme = c.value.as_str().unwrap_or("").to_owned();
                Handle.run(move |ui| ui.set_theme(&theme));
            }
            set_pref(&c.name, c.value);
        }
        // A language the page doesn't carry yet (the main window and setup open
        // with one; setup's langwords comes here too, not being a setup- command):
        // its words, which the page adds and shows if it's the one picked.
        "langwords" => {
            if let Some(js) = crate::lang::words_js(&c.name) {
                Handle.eval(js);
            }
        }
        "fonts" => emit(json!({"type": "fonts", "families": crate::app::font_families(false), "mono": crate::app::font_families(true)}).to_string()),
        "proxy" => {
            std::thread::spawn(|| proxy(None));
        }
        "setproxy" => {
            let body = c.value;
            std::thread::spawn(move || proxy(Some(body)));
        }
        // Settings → Resources: WSL's CPUs and memory, in .wslconfig.
        "resources" => crate::wslconfig::resources(None),
        "setresources" => crate::wslconfig::resources(Some(c.value)),
        "domains" => domains(c.value.as_bool().unwrap_or(false)),
        "routerports" => {
            std::thread::spawn(|| router_ports(None));
        }
        "setrouterports" => {
            let body = c.value;
            std::thread::spawn(move || router_ports(Some(body)));
        }
        // The USB view: usbipd-win's devices, and which are in WSL.
        "usb" => {
            std::thread::spawn(|| emit(crate::usb::list().to_string()));
        }
        "usbattach" | "usbdetach" | "usbinstall" => crate::usb::handle(if c.cmd == "usbattach" { "usbattach" } else if c.cmd == "usbdetach" { "usbdetach" } else { "usbinstall" }, c.name),
        // The Builds view: BuildKit's history, read only while the view is open.
        "builds" => {
            std::thread::spawn(|| emit(builds_event()));
        }
        "build" => {
            std::thread::spawn(move || {
                emit(match crate::builds::detail(&c.name, &c.id) {
                    Ok(mut b) => {
                        b["type"] = "build".into();
                        b
                    }
                    Err(e) => json!({"type": "build", "builder": c.name, "id": c.id, "error": {"message": e.to_string()}, "steps": [], "failed": true}),
                }
                .to_string());
            });
        }
        // Builds → Layers: the image a build made.
        "buildlayers" => {
            std::thread::spawn(move || {
                emit(match crate::builds::layers(&c.name, &c.id) {
                    Ok(mut l) => {
                        l["type"] = "buildlayers".into();
                        l
                    }
                    Err(e) => json!({"type": "buildlayers", "builder": c.name, "id": c.id, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        // Images → an image's layers, read by the engine in WSL (crate::scan).
        "imagelayers" => {
            std::thread::spawn(move || {
                emit(match crate::scan::image_layers(&c.name) {
                    Ok(mut l) => {
                        l["type"] = "imagelayers".into();
                        l
                    }
                    Err(e) => json!({"type": "imagelayers", "name": c.name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        // Security: scan an image (name or ID) with Grype, or show its last scan.
        "scan" => {
            std::thread::spawn(move || {
                let name = c.name;
                let mut last = (String::new(), 255u8);
                let r = crate::scan::scan(&name, &mut |phase, pct| {
                    if (phase, pct) != (last.0.as_str(), last.1) {
                        last = (phase.to_owned(), pct);
                        emit(json!({"type": "scanprogress", "name": name, "phase": phase, "pct": pct}).to_string());
                    }
                });
                emit(match r {
                    Ok(res) => json!({"type": "scanresult", "name": name, "result": res}),
                    Err(e) => json!({"type": "scanresult", "name": name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        // An image's scan history, or one scan of it (`id` its time).
        "scanhistory" => {
            std::thread::spawn(move || {
                emit(match crate::scan::history(&c.name) {
                    Ok(h) => json!({"type": "scanhistory", "name": c.name, "scans": h["scans"]}),
                    Err(e) => json!({"type": "scanhistory", "name": c.name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        "scanopen" => {
            std::thread::spawn(move || {
                emit(match crate::scan::open(&c.name, c.id.parse::<u64>().ok()) {
                    Ok(res) => json!({"type": "scanresult", "name": c.name, "result": res}),
                    Err(e) => json!({"type": "scanresult", "name": c.name, "error": e.to_string()}),
                }
                .to_string());
            });
        }
        "savesbom" => {
            std::thread::spawn(move || match crate::scan::save_sbom(&c.name, c.id.parse().unwrap_or(0), &c.path) {
                Ok(to) => {
                    emit(notice("ok", &trf("Saved the SBOM to Downloads as {file}", &[("file", &to.file_name().unwrap_or_default().to_string_lossy())])));
                    crate::app::reveal(&to);
                }
                Err(e) => emit(notice("error", &trf("Could not save the SBOM: {error}", &[("error", &e)]))),
            });
        }
        "advisory" => crate::scan::open_advisory(&c.name),
        "rmbuild" => {
            std::thread::spawn(move || {
                if let Err(e) = crate::builds::remove(&c.name, &c.id) {
                    emit(notice("error", &trf("Couldn't remove the build: {error}", &[("error", &e)])));
                }
                emit(builds_event());
            });
        }
        // Settings → Diagnostics and an error's Details: crate::diag.
        cmd @ ("diaglogs" | "diagreport" | "diagsend" | "diagcrashes" | "diagsendcrash" | "diagfolder") => {
            crate::diag::handle(cmd, c.name, c.id, &c.value);
        }
        "importsources" => emit(json!({"type": "importsources", "sources": []}).to_string()),
        "zoom" => Handle.run(crate::app::Ui::toggle_maximized),
        // The window has Windows' own title bar to drag.
        "drag" | "toolsretry" | "inventory" | "import" => {}
        _ => {}
    }
}

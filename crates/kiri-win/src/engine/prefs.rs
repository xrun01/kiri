use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::client;

use super::{emit, expect_ok, notice};

/// Puts `text` on the clipboard, by clip.exe: the page's own clipboard
/// needs a gesture the WebView doesn't always see.
pub(crate) fn clipboard(text: &str) -> io::Result<()> {
    let mut cmd = crate::win::tool("clip.exe");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    child.stdin.take().ok_or_else(|| io::Error::other("no stdin"))?.write_all(text.as_bytes())?;
    let status = child.wait()?;
    if status.success() { Ok(()) } else { Err(io::Error::other("clip.exe failed")) }
}

/// Kiri opening for the first time on this account: no settings were saved
/// before this run. Worked out on the first read, before anything is saved.
pub(crate) fn fresh() -> bool {
    static FRESH: OnceLock<bool> = OnceLock::new();
    *FRESH.get_or_init(|| prefs_path().is_some_and(|p| !p.exists()))
}

/// The language step, once: on a first start, or after an update that
/// brings this computer's language (lang::should_ask). The welcome waits behind it.
pub(super) fn ask_language() {
    static ASKED: AtomicBool = AtomicBool::new(false);
    if crate::lang::should_ask(fresh(), &prefs(), &crate::lang::os_languages()) && !ASKED.swap(true, Ordering::SeqCst) {
        emit(r#"{"type":"langpick"}"#.to_owned());
    }
}

/// The window's settings, in %LOCALAPPDATA%\Kiri\ui.json.
fn prefs_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Kiri").join("ui.json"))
}

/// The settings, with the defaults for what the person hasn't picked: Kiri
/// starts dark until Settings → Appearance says otherwise.
pub fn prefs() -> Value {
    fresh();
    let mut p: Value = prefs_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| json!({}));
    if !p.is_object() {
        p = json!({});
    }
    if p.get("theme").is_none() {
        p["theme"] = "dark".into();
    }
    p
}

/// `en` in the window's language (lang.rs), for words sent to the page:
/// notices, and the setup steps.
pub fn tr(en: &str) -> String {
    crate::lang::tr(crate::lang::resolve(prefs()["lang"].as_str()), en)
}

/// [`tr`] with `{name}` placeholders filled, as the page's tr() does.
pub fn trf(en: &str, vars: &[(&str, &dyn std::fmt::Display)]) -> String {
    crate::lang::fill(tr(en), vars)
}

pub(super) fn set_pref(name: &str, value: Value) {
    if !matches!(name, "font" | "mono" | "scale" | "termH" | "termFs" | "termFont" | "domainsAsked" | "theme" | "accent" | "starAsked" | "updateAuto" | "lang" | "langAsked") {
        return;
    }
    let Some(path) = prefs_path() else { return };
    let mut p = prefs();
    p[name] = value;
    let tmp = path.with_extension("json.tmp");
    let saved = path.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&tmp, p.to_string())).and_then(|()| std::fs::rename(&tmp, &path));
    if saved.is_err() {
        emit(notice("error", &trf("Could not save the setting to {path}", &[("path", &"%LOCALAPPDATA%\\Kiri\\ui.json")])));
    }
}

/// A page link: only Kiri's own addresses, as on the Mac.
pub(super) fn allowed_url(url: &str) -> bool {
    if url == "https://kiri.canestack.com" || url == "https://learn.microsoft.com/windows/wsl/about" || url == "https://github.com/libracoder/kiri/issues/new" || url == "https://github.com/libracoder/kiri" || url == "https://login.docker.com/activate" || url == "https://github.com/login/device" {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else { return false };
    // The authority is up to the first path/query/fragment. Reject credentials
    // (user@host, so the real host is after the @) and backslashes (which some
    // parsers treat as a separator), then take the host before the port.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') || authority.contains('\\') {
        return false;
    }
    let host = authority.split(':').next().unwrap_or("");
    host == "localhost" || host.ends_with(".kiri.localhost") || host.ends_with(".kiri.local")
}

/// Windows' proxy, from Internet Settings, for the daemon's "system" mode:
/// `{http, https, no_proxy, pac}`. ProxyServer is `host:port` for every
/// protocol, or `http=host:port;https=host:port`.
fn windows_proxy() -> Value {
    let mut cmd = crate::win::tool("reg");
    std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
    let out = cmd.args(["query", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let field = |name: &str| out.lines().find_map(|l| {
        let mut p = l.split_whitespace();
        (p.next()? == name).then(|| p.skip(1).collect::<Vec<_>>().join(" "))
    });
    let enabled = field("ProxyEnable").is_some_and(|v| v == "0x1");
    let server = if enabled { field("ProxyServer").unwrap_or_default() } else { String::new() };
    let (mut http, mut https) = (None, None);
    if server.contains('=') {
        for part in server.split(';') {
            match part.split_once('=') {
                Some(("http", v)) => http = Some(v.to_owned()),
                Some(("https", v)) => https = Some(v.to_owned()),
                _ => {}
            }
        }
    } else if !server.is_empty() {
        (http, https) = (Some(server.clone()), Some(server));
    }
    let no_proxy: Vec<String> = if enabled {
        field("ProxyOverride").unwrap_or_default().split(';').map(str::trim).filter(|h| !h.is_empty() && *h != "<local>").map(str::to_owned).collect()
    } else {
        Vec::new()
    };
    json!({"http": http, "https": https, "no_proxy": no_proxy, "pac": field("AutoConfigURL").is_some()})
}

/// Tells the daemon what Windows' proxy is. A proxy on Windows' own
/// 127.0.0.1 (Windows' or one set in Settings → Network) is reached through
/// the relay (relay.rs), and the daemon gets its ports too.
pub(crate) fn send_windows_proxy() {
    let mut body = windows_proxy();
    let custom = client::get("/kiri/proxy").map(|v| v["settings"].clone()).unwrap_or_default();
    let mut targets: Vec<u16> = [&body["http"], &body["https"], &custom["http"], &custom["https"]]
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(|u| crate::hostrelay::loopback(u).map(|l| l.0))
        .collect();
    targets.sort_unstable();
    targets.dedup();
    let relay = crate::relay::ports(&targets);
    body["relay"] = relay.iter().map(|(t, l)| (t.to_string(), json!(l))).collect::<serde_json::Map<_, _>>().into();
    let _ = client::request("PUT", "/kiri/proxy/system", Some(&body));
}

/// Settings → Domains: reads (or moves, then reads) the routers' ports. The
/// routers listen on WSL's loopback; WSL forwards their port to Windows'.
pub(super) fn router_ports(set: Option<Value>) {
    if let Some(body) = set {
        match expect_ok(client::request("POST", "/kiri/domains", Some(&body))) {
            Ok(()) => emit(notice("ok", &tr("Ports changed"))),
            Err(e) => emit(notice("error", &trf("Could not change the ports: {error}", &[("error", &e)]))),
        }
    }
    let mut v = client::get("/kiri/domains").unwrap_or_else(|e| json!({"error": e.to_string()}));
    v["type"] = json!("routerports");
    emit(v.to_string());
}

/// Settings → Network: the daemon's proxy settings.
pub(super) fn proxy(set: Option<Value>) {
    send_windows_proxy();
    if let Some(body) = set {
        match expect_ok(client::request("POST", "/kiri/proxy", Some(&body))) {
            Ok(()) => {
                // A custom proxy on Windows' loopback needs the relay too.
                send_windows_proxy();
                emit(notice("ok", &tr("Proxy settings saved")));
            }
            Err(e) => emit(notice("error", &trf("Could not save the proxy settings: {error}", &[("error", &e)]))),
        }
    }
    let ev = match client::get("/kiri/proxy") {
        Ok(v) => {
            let mut v = v;
            v["type"] = json!("proxy");
            v
        }
        Err(e) => json!({"type": "proxy", "error": e.to_string()}),
    };
    emit(ev.to_string());
}

#[cfg(test)]
mod tests {
    use super::allowed_url;

    #[test]
    fn only_kiris_own_urls_open() {
        assert!(allowed_url("http://localhost:8080/app"));
        assert!(allowed_url("https://web.kiri.localhost/"));
        assert!(allowed_url("http://api.kiri.local"));
        assert!(allowed_url("https://github.com/libracoder/kiri"));
        assert!(!allowed_url("http://evil.example/"));
        // Bypass attempts the audit found.
        assert!(!allowed_url("http://localhost:1@evil.example/"));
        assert!(!allowed_url("http://evil.example\\x.kiri.localhost"));
        assert!(!allowed_url("https://evil.example/#.kiri.localhost"));
        assert!(!allowed_url("ftp://localhost/"));
    }
}

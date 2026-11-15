use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

use super::config::{
    BYTES_PER_MIB, HIBERNATE_SNAP_NAME, RECLAIM_TARGET_BYTES, WinConfig, disk_path_in, ensure_disk_in, load_config_in, resolve_bundle_paths, shared_folders_payload, snaps_dir_in,
    update_config_in, win_home,
};
use super::session::{close_active_session, start_vm, with_active_session};
use super::snapshot::{delete_snapshot_in, hibernate_vm, list_snapshots_in, restore_snapshot_in, run_smart_guard_snapshot};

const SCANCODE_LCTRL: u32 = 0x1d;
const SCANCODE_LALT: u32 = 0x38;
const SCANCODE_DELETE: u32 = 0xe053;
const SCANCODE_LWIN: u32 = 0xe05b;
const SCANCODE_LSHIFT: u32 = 0x2a;
const SCANCODE_ESC: u32 = 0x01;
const SCANCODE_R: u32 = 0x13;
const SCANCODE_L: u32 = 0x26;

/// @xvm(feat = FEAT_COHERENCE, layer = LAYER_UI, role = TAG_ROLE_UI)
fn set_coherence_mode(home: &Path, enabled: bool) -> Result<WinConfig, String> {
    let cfg = update_config_in(home, |cfg| {
        cfg.display_mode = if enabled { "coherence".to_owned() } else { "desktop".to_owned() };
    })?;
    let _ = with_active_session(|s| s.tool_call("vm_set_coherence", json!({ "enabled": enabled })));
    Ok(cfg)
}

/// @xvm(feat = FEAT_MODALITY_PIP, layer = LAYER_UI, role = TAG_ROLE_UI)
fn set_pip_modality(home: &Path, enabled: bool) -> Result<WinConfig, String> {
    // 画中画置顶模式标志持久化写入配置并在运行期同步关闭融合模式
    let cfg = update_config_in(home, |cfg| {
        cfg.display_mode = if enabled { "pip".to_owned() } else { "desktop".to_owned() };
    })?;
    if cfg.display_mode == "pip" {
        let _ = with_active_session(|s| s.tool_call("vm_set_coherence", json!({ "enabled": false })));
    }
    Ok(cfg)
}

/// @xvm(feat = FEAT_ECO_MODE, layer = LAYER_UI, role = TAG_ROLE_UI)
fn toggle_eco_mode(home: &Path, enabled: bool) -> Result<WinConfig, String> {
    let cfg = update_config_in(home, |cfg| {
        cfg.eco_mode = enabled;
    })?;
    let _ = with_active_session(|s| s.tool_call("vm_set_eco_mode", json!({ "enabled": enabled })));
    Ok(cfg)
}

/// @xvm(feat = FEAT_AUTO_PAUSE, layer = LAYER_UI, role = TAG_ROLE_UI)
fn toggle_auto_pause(home: &Path, enabled: bool) -> Result<WinConfig, String> {
    update_config_in(home, |cfg| {
        cfg.auto_pause = enabled;
    })
}

/// @xvm(feat = FEAT_RICH_CLIPBOARD, layer = LAYER_UI, role = TAG_ROLE_UI)
fn sync_clipboard_state(home: &Path, enabled: bool, text: Option<&str>) -> Result<WinConfig, String> {
    let cfg = update_config_in(home, |cfg| {
        cfg.clipboard_sync = enabled;
    })?;
    if enabled && let Some(clip) = text {
        let _ = with_active_session(|s| s.tool_call("vm_sync_clipboard", json!({ "text": clip })));
    }
    Ok(cfg)
}

/// @xvm(feat = FEAT_NET_VMNET, layer = LAYER_UI, role = TAG_ROLE_UI)
fn apply_vmnet_mode(home: &Path, mode: &str, isolated: bool) -> Result<WinConfig, String> {
    let cfg = update_config_in(home, |cfg| {
        cfg.net_mode = if mode == "bridged" { "bridged".to_owned() } else { "shared".to_owned() };
        cfg.isolated = isolated;
    })?;
    let _ = with_active_session(|s| s.tool_call("vm_set_network_isolation", json!({ "isolated": isolated })));
    Ok(cfg)
}

/// @xvm(feat = FEAT_RESOURCE_HUD, layer = LAYER_UI, role = TAG_ROLE_UI)
fn query_resource_hud() -> Value {
    let live = with_active_session(|s| s.tool_call("vm_query_status", json!({}))).ok().flatten();
    live.and_then(|v| v.get("status").cloned()).unwrap_or(Value::Null)
}

/// @xvm(feat = FEAT_DUAL_RECLAIM, layer = LAYER_UI, role = TAG_ROLE_UI)
fn reclaim_resources(home: &Path) -> Result<Value, String> {
    let cfg = load_config_in(home);
    let target = (cfg.memory_mib * BYTES_PER_MIB / 4).min(RECLAIM_TARGET_BYTES);
    let res = with_active_session(|s| s.tool_call("vm_reclaim_resources", json!({ "targetMemoryBytes": target })))?
        .ok_or_else(|| "Windows VM is not running.".to_owned())?;
    Ok(res)
}

/// @xvm(feat = FEAT_SMART_KEYMAP, layer = LAYER_UI, role = TAG_ROLE_UI)
fn send_keymap_shortcut(shortcut: &str) -> Result<bool, String> {
    let key_seq: &[u32] = match shortcut {
        "ctrl_alt_del" => &[SCANCODE_LCTRL, SCANCODE_LALT, SCANCODE_DELETE],
        "run" | "win_r" => &[SCANCODE_LWIN, SCANCODE_R],
        "task_mgr" => &[SCANCODE_LCTRL, SCANCODE_LSHIFT, SCANCODE_ESC],
        "lock_screen" => &[SCANCODE_LWIN, SCANCODE_L],
        other => return Err(format!("Unknown keyboard shortcut: {other}")),
    };
    let ran = with_active_session(|s| {
        for &code in key_seq {
            s.tool_call("vm_send_key", json!({ "scanCode": code, "isKeyDown": true, "modifiers": 0 }))?;
        }
        for &code in key_seq.iter().rev() {
            s.tool_call("vm_send_key", json!({ "scanCode": code, "isKeyDown": false, "modifiers": 0 }))?;
        }
        Ok(true)
    })?;
    ran.ok_or_else(|| "Windows VM is not running.".to_owned())
}

/// @xvm(feat = FEAT_DOCK_APP, layer = LAYER_UI, role = TAG_ROLE_UI)
fn launch_guest_shortcut(command: &str) -> Result<bool, String> {
    let target = if command.trim().is_empty() { "__start_menu__" } else { command.trim() };
    let ran = with_active_session(|s| s.tool_call("vm_launch_guest_app", json!({ "command": target })))?;
    ran.map(|v| v["ok"] == true).ok_or_else(|| "Windows VM is not running.".to_owned())
}

fn status_in(home: &Path) -> Value {
    let cfg = load_config_in(home);
    let bundle = resolve_bundle_paths();
    let disk_path = disk_path_in(home);
    let disk_exists = disk_path.is_file();
    let disk_bytes = fs::metadata(&disk_path).map_or(0, |m| m.len());
    let live = query_resource_hud();
    let running = live["running"].as_bool().unwrap_or(false);
    let paused = live["paused"].as_bool().unwrap_or(false);
    let hibernated = !running && snaps_dir_in(home).join(HIBERNATE_SNAP_NAME).join("state.vmsave").is_file();
    let state = if !running {
        if hibernated { "hibernated" } else { "stopped" }
    } else if paused {
        "paused"
    } else {
        "running"
    };
    json!({
        "state": state,
        "running": running,
        "paused": paused,
        "hibernated": hibernated,
        "home": home.display().to_string(),
        "diskPath": disk_path.display().to_string(),
        "diskExists": disk_exists,
        "diskBytes": disk_bytes,
        "vmMcpFound": bundle.vm_mcp.is_file(),
        "efiFound": bundle.efi.is_file(),
        "guestToolsFound": bundle.guest_iso.is_file(),
        "config": cfg,
        "live": live,
        "snapshots": list_snapshots_in(home),
    })
}

pub(crate) fn status_json() -> Value {
    status_in(&win_home())
}

pub(crate) fn snapshots_list() -> Value {
    json!({ "snapshots": list_snapshots_in(&win_home()) })
}

pub(crate) fn snapshot_create(payload: &Value) -> Result<Value, String> {
    let home = win_home();
    let name = payload["name"].as_str();
    let note = payload["note"].as_str().unwrap_or("");
    let auto_guard = payload["autoGuard"].as_bool().unwrap_or(false);
    let meta = run_smart_guard_snapshot(&home, name, note, auto_guard)?;
    Ok(json!({ "snapshot": meta, "status": status_in(&home) }))
}

pub(crate) fn snapshot_delete(name: &str) -> Result<Value, String> {
    let home = win_home();
    delete_snapshot_in(&home, name)?;
    Ok(status_in(&home))
}

pub(crate) fn handle_action(action: &str, payload: &Value) -> Result<Value, String> {
    let home = win_home();
    match action {
        "" | "status" => {}
        "start" => start_vm(&home)?,
        "stop" => close_active_session()?,
        "pause" => {
            with_active_session(|s| s.tool_call("vm_pause", json!({})))?
                .ok_or_else(|| "Windows VM is not running.".to_owned())?;
        }
        "resume" => {
            with_active_session(|s| s.tool_call("vm_resume", json!({})))?
                .ok_or_else(|| "Windows VM is not running.".to_owned())?;
        }
        "reset" => {
            with_active_session(|s| s.tool_call("vm_reset", json!({})))?
                .ok_or_else(|| "Windows VM is not running.".to_owned())?;
        }
        "hibernate" => hibernate_vm(&home)?,
        "reclaim" => {
            let result = reclaim_resources(&home)?;
            let mut st = status_in(&home);
            st["reclaimResult"] = result;
            return Ok(st);
        }
        "coherence" => {
            let enabled = payload["enabled"].as_bool().unwrap_or(true);
            set_coherence_mode(&home, enabled)?;
        }
        "pip" => {
            let enabled = payload["enabled"].as_bool().unwrap_or(true);
            set_pip_modality(&home, enabled)?;
        }
        "eco" => {
            let enabled = payload["enabled"].as_bool().unwrap_or(false);
            toggle_eco_mode(&home, enabled)?;
        }
        "autopause" => {
            let enabled = payload["enabled"].as_bool().unwrap_or(true);
            toggle_auto_pause(&home, enabled)?;
        }
        "clipboard" => {
            let enabled = payload["enabled"].as_bool().unwrap_or(true);
            sync_clipboard_state(&home, enabled, payload["text"].as_str())?;
        }
        "vmnet" => {
            let mode = payload["mode"].as_str().unwrap_or("shared");
            let isolated = payload["isolated"].as_bool().unwrap_or(false);
            apply_vmnet_mode(&home, mode, isolated)?;
        }
        "key" => {
            let shortcut = payload["shortcut"].as_str().unwrap_or("ctrl_alt_del");
            send_keymap_shortcut(shortcut)?;
        }
        "launch" => {
            let cmd = payload["command"].as_str().unwrap_or("__start_menu__");
            launch_guest_shortcut(cmd)?;
        }
        "config" => {
            let cfg = update_config_in(&home, |cfg| {
                if let Some(v) = payload["cpus"].as_u64() {
                    cfg.cpus = v.clamp(1, 16) as u32;
                }
                if let Some(v) = payload["memoryMib"].as_u64() {
                    cfg.memory_mib = v.clamp(2048, 65536);
                }
                if let Some(v) = payload["diskGb"].as_u64() {
                    cfg.disk_gb = v.clamp(16, 1024);
                }
                if let Some(v) = payload["isoPath"].as_str() {
                    cfg.iso_path = v.trim().to_owned();
                }
                if let Some(v) = payload["displayMode"].as_str() && matches!(v, "desktop" | "coherence" | "pip") {
                    cfg.display_mode = v.to_owned();
                }
                if let Some(v) = payload["netMode"].as_str() && matches!(v, "shared" | "bridged") {
                    cfg.net_mode = v.to_owned();
                }
                if let Some(v) = payload["ecoMode"].as_bool() {
                    cfg.eco_mode = v;
                }
                if let Some(v) = payload["autoPause"].as_bool() {
                    cfg.auto_pause = v;
                }
                if let Some(v) = payload["isolated"].as_bool() {
                    cfg.isolated = v;
                }
                if let Some(v) = payload["smartGuard"].as_bool() {
                    cfg.smart_guard = v;
                }
                if let Some(v) = payload["smartKeymap"].as_bool() {
                    cfg.smart_keymap = v;
                }
                if let Some(v) = payload["clipboardSync"].as_bool() {
                    cfg.clipboard_sync = v;
                }
                if let Some(v) = payload["unattended"].as_bool() {
                    cfg.unattended = v;
                }
                if let Some(v) = payload["shareDesktop"].as_bool() {
                    cfg.share_desktop = v;
                }
                if let Some(v) = payload["shareDocuments"].as_bool() {
                    cfg.share_documents = v;
                }
                if let Some(v) = payload["shareDownloads"].as_bool() {
                    cfg.share_downloads = v;
                }
            })?;
            let _ = ensure_disk_in(&home, cfg.disk_gb);
            let _ = with_active_session(|s| {
                let _ = s.tool_call("vm_update_shared_folders", json!({ "sharedFolders": shared_folders_payload(&cfg) }));
                let _ = s.tool_call("vm_set_eco_mode", json!({ "enabled": cfg.eco_mode }));
                s.tool_call("vm_set_network_isolation", json!({ "isolated": cfg.isolated }))
            });
        }
        "snap" => {
            run_smart_guard_snapshot(
                &home,
                payload["name"].as_str(),
                payload["note"].as_str().unwrap_or(""),
                payload["autoGuard"].as_bool().unwrap_or(false),
            )?;
        }
        "restore" => {
            let name = payload["name"].as_str().ok_or("snapshot name is required")?;
            restore_snapshot_in(&home, name)?;
        }
        "rm-snap" => {
            let name = payload["name"].as_str().ok_or("snapshot name is required")?;
            delete_snapshot_in(&home, name)?;
        }
        other => return Err(format!("Unsupported winvm action: {other}")),
    }
    Ok(status_in(&home))
}

pub(crate) fn cli(args: &[String]) -> io::Result<()> {
    let sub = args.first().map(String::as_str).unwrap_or("status");
    let res = match sub {
        "status" => Ok(status_json()),
        "start" | "stop" | "pause" | "resume" | "reset" | "hibernate" | "reclaim" => {
            handle_action(sub, &Value::Null)
        }
        "snap" => {
            let name = args.iter().skip(1).find(|a| !a.starts_with("--"));
            let guard = args.iter().any(|a| a == "--guard");
            handle_action("snap", &json!({ "name": name, "autoGuard": guard }))
        }
        "restore" => handle_action("restore", &json!({ "name": args.get(1) })),
        "rm-snap" => handle_action("rm-snap", &json!({ "name": args.get(1) })),
        "config" => {
            let mut body = serde_json::Map::new();
            let mut i = 1;
            while i < args.len() {
                let Some(val) = args.get(i + 1) else {
                    break;
                };
                let step = match args[i].as_str() {
                    "--cpus" => {
                        if let Ok(n) = val.parse::<u64>() {
                            body.insert("cpus".into(), n.into());
                        }
                        2
                    }
                    "--memory" => {
                        if let Ok(n) = val.parse::<u64>() {
                            body.insert("memoryMib".into(), n.into());
                        }
                        2
                    }
                    "--disk" => {
                        if let Ok(n) = val.parse::<u64>() {
                            body.insert("diskGb".into(), n.into());
                        }
                        2
                    }
                    "--iso" => {
                        body.insert("isoPath".into(), val.clone().into());
                        2
                    }
                    "--display" => {
                        body.insert("displayMode".into(), val.clone().into());
                        2
                    }
                    "--net" => {
                        body.insert("netMode".into(), val.clone().into());
                        2
                    }
                    _ => 1,
                };
                i += step;
            }
            handle_action("config", &Value::Object(body))
        }
        "-h" | "--help" | "help" => {
            println!("kiri win <status|start|stop|pause|resume|reset|hibernate|reclaim|snap [name]|restore <name>|rm-snap <name>|config [--cpus N --memory MiB --disk GB --iso path --display mode --net mode]>");
            return Ok(());
        }
        other => Err(format!("unknown kiri win command: {other}")),
    };
    match res {
        Ok(val) => {
            println!("{}", serde_json::to_string_pretty(&val).map_err(io::Error::other)?);
            Ok(())
        }
        Err(msg) => Err(io::Error::other(msg)),
    }
}

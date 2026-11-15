use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::config::{
    BYTES_PER_GIB, BYTES_PER_MIB, HIBERNATE_SNAP_NAME, ensure_disk_in, load_config_in,
    resolve_bundle_paths, shared_folders_payload, snaps_dir_in,
};

pub(super) const DEFAULT_WIDTH: u32 = 1280;
pub(super) const DEFAULT_HEIGHT: u32 = 800;
pub(super) const DEFAULT_DPI: u32 = 192;
pub(super) const DEFAULT_SCALE: f64 = 2.0;

#[derive(Debug)]
pub(super) struct WinSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    mode: String,
}

impl Drop for WinSession {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if self.is_alive() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

static SESSION: Mutex<Option<WinSession>> = Mutex::new(None);

impl WinSession {
    fn open(vm_mcp: &Path, display_mode: &str) -> Result<Self, String> {
        let mut cmd = Command::new(vm_mcp);
        if display_mode == "pip" {
            cmd.arg("--pip");
        } else {
            cmd.arg("--gui");
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| format!("Failed to launch {}: {e}", vm_mcp.display()))?;
        let stdin = child.stdin.take();
        let stdout = match child.stdout.take() {
            Some(s) => BufReader::new(s),
            None => {
                drop(stdin);
                let _ = child.kill();
                let _ = child.wait();
                return Err("missing vm-mcp stdout".into());
            }
        };
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
            mode: display_mode.to_owned(),
        };
        let _ = session.rpc_call("initialize", json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "kiri", "version": env!("CARGO_PKG_VERSION") },
        }))?;
        Ok(session)
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn rpc_call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string();
        let stdin = self.stdin.as_mut().ok_or("vm-mcp stdin already closed")?;
        writeln!(stdin, "{line}").map_err(|e| format!("write to vm-mcp failed: {e}"))?;
        stdin.flush().map_err(|e| format!("flush to vm-mcp failed: {e}"))?;

        let mut resp = String::new();
        let read = self.stdout.read_line(&mut resp).map_err(|e| format!("read from vm-mcp failed: {e}"))?;
        if read == 0 {
            return Err("vm-mcp closed the control pipe".into());
        }
        let parsed: Value = serde_json::from_str(&resp).map_err(|e| format!("invalid vm-mcp reply: {e}"))?;
        if let Some(err) = parsed.get("error") {
            return Err(err["message"].as_str().unwrap_or("vm-mcp error").to_owned());
        }
        Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
    }

    pub(super) fn tool_call(&mut self, name: &str, args: Value) -> Result<Value, String> {
        let result = self.rpc_call("tools/call", json!({ "name": name, "arguments": args }))?;
        if let Some(text) = result["content"][0]["text"].as_str() {
            return serde_json::from_str(text).map_err(|e| format!("invalid tool payload from {name}: {e}"));
        }
        Ok(result)
    }

    fn shutdown(&mut self) {
        if self.is_alive() {
            let _ = self.tool_call("vm_stop", json!({ "graceful": true }));
        }
        // Close stdin so vm-mcp observes EOF and exits its stdio JSON-RPC loop cleanly.
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(super) fn with_active_session<T>(f: impl FnOnce(&mut WinSession) -> Result<T, String>) -> Result<Option<T>, String> {
    let mut guard = SESSION.lock();
    let Some(sess) = guard.as_mut() else {
        return Ok(None);
    };
    if !sess.is_alive() {
        *guard = None;
        return Ok(None);
    }
    match f(sess) {
        Ok(val) => Ok(Some(val)),
        Err(err) => {
            if !sess.is_alive() {
                *guard = None;
            }
            Err(err)
        }
    }
}

pub(super) fn close_active_session() -> Result<(), String> {
    let mut guard = SESSION.lock();
    if let Some(mut sess) = guard.take() {
        sess.shutdown();
    }
    Ok(())
}

pub(super) fn start_vm(home: &Path) -> Result<(), String> {
    let cfg = load_config_in(home);
    let disk_path = ensure_disk_in(home, cfg.disk_gb)?;
    let bundle = resolve_bundle_paths();
    if !bundle.vm_mcp.is_file() {
        return Err(format!("vm-mcp binary not found ({}). Run buildApp first.", bundle.vm_mcp.display()));
    }

    let mut disks = vec![json!({
        "identifier": "disk0",
        "imagePath": disk_path,
        "isOptical": false,
        "isReadOnly": false,
        "sizeBytes": cfg.disk_gb * BYTES_PER_GIB,
    })];
    if !cfg.iso_path.is_empty() && Path::new(&cfg.iso_path).is_file() {
        disks.push(json!({
            "identifier": "install-iso",
            "imagePath": cfg.iso_path,
            "isOptical": true,
            "isReadOnly": true,
        }));
    }
    if bundle.guest_iso.is_file() {
        disks.push(json!({
            "identifier": "guest-tools",
            "imagePath": bundle.guest_iso,
            "isOptical": true,
            "isReadOnly": true,
        }));
    }

    let vm_cfg = json!({
        "vmName": "Windows 11 ARM64",
        "cpuCount": cfg.cpus,
        "memorySizeBytes": cfg.memory_mib * BYTES_PER_MIB,
        "displayWidth": DEFAULT_WIDTH,
        "displayHeight": DEFAULT_HEIGHT,
        "scaleFactor": DEFAULT_SCALE,
        "efiFirmwarePath": bundle.efi,
        "monitorDylibPath": bundle.monitor_dylib,
        "disks": disks,
        "sharedFolders": shared_folders_payload(&cfg),
        "adaptiveBalloonEnabled": true,
        "audioOutputEnabled": true,
        "audioInputEnabled": true,
        "useGenericVirtualization": true,
        "unattendedInstallEnabled": cfg.unattended,
        "ecoMode": cfg.eco_mode,
        "networkBridged": cfg.net_mode == "bridged",
        "networkIsolated": cfg.isolated,
    });

    let mut guard = SESSION.lock();
    if let Some(mut sess) = guard.take()
        && sess.is_alive()
    {
        if sess.mode == cfg.display_mode {
            *guard = Some(sess);
            return Ok(());
        }
        sess.shutdown();
    }

    let mut sess = WinSession::open(&bundle.vm_mcp, &cfg.display_mode)?;
    let start_res = sess.tool_call("vm_start", json!({ "config": vm_cfg }))?;
    if start_res["ok"] == false {
        let msg = start_res["errorMessage"].as_str().unwrap_or("Failed to start Windows VM");
        return Err(msg.to_owned());
    }
    let _ = sess.tool_call(
        "vm_update_display",
        json!({ "width": DEFAULT_WIDTH, "height": DEFAULT_HEIGHT, "dpi": DEFAULT_DPI, "scaleFactor": DEFAULT_SCALE }),
    );
    if cfg.display_mode == "coherence" {
        let _ = sess.tool_call("vm_set_coherence", json!({ "enabled": true }));
    }
    if cfg.eco_mode {
        let _ = sess.tool_call("vm_set_eco_mode", json!({ "enabled": true }));
    }
    if cfg.isolated {
        let _ = sess.tool_call("vm_set_network_isolation", json!({ "isolated": true }));
    }
    let hiber_state = snaps_dir_in(home).join(HIBERNATE_SNAP_NAME).join("state.vmsave");
    if hiber_state.is_file()
        && let Ok(res) = sess.tool_call("vm_restore_live_snapshot", json!({ "path": hiber_state }))
        && res["ok"] == true
    {
        let _ = fs::remove_file(&hiber_state);
        let _ = fs::remove_file(super::snapshot::frozen_snap_disk_path(&disk_path, HIBERNATE_SNAP_NAME));
    }
    *guard = Some(sess);
    Ok(())
}

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::config::{HIBERNATE_SNAP_NAME, cow_clone_file, disk_path_in, now_secs, snaps_dir_in, valid_snap_name};
use super::session::{close_active_session, with_active_session};

pub(super) const SMART_GUARD_KEEP_MAX: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotMeta {
    pub(super) name: String,
    pub(super) created_at: u64,
    pub(super) note: String,
    pub(super) auto_guard: bool,
    pub(super) live_state: bool,
    pub(super) size_bytes: u64,
}

fn frozen_clone_suffix(snap_name: &str) -> String {
    format!(".{snap_name}.snap")
}

/// 生成与 host-storage 约定的冻结磁盘 CoW 克隆路径（`<disk>.<snap_name>.snap`）。
pub(super) fn frozen_snap_disk_path(disk: &Path, snap_name: &str) -> PathBuf {
    let mut os = disk.as_os_str().to_os_string();
    os.push(frozen_clone_suffix(snap_name));
    PathBuf::from(os)
}

fn next_guard_snap_name(home: &Path) -> String {
    let base = format!("guard-{}", now_secs());
    let snaps = snaps_dir_in(home);
    if !snaps.join(&base).exists() {
        return base;
    }
    for seq in 1_u32.. {
        let candidate = format!("{base}-{seq}");
        if !snaps.join(&candidate).exists() {
            return candidate;
        }
    }
    base
}

/// @xvm(feat = FEAT_DEEP_HIBERNATE, layer = LAYER_UI, role = TAG_ROLE_UI)
pub(super) fn hibernate_vm(home: &Path) -> Result<(), String> {
    let disk = disk_path_in(home);
    let snap_dir = snaps_dir_in(home).join(HIBERNATE_SNAP_NAME);
    fs::create_dir_all(&snap_dir).map_err(|e| e.to_string())?;
    let state_path = snap_dir.join("state.vmsave");
    // 先在运行态冻结 vCPU 导出内存与一致性磁盘克隆，再复制为冷启动备用 disk.img。
    let cloned = with_active_session(|s| {
        s.tool_call(
            "vm_save_live_snapshot",
            json!({
                "path": state_path,
                "paths": [&disk],
                "cloneSuffix": frozen_clone_suffix(HIBERNATE_SNAP_NAME),
            }),
        )
    })?;
    let has_live = cloned.is_some_and(|v| v["ok"] == true) && state_path.is_file();
    let dst_disk = snap_dir.join("disk.img");
    let frozen_disk = frozen_snap_disk_path(&disk, HIBERNATE_SNAP_NAME);
    if has_live && frozen_disk.is_file() {
        cow_clone_file(&frozen_disk, &dst_disk).map_err(|e| e.to_string())?;
    } else if disk.is_file() {
        cow_clone_file(&disk, &dst_disk).map_err(|e| e.to_string())?;
    }
    let meta = SnapshotMeta {
        name: HIBERNATE_SNAP_NAME.to_owned(),
        created_at: now_secs(),
        note: "Hibernate checkpoint".to_owned(),
        auto_guard: false,
        live_state: has_live,
        size_bytes: fs::metadata(&state_path).map_or(0, |m| m.len()),
    };
    let bytes = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
    fs::write(snap_dir.join("meta.json"), bytes).map_err(|e| e.to_string())?;
    close_active_session()?;
    Ok(())
}

pub(super) fn list_snapshots_in(home: &Path) -> Vec<SnapshotMeta> {
    let dir = snaps_dir_in(home);
    let Ok(entries) = fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let meta_path = p.join("meta.json");
        if let Ok(raw) = fs::read(&meta_path)
            && let Ok(meta) = serde_json::from_slice::<SnapshotMeta>(&raw)
        {
            out.push(meta);
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.name.cmp(&b.name)));
    out
}

fn prune_smart_guard_in(home: &Path, keep: usize) {
    let disk = disk_path_in(home);
    let guards: Vec<SnapshotMeta> = list_snapshots_in(home).into_iter().filter(|m| m.auto_guard).collect();
    for old in guards.into_iter().skip(keep) {
        let _ = fs::remove_dir_all(snaps_dir_in(home).join(&old.name));
        let _ = fs::remove_file(frozen_snap_disk_path(&disk, &old.name));
    }
}

/// @xvm(feat = FEAT_SMART_GUARD, layer = LAYER_UI, role = TAG_ROLE_UI)
pub(super) fn run_smart_guard_snapshot(home: &Path, name: Option<&str>, note: &str, auto_guard: bool) -> Result<SnapshotMeta, String> {
    let snap_name = match name.filter(|s| !s.trim().is_empty()) {
        Some(n) => valid_snap_name(n)?.to_owned(),
        None => next_guard_snap_name(home),
    };
    let snap_dir = snaps_dir_in(home).join(&snap_name);
    fs::create_dir_all(&snap_dir).map_err(|e| e.to_string())?;
    let disk = disk_path_in(home);
    let state_path = snap_dir.join("state.vmsave");
    let live_res = with_active_session(|s| {
        s.tool_call(
            "vm_save_live_snapshot",
            json!({ "path": state_path, "paths": [&disk], "cloneSuffix": frozen_clone_suffix(&snap_name) }),
        )
    })
    .ok()
    .flatten();
    let live_state = live_res.is_some_and(|v| v["ok"] == true) && state_path.is_file();
    let dst_disk = snap_dir.join("disk.img");
    let frozen_disk = frozen_snap_disk_path(&disk, &snap_name);
    if live_state && frozen_disk.is_file() {
        cow_clone_file(&frozen_disk, &dst_disk).map_err(|e| format!("Failed to clone disk: {e}"))?;
    } else if disk.is_file() {
        cow_clone_file(&disk, &dst_disk).map_err(|e| format!("Failed to clone disk: {e}"))?;
    } else {
        File::create(&dst_disk).map_err(|e| e.to_string())?;
    }
    let size_bytes = fs::metadata(&dst_disk).map_or(0, |m| m.len()) + fs::metadata(&state_path).map_or(0, |m| m.len());
    let meta = SnapshotMeta {
        name: snap_name,
        created_at: now_secs(),
        note: note.to_owned(),
        auto_guard,
        live_state,
        size_bytes,
    };
    let bytes = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
    fs::write(snap_dir.join("meta.json"), bytes).map_err(|e| e.to_string())?;
    if auto_guard {
        prune_smart_guard_in(home, SMART_GUARD_KEEP_MAX);
    }
    Ok(meta)
}

pub(super) fn restore_snapshot_in(home: &Path, name: &str) -> Result<(), String> {
    let snap_name = valid_snap_name(name)?;
    let snap_dir = snaps_dir_in(home).join(snap_name);
    if !snap_dir.is_dir() {
        return Err(format!("Snapshot {snap_name} not found."));
    }
    let state_path = snap_dir.join("state.vmsave");
    if state_path.is_file()
        && let Ok(Some(res)) = with_active_session(|s| s.tool_call("vm_restore_live_snapshot", json!({ "path": state_path })))
        && res["ok"] == true
    {
        return Ok(());
    }
    close_active_session()?;
    let snap_disk = snap_dir.join("disk.img");
    let disk = disk_path_in(home);
    if snap_disk.is_file() {
        cow_clone_file(&snap_disk, &disk).map_err(|e| format!("Failed to restore disk: {e}"))?;
    }
    // 仅在恢复其他快照时清理休眠状态，避免冷恢复休眠快照自身时误删休眠现场。
    if snap_name != HIBERNATE_SNAP_NAME {
        let _ = fs::remove_file(snaps_dir_in(home).join(HIBERNATE_SNAP_NAME).join("state.vmsave"));
        let _ = fs::remove_file(frozen_snap_disk_path(&disk, HIBERNATE_SNAP_NAME));
    }
    Ok(())
}

pub(super) fn delete_snapshot_in(home: &Path, name: &str) -> Result<(), String> {
    let snap_name = valid_snap_name(name)?;
    let snap_dir = snaps_dir_in(home).join(snap_name);
    if !snap_dir.is_dir() {
        return Err(format!("Snapshot {snap_name} not found."));
    }
    fs::remove_dir_all(&snap_dir).map_err(|e| e.to_string())?;
    let _ = fs::remove_file(frozen_snap_disk_path(&disk_path_in(home), snap_name));
    Ok(())
}

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

use parking_lot::Mutex;
use serde_json::json;

use super::actions::handle_action;
use super::config::{BYTES_PER_GIB, HIBERNATE_SNAP_NAME, WinConfig, disk_path_in, ensure_disk_in, load_config_in, snaps_dir_in, update_config_in};
use super::snapshot::{SMART_GUARD_KEEP_MAX, delete_snapshot_in, frozen_snap_disk_path, list_snapshots_in, restore_snapshot_in, run_smart_guard_snapshot};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn temp_home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kiri-winvm-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn config_roundtrip_and_clamping() {
    let home = temp_home("cfg");
    let cfg = WinConfig {
        cpus: 6,
        memory_mib: 8192,
        disk_gb: 80,
        iso_path: "/tmp/test-win11.iso".into(),
        display_mode: "coherence".into(),
        net_mode: "bridged".into(),
        eco_mode: true,
        ..WinConfig::default()
    };
    update_config_in(&home, |c| *c = cfg.clone()).unwrap();
    let loaded = load_config_in(&home);
    assert_eq!(loaded, cfg);

    let extreme = WinConfig { cpus: 99, memory_mib: 512, disk_gb: 4, ..WinConfig::default() };
    update_config_in(&home, |c| *c = extreme).unwrap();
    let clamped = load_config_in(&home);
    assert_eq!(clamped.cpus, 16);
    assert_eq!(clamped.memory_mib, 2048);
    assert_eq!(clamped.disk_gb, 16);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn sparse_disk_creation_and_cow_snapshot_lifecycle() {
    let home = temp_home("snap");
    let disk = ensure_disk_in(&home, 16).unwrap();
    assert_eq!(fs::metadata(&disk).unwrap().len(), 16 * BYTES_PER_GIB);

    // 写入标记扇区后创建快照 s1，修改磁盘并恢复，校验内容回滚与冷清理。
    let mut f = OpenOptions::new().read(true).write(true).open(&disk).unwrap();
    f.write_all(b"KIRI-WIN-TEST-01").unwrap();
    drop(f);

    let meta = run_smart_guard_snapshot(&home, Some("s1"), "baseline", false).unwrap();
    assert_eq!(meta.name, "s1");
    let frozen_stub = frozen_snap_disk_path(&disk, "s1");
    fs::write(&frozen_stub, b"stub").unwrap();

    let mut f = OpenOptions::new().read(true).write(true).open(&disk).unwrap();
    f.write_all(b"MUTATED-DATA-999").unwrap();
    drop(f);

    restore_snapshot_in(&home, "s1").unwrap();
    let restored = OpenOptions::new().read(true).open(&disk).unwrap();
    let mut buf = [0u8; 16];
    io::Read::read_exact(&mut io::BufReader::new(restored), &mut buf).unwrap();
    assert_eq!(&buf, b"KIRI-WIN-TEST-01");

    delete_snapshot_in(&home, "s1").unwrap();
    assert!(!frozen_stub.exists());
    assert!(list_snapshots_in(&home).is_empty());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn smart_guard_rotates_and_disambiguates_same_second() {
    let home = temp_home("guard");
    ensure_disk_in(&home, 16).unwrap();
    let first = run_smart_guard_snapshot(&home, None, "auto-1", false).unwrap();
    let second = run_smart_guard_snapshot(&home, None, "auto-2", false).unwrap();
    assert_ne!(first.name, second.name);

    // 验证冷恢复 hibernate 自身时不误删休眠状态文件。
    let hiber_dir = snaps_dir_in(&home).join(HIBERNATE_SNAP_NAME);
    fs::create_dir_all(&hiber_dir).unwrap();
    let hiber_state = hiber_dir.join("state.vmsave");
    fs::write(&hiber_state, b"invalid-or-offline").unwrap();
    fs::write(hiber_dir.join("disk.img"), b"disk").unwrap();
    let _ = fs::remove_file(disk_path_in(&home));
    restore_snapshot_in(&home, HIBERNATE_SNAP_NAME).unwrap();
    assert!(hiber_state.is_file());

    for i in 0..7 {
        run_smart_guard_snapshot(&home, Some(&format!("g-{i}")), "", true).unwrap();
    }
    let guards: Vec<_> = list_snapshots_in(&home).into_iter().filter(|m| m.auto_guard).collect();
    assert_eq!(guards.len(), SMART_GUARD_KEEP_MAX);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn mode_toggles_and_env_override_status_json() {
    let _guard = ENV_LOCK.lock();
    let home = temp_home("env");
    // SAFETY: 进程级环境变量读写已在 ENV_LOCK 下串行化，测试结束后立即清除。
    unsafe { std::env::set_var("KIRI_WIN_HOME", &home) };
    let after_eco = handle_action("eco", &json!({ "enabled": true })).unwrap();
    assert_eq!(after_eco["config"]["ecoMode"], true);
    let after_vmnet = handle_action("vmnet", &json!({ "mode": "bridged", "isolated": true })).unwrap();
    assert_eq!(after_vmnet["config"]["netMode"], "bridged");
    assert_eq!(after_vmnet["config"]["isolated"], true);
    let after_coh = handle_action("coherence", &json!({ "enabled": true })).unwrap();
    assert_eq!(after_coh["config"]["displayMode"], "coherence");
    // SAFETY: 恢复测试前环境变量状态。
    unsafe { std::env::remove_var("KIRI_WIN_HOME") };
    let _ = fs::remove_dir_all(&home);
}

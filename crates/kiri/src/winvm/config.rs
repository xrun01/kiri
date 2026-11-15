use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(super) const BYTES_PER_MIB: u64 = 1024 * 1024;
pub(super) const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;
const DEFAULT_CPUS: u32 = 4;
const DEFAULT_MEMORY_MIB: u64 = 4096;
const DEFAULT_DISK_GB: u64 = 64;
pub(super) const HIBERNATE_SNAP_NAME: &str = "hibernate";
pub(super) const RECLAIM_TARGET_BYTES: u64 = 512 * BYTES_PER_MIB;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SharedFolderEntry {
    pub(crate) share_name: String,
    pub(crate) host_path: String,
    pub(crate) read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct WinConfig {
    pub(crate) cpus: u32,
    pub(crate) memory_mib: u64,
    pub(crate) disk_gb: u64,
    pub(crate) iso_path: String,
    pub(crate) display_mode: String,
    pub(crate) net_mode: String,
    pub(crate) eco_mode: bool,
    pub(crate) auto_pause: bool,
    pub(crate) isolated: bool,
    pub(crate) smart_guard: bool,
    pub(crate) smart_keymap: bool,
    pub(crate) clipboard_sync: bool,
    pub(crate) unattended: bool,
    pub(crate) share_desktop: bool,
    pub(crate) share_documents: bool,
    pub(crate) share_downloads: bool,
    pub(crate) custom_shares: Vec<SharedFolderEntry>,
}

impl Default for WinConfig {
    fn default() -> Self {
        Self {
            cpus: DEFAULT_CPUS,
            memory_mib: DEFAULT_MEMORY_MIB,
            disk_gb: DEFAULT_DISK_GB,
            iso_path: String::new(),
            display_mode: "desktop".to_owned(),
            net_mode: "shared".to_owned(),
            eco_mode: false,
            auto_pause: true,
            isolated: false,
            smart_guard: true,
            smart_keymap: true,
            clipboard_sync: true,
            unattended: false,
            share_desktop: true,
            share_documents: true,
            share_downloads: true,
            custom_shares: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct BundlePaths {
    pub(super) vm_mcp: PathBuf,
    pub(super) efi: PathBuf,
    pub(super) guest_iso: PathBuf,
    pub(super) monitor_dylib: PathBuf,
}

static CONFIG_LOCK: Mutex<()> = Mutex::new(());
static CONFIG_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub(crate) fn win_home() -> PathBuf {
    if let Some(custom) = std::env::var_os("KIRI_WIN_HOME").filter(|v| !v.is_empty()) {
        return PathBuf::from(custom);
    }
    user_home().join(".kiri/windows")
}

pub(super) fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub(super) fn valid_snap_name(name: &str) -> Result<&str, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err("Snapshot name must be 1 to 64 characters.".into());
    }
    if !trimmed.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) || trimmed.starts_with('.') {
        return Err("Snapshot name may use letters, digits, '-', '_' and '.' only.".into());
    }
    Ok(trimmed)
}

fn config_path_in(home: &Path) -> PathBuf {
    home.join("config.json")
}

pub(super) fn disk_path_in(home: &Path) -> PathBuf {
    home.join("system.img")
}

pub(super) fn snaps_dir_in(home: &Path) -> PathBuf {
    home.join("snapshots")
}

fn load_config_unlocked(home: &Path) -> WinConfig {
    let path = config_path_in(home);
    let mut cfg: WinConfig = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if cfg.iso_path.is_empty()
        && let Some(iso) = detect_windows_iso(home)
    {
        cfg.iso_path = iso.display().to_string();
    }
    cfg.cpus = cfg.cpus.clamp(1, 16);
    cfg.memory_mib = cfg.memory_mib.clamp(2048, 65536);
    cfg.disk_gb = cfg.disk_gb.clamp(16, 1024);
    cfg
}

pub(super) fn load_config_in(home: &Path) -> WinConfig {
    let _guard = CONFIG_LOCK.lock();
    load_config_unlocked(home)
}

fn save_config_unlocked(home: &Path, cfg: &WinConfig) -> Result<(), String> {
    fs::create_dir_all(home).map_err(|e| format!("Failed to create {}: {e}", home.display()))?;
    let path = config_path_in(home);
    let seq = CONFIG_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = home.join(format!("config.json.tmp.{}.{seq}", std::process::id()));
    let bytes = serde_json::to_vec_pretty(cfg).map_err(|e| e.to_string())?;
    fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

pub(super) fn update_config_in(
    home: &Path,
    mutate: impl FnOnce(&mut WinConfig),
) -> Result<WinConfig, String> {
    let _guard = CONFIG_LOCK.lock();
    let mut cfg = load_config_unlocked(home);
    mutate(&mut cfg);
    save_config_unlocked(home, &cfg)?;
    Ok(cfg)
}

pub(super) fn ensure_disk_in(home: &Path, disk_gb: u64) -> Result<PathBuf, String> {
    fs::create_dir_all(home).map_err(|e| e.to_string())?;
    let path = disk_path_in(home);
    let target_bytes = disk_gb * BYTES_PER_GIB;
    if !path.exists() {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
        f.set_len(target_bytes).map_err(|e| e.to_string())?;
        return Ok(path);
    }
    let cur = fs::metadata(&path).map_err(|e| e.to_string())?.len();
    if target_bytes > cur {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| e.to_string())?;
        f.set_len(target_bytes).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn find_first_iso(dir: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut iso_li: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("iso")))
        .collect();
    iso_li.sort();
    // Prefer an image with both win11 and arm64 hints in its filename.
    iso_li
        .iter()
        .find(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
            n.contains("win11") && n.contains("arm64")
        })
        .cloned()
        .or_else(|| {
            iso_li
                .iter()
                .find(|p| {
                    let n = p.file_name().unwrap_or_default().to_string_lossy().to_ascii_lowercase();
                    n.contains("win") && n.contains("arm")
                })
                .cloned()
        })
        .or_else(|| iso_li.into_iter().next())
}

fn push_iso_root_candidates(root: &Path, candidates: &mut Vec<PathBuf>) {
    for anc in root.ancestors() {
        let win_iso = anc.join("git/win/iso");
        if win_iso.is_dir() {
            candidates.push(win_iso);
            break;
        }
        let local_win_iso = anc.join("win/iso");
        if local_win_iso.is_dir() {
            candidates.push(local_win_iso);
            break;
        }
    }
}

fn detect_windows_iso(home: &Path) -> Option<PathBuf> {
    let mut candidates = vec![home.to_path_buf(), user_home().join("Downloads")];
    if let Ok(exe) = std::env::current_exe() {
        push_iso_root_candidates(&exe, &mut candidates);
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_iso_root_candidates(&cwd, &mut candidates);
    }
    candidates.into_iter().find_map(|d| find_first_iso(&d))
}

fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.is_file()).cloned()
}

fn push_repo_bundle_candidates(
    root: &Path,
    mcp_cands: &mut Vec<PathBuf>,
    efi_cands: &mut Vec<PathBuf>,
    iso_cands: &mut Vec<PathBuf>,
    mon_cands: &mut Vec<PathBuf>,
) {
    for anc in root.ancestors() {
        let vm_dir = anc.join("git/vm");
        let candidate_vm_dir = if vm_dir.is_dir() {
            Some(vm_dir)
        } else if anc.join("rs").is_dir() && anc.join("engine").is_dir() {
            Some(anc.to_path_buf())
        } else {
            None
        };
        if let Some(vm_dir) = candidate_vm_dir {
            let dist_dir = vm_dir.join("engine/build/dist");
            mcp_cands.push(dist_dir.join("vm-mcp"));
            mcp_cands.push(vm_dir.join("rs/target/release/vm-mcp"));
            mcp_cands.push(vm_dir.join("rs/target/debug/vm-mcp"));
            efi_cands.push(dist_dir.join("efia64.bin"));
            efi_cands.push(vm_dir.join("rs/mon-firmware/res/efia64.bin"));
            iso_cands.push(dist_dir.join("GuestTools.iso"));
            iso_cands.push(vm_dir.join("engine/build/GuestTools/GuestTools.iso"));
            mon_cands.push(dist_dir.join("libMonitorArm.dylib"));
            mon_cands.push(vm_dir.join("rs/target/release/libmon_session.dylib"));
            mon_cands.push(vm_dir.join("rs/target/debug/libmon_session.dylib"));
            break;
        }
    }
}

pub(super) fn resolve_bundle_paths() -> BundlePaths {
    let mut mcp_cands = Vec::new();
    let mut efi_cands = Vec::new();
    let mut iso_cands = Vec::new();
    let mut mon_cands = Vec::new();

    if let Some(p) = std::env::var_os("KIRI_VM_MCP").filter(|v| !v.is_empty()) {
        mcp_cands.push(PathBuf::from(p));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(macos_dir) = exe.parent() {
            mcp_cands.push(macos_dir.join("vm-mcp"));
            if let Some(contents_dir) = macos_dir.parent() {
                efi_cands.push(contents_dir.join("Resources/efia64.bin"));
                iso_cands.push(contents_dir.join("Resources/GuestTools.iso"));
                mon_cands.push(contents_dir.join("Frameworks/libMonitorArm.dylib"));
                mon_cands.push(macos_dir.join("libMonitorArm.dylib"));
            }
        }
        push_repo_bundle_candidates(&exe, &mut mcp_cands, &mut efi_cands, &mut iso_cands, &mut mon_cands);
    }
    if let Ok(cwd) = std::env::current_dir() {
        push_repo_bundle_candidates(&cwd, &mut mcp_cands, &mut efi_cands, &mut iso_cands, &mut mon_cands);
    }

    BundlePaths {
        vm_mcp: first_existing(&mcp_cands).unwrap_or_else(|| PathBuf::from("vm-mcp")),
        efi: first_existing(&efi_cands).unwrap_or_else(|| PathBuf::from("efia64.bin")),
        guest_iso: first_existing(&iso_cands).unwrap_or_else(|| PathBuf::from("GuestTools.iso")),
        monitor_dylib: first_existing(&mon_cands).unwrap_or_else(|| PathBuf::from("libMonitorArm.dylib")),
    }
}

/// Copy-on-write clone on APFS (`clonefile`), falling back to regular file copy.
pub(super) fn cow_clone_file(src: &Path, dst: &Path) -> io::Result<()> {
    if dst.exists() {
        let _ = fs::remove_file(dst);
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let c_src = CString::new(src.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid source path"))?;
        let c_dst = CString::new(dst.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid target path"))?;
        // SAFETY: both pointers are valid NUL-terminated C strings passed to BSD clonefile.
        let rc = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
        if rc == 0 {
            return Ok(());
        }
    }
    fs::copy(src, dst)?;
    Ok(())
}

/// @xvm(feat = FEAT_SHARED_PROFILE, layer = LAYER_UI, role = TAG_ROLE_UI)
fn shared_folder_entries(cfg: &WinConfig) -> Vec<SharedFolderEntry> {
    let home = user_home();
    let mut entries = Vec::new();
    let profiles = [
        (cfg.share_desktop, "Desktop", "Desktop"),
        (cfg.share_documents, "Documents", "Documents"),
        (cfg.share_downloads, "Downloads", "Downloads"),
    ];
    for (enabled, dir_name, share_name) in profiles {
        if enabled {
            let path = home.join(dir_name);
            if path.is_dir() {
                entries.push(SharedFolderEntry {
                    share_name: share_name.to_owned(),
                    host_path: path.display().to_string(),
                    read_only: false,
                });
            }
        }
    }
    for custom in &cfg.custom_shares {
        if !custom.share_name.trim().is_empty() && Path::new(&custom.host_path).is_dir() {
            entries.push(custom.clone());
        }
    }
    entries
}

pub(super) fn shared_folders_payload(cfg: &WinConfig) -> Value {
    let items: Vec<Value> = shared_folder_entries(cfg)
        .into_iter()
        .map(|s| {
            json!({
                "shareName": s.share_name,
                "hostPath": s.host_path,
                "isReadOnly": s.read_only,
            })
        })
        .collect();
    Value::Array(items)
}

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::engine::trf;

use super::{CANCEL, Pin, dir, quiet, task, update, verified};

/// Bytes in MB as the page shows them (1 MB = 1,000,000 bytes, rounded),
/// so a task's "4 / 368 MB" agrees with the bar's "4 of 368 MB".
fn mb(bytes: u64) -> u64 {
    (bytes + 500_000) / 1_000_000
}

/// Downloads `pin` into %LOCALAPPDATA%\Kiri\downloads with curl.exe (part
/// of Windows), showing bytes, speed and time left, then checks its SHA-256.
pub(super) fn download(pin: &Pin, task_index: usize) -> io::Result<PathBuf> {
    let dl = dir().join("downloads");
    std::fs::create_dir_all(&dl)?;
    let dest = dl.join(pin.file);
    if dest.is_file() && sha256(&dest)? == pin.sha256 {
        task(task_index, "done", &trf("{size} MB · already here", &[("size", &mb(pin.size))]));
        return Ok(dest);
    }
    let part = dl.join(format!("{}.part", pin.file));
    let _ = std::fs::remove_file(&part);
    let label = trf("Downloading {name} from {source}", &[("name", &pin.name), ("source", &if pin.url.contains("github.com/microsoft") { "Microsoft" } else { "ubuntu.com" })]);
    task(task_index, "active", &format!("0 / {} MB", mb(pin.size)));
    let mut child = quiet("curl.exe").args(["-fsSL", "--retry", "3", "-o"]).arg(&part).arg(pin.url).spawn()?;
    let (start, mut last, mut rate) = (Instant::now(), (Instant::now(), 0u64), 0f64);
    loop {
        if CANCEL.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = std::fs::remove_file(&part);
            return Err(io::Error::other("cancelled"));
        }
        let finished = child.try_wait()?;
        let got = std::fs::metadata(&part).map_or(0, |m| m.len());
        let dt = last.0.elapsed().as_secs_f64();
        if dt >= 0.25 {
            let now = (got - last.1.min(got)) as f64 / dt;
            rate = if rate == 0.0 { now } else { rate * 0.7 + now * 0.3 };
            last = (Instant::now(), got);
        }
        let eta = if rate > 0.0 { ((pin.size.saturating_sub(got)) as f64 / rate).ceil() as u64 } else { 0 };
        update(|s| {
            s.progress = Some(json!({"pct": got * 100 / pin.size.max(1), "done": got, "total": pin.size, "rate": rate as u64, "eta": eta, "label": label}));
            if let Some(t) = s.tasks.get_mut(task_index) {
                t.detail = format!("{} / {} MB", mb(got), mb(pin.size));
            }
        });
        if let Some(status) = finished {
            if !status.success() {
                return Err(io::Error::other(format!("downloading {} failed: check the internet connection", pin.name)));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    let got = sha256(&part)?;
    if got != pin.sha256 {
        let _ = std::fs::remove_file(&part);
        return Err(io::Error::other(format!("{} did not match its SHA-256 (got {got})", pin.name)));
    }
    std::fs::rename(&part, &dest)?;
    task(task_index, "done", &trf("{size} MB in {secs} s", &[("size", &mb(pin.size)), ("secs", &start.elapsed().as_secs())]));
    Ok(dest)
}

pub(crate) fn sha256(path: &Path) -> io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Runs kiri-admin.ps1 (contrib/windows, installed beside this exe) with
/// `args` in an elevated PowerShell (Windows asks for approval) and returns
/// its exit code; a refusal is "cancelled". A script file checked against
/// this build's SHA-256, not script text in this binary run with
/// -EncodedCommand: an elevated, hidden PowerShell fed an encoded script is
/// what malware droppers do, and Defender quarantined kiri-win.exe 0.1.7 as
/// Trojan:Win32/Bearfoos.A!ml for carrying it.
pub(super) fn elevated(args: &[&str]) -> io::Result<i32> {
    let script = verified("kiri-admin.ps1")?;
    match crate::win::elevated_script(&script, args) {
        // As before: anything that kept it from starting reads as declined.
        Err(e) => {
            crate::serve::log(&format!("setup: the elevated PowerShell did not start ({e})"));
            Err(io::Error::other("cancelled: Windows' approval was declined"))
        }
        Ok(1223) => Err(io::Error::other("cancelled: Windows' approval was declined")),
        Ok(c) => Ok(c as i32),
    }
}

//! Rule 11: Time Machine's local snapshots, listed with tmutil, for review.

use super::{Sink, TOOL_LOCK, run_tool_timeout};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const TMUTIL: &str = "/usr/bin/tmutil";
const PREFIX: &str = "com.apple.TimeMachine.";

/// The boot volume and every writable local APFS volume under /Volumes.
fn volumes() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/")];
    if let Ok(rd) = std::fs::read_dir("/Volumes") {
        for e in rd.flatten() {
            let p = e.path();
            if is_local_apfs(&p) {
                v.push(p);
            }
        }
    }
    v
}

fn is_local_apfs(p: &Path) -> bool {
    let Ok(c) = crate::fsattr::cpath(p) else { return false };
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    let ty = unsafe { std::ffi::CStr::from_ptr(st.f_fstypename.as_ptr()) }.to_string_lossy().into_owned();
    let mount = unsafe { std::ffi::CStr::from_ptr(st.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
    let flags = st.f_flags;
    ty == "apfs"
        && Path::new(&mount) == p
        && flags & libc::MNT_RDONLY as u32 == 0
        && flags & libc::MNT_LOCAL as u32 != 0
}

pub fn discover(_ctx: &Ctx, sink: &dyn Sink) {
    if !Path::new(TMUTIL).exists() {
        return;
    }
    for vol in volumes() {
        let mut cmd = Command::new(TMUTIL);
        cmd.arg("listlocalsnapshots").arg(&vol);
        match run_tool_timeout(&mut cmd, Duration::from_secs(60)) {
            Ok(out) => {
                for name in parse(&out) {
                    let label = format!("{} on {}", date_of(&name), vol.display());
                    sink.item(Kind::Snapshot, vol.clone(), label, Target::Snapshot { volume: vol.clone(), name });
                }
            }
            Err(e) => sink.note(Kind::Snapshot, format!("tmutil couldn't list {}: {e}", vol.display())),
        }
    }
}

pub fn parse(out: &str) -> Vec<String> {
    out.lines().map(str::trim).filter(|l| l.starts_with(PREFIX)).map(String::from).collect()
}

/// `com.apple.TimeMachine.2026-10-12-093012.local` → `2026-10-12-093012`.
fn date_of(name: &str) -> &str {
    name.strip_prefix(PREFIX).and_then(|s| s.strip_suffix(".local")).unwrap_or(name)
}

pub fn evaluate(volume: &Path, name: &str) -> Verdict {
    let mut v = Verdict::new(Badge::Review, "Time Machine's local copy; macOS also thins these when space runs low");
    v.checks.push(Check::info(format!("on {}", volume.display())));
    v.checks.push(Check::info("APFS doesn't report how much a snapshot holds, so its size is unknown"));
    v.checks.push(Check::info(format!("snapshot {name}")));
    v
}

pub fn delete(volume: &Path, name: &str) -> Result<String> {
    let _g = TOOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cmd = Command::new(TMUTIL);
    cmd.arg("deletelocalsnapshots").arg(date_of(name));
    run_tool_timeout(&mut cmd, Duration::from_secs(300))
        .map_err(|e| e.context(format!("deleting {name} on {}", volume.display())))?;
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tmutil() {
        let out = "Snapshots for disk /:\ncom.apple.TimeMachine.2026-10-12-093012.local\ncom.apple.TimeMachine.2026-10-13-101500.local\n";
        let names = parse(out);
        assert_eq!(names.len(), 2);
        assert_eq!(date_of(&names[0]), "2026-10-12-093012");
        assert!(parse("Snapshots for disk /:\n").is_empty());
        let v = evaluate(Path::new("/"), &names[0]);
        assert_eq!(v.badge, Badge::Review);
        assert_eq!(v.bytes, None);
    }
}

//! Rule 7: files in ~/Library/Logs not written for a week. One item per
//! top-level entry; deleting a folder's item removes only its old files.

use super::{Found, Sink, gone, not_in_use, readable};
use crate::ctx::Ctx;
use crate::fsattr::{self, ObjKind};
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk::{self, Opts};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn root(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Logs")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![root(ctx)]
}

pub fn entry(ctx: &Ctx, path: &Path) -> Option<Found> {
    if path.parent()? != root(ctx) {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    // sweep's own record of what it deleted stays.
    if name == "sweep" || name == ".DS_Store" {
        return None;
    }
    Some((Kind::Logs, path.to_path_buf(), name, Target::Logs))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let root = root(ctx);
    let Ok(entries) = fsattr::read_dir_quick(&root) else { return };
    for e in entries {
        if let Some((kind, path, label, target)) = entry(ctx, &root.join(&e.name)) {
            sink.item(kind, path, label, target);
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let days = ctx.cfg.thresholds.logs_days;
    let cutoff = ctx.cutoff(days);
    let s = walk::measure(path, &Opts { old_before: Some(cutoff), ..ctx.walk_opts() });
    let mut v = Verdict::new(Badge::Safe, "");
    v.bytes = Some(s.old_freed);
    v.last_write = s.last_write();
    if s.old_files == 0 {
        v.hidden = true;
        v.keep(format!("nothing older than {days} days"));
        return v;
    }
    let total = s.files;
    v.checks.push(Check::pass(format!(
        "{} of {} not written for {days} days",
        crate::fmt::count(s.old_files as usize, "file", "files"),
        total
    )));
    not_in_use(ctx, &mut v, path);
    readable(&mut v, &s);
    if v.badge == Badge::Safe {
        v.reason = if s.files == 1 && s.dirs == 0 {
            format!("not written for {days} days")
        } else {
            format!("{} older than {days} days", crate::fmt::count(s.old_files as usize, "log file", "log files"))
        };
    }
    v
}

/// Removes the files not written since the cutoff, judged afresh.
pub fn delete(ctx: &Ctx, path: &Path) -> Result<String> {
    let cutoff = ctx.cutoff(ctx.cfg.thresholds.logs_days);
    let a = fsattr::attrs(path)?;
    if a.kind != ObjKind::Dir {
        if a.last_write() < cutoff {
            std::fs::remove_file(path)?;
        }
        return Ok(String::new());
    }
    let mut removed = 0usize;
    let mut first_err = None;
    remove_old(path, a.dev, cutoff, &mut removed, &mut first_err);
    if let Some(e) = first_err {
        if removed == 0 {
            anyhow::bail!(e);
        }
        return Ok(format!("{} removed; some couldn't be: {e}", crate::fmt::count(removed, "file", "files")));
    }
    Ok(format!("{} removed", crate::fmt::count(removed, "file", "files")))
}

fn remove_old(dir: &Path, dev: i32, cutoff: std::time::SystemTime, removed: &mut usize, err: &mut Option<String>) {
    let entries = match fsattr::read_dir_quick(dir) {
        Ok(e) => e,
        Err(e) => {
            err.get_or_insert_with(|| format!("{}: {e}", dir.display()));
            return;
        }
    };
    for e in entries {
        let p = dir.join(&e.name);
        if e.attrs.kind == ObjKind::Dir {
            if e.attrs.dev == dev {
                remove_old(&p, dev, cutoff, removed, err);
            }
        } else if e.attrs.last_write() < cutoff {
            match std::fs::remove_file(&p) {
                Ok(()) => *removed += 1,
                Err(e) => {
                    err.get_or_insert_with(|| format!("{}: {e}", p.display()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::testing::{badge, find, scan};
    use crate::rules::{DeleteOpts, delete as delete_item, evaluate as eval_item};
    use crate::testutil::{set_mtime, write};
    use std::time::{Duration, SystemTime};

    #[test]
    fn old_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let logs = home.join("Library/Logs");
        write(&logs.join("DiagnosticReports/a.ips"), "old");
        write(&logs.join("DiagnosticReports/b.ips"), "new");
        write(&logs.join("Fresh/today.log"), "new");
        write(&logs.join("SparkleUpdateLog.log"), "old");
        write(&logs.join("sweep/2026-10-01.log"), "old");
        let now = SystemTime::now() + Duration::from_secs(30 * 86_400);
        set_mtime(&logs.join("DiagnosticReports/b.ips"), now - Duration::from_secs(86_400));
        set_mtime(&logs.join("Fresh/today.log"), now - Duration::from_secs(3600));
        let ctx = crate::ctx::test_ctx(&home, now);

        let (items, _) = scan(&ctx, discover);
        assert!(items.iter().all(|i| i.label != "sweep"), "sweep keeps its own logs");
        let dr = find(&items, "DiagnosticReports");
        assert_eq!(dr.badge(), Some(Badge::Safe));
        assert_eq!(dr.verdict.as_ref().unwrap().reason, "1 log file older than 7 days");
        assert!(find(&items, "Fresh").verdict.as_ref().unwrap().hidden);
        assert_eq!(badge(&items, "SparkleUpdateLog.log"), Badge::Safe);

        let v = eval_item(&ctx, dr);
        delete_item(&ctx, dr, &v, DeleteOpts::default()).unwrap();
        assert!(!logs.join("DiagnosticReports/a.ips").exists());
        assert!(logs.join("DiagnosticReports/b.ips").exists());
        assert!(logs.join("DiagnosticReports").is_dir());
    }
}

//! Rule 10: big, old files in ~/Downloads, for review.

use super::{Sink, gone, not_in_use};
use crate::ctx::Ctx;
use crate::fsattr::{self, ObjKind};
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::watch::Change;
use std::path::{Path, PathBuf};

/// Folders that are documents in their own right; never picked apart.
const PACKAGES: [&str; 9] =
    ["app", "bundle", "framework", "photoslibrary", "fcpbundle", "logicx", "xcarchive", "pkg", "dmg"];

pub fn root(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Downloads")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![root(ctx)]
}

/// Discovery looks through every folder in Downloads. Only files already a
/// month old count, so new downloads don't matter until then.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    c.entries && c.path.starts_with(root(ctx))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let root = root(ctx);
    if let Err(e) = std::fs::read_dir(&root) {
        if e.kind() == std::io::ErrorKind::PermissionDenied || e.raw_os_error() == Some(libc::EPERM) {
            sink.note(Kind::Downloads, "macOS didn't allow reading ~/Downloads; give your terminal access in System Settings → Privacy & Security → Files and Folders".into());
        }
        return;
    }
    let t = &ctx.cfg.thresholds;
    let cutoff = ctx.cutoff(t.downloads_days);
    let min = t.downloads_min_mb * 1_000_000;
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fsattr::read_dir(&dir) else { continue };
        for e in entries {
            let p = dir.join(&e.name);
            match e.attrs.kind {
                ObjKind::Dir => {
                    let pkg =
                        p.extension().is_some_and(|x| PACKAGES.contains(&x.to_string_lossy().to_lowercase().as_str()));
                    if !pkg {
                        stack.push(p);
                    }
                }
                ObjKind::File if e.attrs.freed() >= min && e.attrs.last_write() < cutoff => {
                    let label = p.strip_prefix(&root).unwrap_or(&p).display().to_string();
                    sink.item(Kind::Downloads, p, label, Target::Download);
                }
                _ => {}
            }
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Review, "");
    let a = match fsattr::attrs(path) {
        Ok(a) => a,
        Err(e) => {
            v.keep(format!("couldn't read it: {e}"));
            return v;
        }
    };
    v.bytes = Some(a.freed());
    v.du = Some(a.alloc);
    v.last_write = Some(a.last_write());
    let ago = ctx.ago(a.last_write());
    if ctx.age_days(a.last_write()) < ctx.cfg.thresholds.downloads_days {
        v.hidden = true;
        v.keep(format!("written {ago}"));
        return v;
    }
    v.checks.push(Check::pass(format!("downloaded or changed {ago}")));
    not_in_use(ctx, &mut v, path);
    if v.badge == Badge::Review {
        v.reason = format!("downloaded {ago}; you can download it again");
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::testing::{find, scan};
    use crate::testutil::{set_mtime, write_random};
    use std::time::{Duration, SystemTime};

    #[test]
    fn big_and_old_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let d = home.join("Downloads");
        write_random(&d.join("Xcode_26.xip"), 3_000_000);
        write_random(&d.join("small.zip"), 100_000);
        write_random(&d.join("deep/folder/dataset.tar"), 2_000_000);
        write_random(&d.join("Some.app/Contents/big"), 3_000_000);
        write_random(&d.join("recent.dmg"), 3_000_000);
        let now = SystemTime::now() + Duration::from_secs(45 * 86_400);
        set_mtime(&d.join("recent.dmg"), now - Duration::from_secs(86_400));
        let mut ctx = crate::ctx::test_ctx(&home, now);
        ctx.cfg.thresholds.downloads_min_mb = 1;
        let (items, _) = scan(&ctx, discover);
        let mut labels: Vec<_> = items.iter().map(|i| i.label.clone()).collect();
        labels.sort();
        assert_eq!(labels, ["Xcode_26.xip", "deep/folder/dataset.tar"]);
        let x = find(&items, "Xcode_26.xip").verdict.clone().unwrap();
        assert_eq!(x.badge, Badge::Review);
        assert_eq!(x.reason, "downloaded 45 days ago; you can download it again");
    }
}

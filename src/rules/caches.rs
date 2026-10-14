//! Rule 12: apps' caches: the folders in ~/Library/Caches, and the developer
//! tools' folders in the per-user cache folder macOS keeps beside $TMPDIR.
//! Apps make them again as they need them, so one goes once its app isn't
//! running and nothing in it was written for a week. Apps' sandbox
//! containers stay unread: macOS asks before letting anything into another
//! app's data.

use super::{Found, Sink, gone, idle, idle_for, no_mounts, not_in_use, readable, sized};
use crate::ctx::Ctx;
use crate::glob;
use crate::model::CachePlace;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk;
use std::path::{Path, PathBuf};

/// In the per-user cache folder, only developer tools' caches: the rest are
/// the system's own (Launchpad's layout lives there).
const USER_TOOLS: &[&str] = &["clang", "lldb", "org.python.python", "com.apple.dt.*", "org.swift.*", "org.llvm.*"];

/// Below this a cache isn't worth a row.
const SMALL: u64 = 1_000_000;

fn library(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Caches")
}

/// The cache folder beside $TMPDIR (`…/T/` is the temp folder, `…/C/` the caches).
fn user(ctx: &Ctx) -> Option<PathBuf> {
    let t = ctx.tmpdir.components().collect::<PathBuf>();
    (t.file_name()? == "T").then(|| t.with_file_name("C"))
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    let mut r = vec![library(ctx)];
    r.extend(user(ctx));
    r
}

/// Folders in ~/Library/Caches another rule looks after: package managers'
/// (wherever their variables point) and Core ML's.
fn others(ctx: &Ctx, path: &Path) -> bool {
    const PACKAGES: [&str; 6] = ["pip", "Homebrew", "Yarn", "org.swift.swiftpm", "ms-playwright", "node-gyp"];
    path.file_name().is_some_and(|n| PACKAGES.iter().any(|p| n == *p))
        || super::pkgcache::locations(ctx).iter().any(|(_, p)| p == path)
        || path.join("com.apple.e5rt.e5bundlecache").exists()
}

/// The cache a new entry in ~/Library/Caches or the per-user cache folder is.
pub fn entry(ctx: &Ctx, path: &Path) -> Option<Found> {
    let dir = path.parent()?;
    let owner = path.file_name()?.to_string_lossy().into_owned();
    let place = if dir == library(ctx) {
        CachePlace::Library
    } else if Some(dir) == user(ctx).as_deref() && USER_TOOLS.iter().any(|t| glob::matches(t, &owner)) {
        CachePlace::User
    } else {
        return None;
    };
    if owner.starts_with('.') || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) || others(ctx, path) {
        return None;
    }
    Some((Kind::AppCache, path.to_path_buf(), owner.clone(), Target::AppCache { owner, place }))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    for dir in [Some(library(ctx)), user(ctx)].into_iter().flatten() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if let Some((kind, path, label, target)) = entry(ctx, &e.path()) {
                sink.item(kind, path, label, target);
            }
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, owner: &str) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Safe, "");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    match ctx.procs.app_running(owner) {
        Some(why) => {
            v.checks.push(Check::fail(why.clone()));
            v.keep(format!("its app is running: {why}"));
        }
        None => v.checks.push(Check::pass(format!("{owner} isn't running"))),
    }
    idle(ctx, &mut v, path, &s, ctx.cfg.thresholds.caches_days);
    not_in_use(ctx, &mut v, path);
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge == Badge::Safe {
        v.reason = format!("a cache, made again when needed; idle {}", idle_for(ctx, &s));
    }
    if v.badge != Badge::Kept && s.freed < SMALL {
        v.hidden = true;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Keep;
    use crate::procs::{Proc, Procs};
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{set_mtime, write, write_random};
    use std::time::{Duration, SystemTime};

    #[test]
    fn idle_caches_of_apps_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let caches = home.join("Library/Caches");
        write_random(&caches.join("com.example.Idle/data/blob"), 2_000_000);
        write_random(&caches.join("com.example.Busy/blob"), 2_000_000);
        write_random(&caches.join("com.example.Running/blob"), 2_000_000);
        write(&caches.join("com.example.Tiny/x"), "x");
        write_random(&caches.join("pip/http/a"), 2_000_000);
        write_random(&caches.join("com.example.Models/com.apple.e5rt.e5bundlecache/25A1/m/x"), 10);
        write_random(&caches.join("work-tool/obj"), 2_000_000);
        write_random(&home.join("Library/Containers/com.example.Sandboxed/Data/Library/Caches/c"), 2_000_000);
        write_random(&home.join("C/clang/ModuleCache/m.pcm"), 2_000_000);
        write_random(&home.join("C/com.apple.dock.launchpad/db"), 2_000_000);
        let now = SystemTime::now() + Duration::from_secs(10 * 86_400);
        set_mtime(&caches.join("com.example.Busy/blob"), now - Duration::from_secs(3 * 86_400));
        let mut ctx = crate::ctx::test_ctx(&home, now);
        ctx.cfg.keep = vec![Keep::Why { path: "~/Library/Caches/work-tool".into(), why: "work's tools".into() }];
        let ctx = ctx.at(
            now,
            Procs::from_list(vec![Proc {
                pid: 9,
                cmd: "/Applications/Running.app/Contents/MacOS/running".into(),
                exe: None,
                cwd: None,
            }]),
        );
        let (items, _) = scan(&ctx, discover);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(!labels.contains(&"pip"), "pip's cache is the package caches' rule");
        assert!(!labels.contains(&"com.example.Models"), "Core ML's caches are their own rule");
        assert!(!labels.contains(&"com.apple.dock.launchpad"), "the system's own, in the per-user folder");
        assert_eq!(badge(&items, "com.example.Idle"), Badge::Safe);
        let busy = find(&items, "com.example.Busy").verdict.clone().unwrap();
        assert_eq!((busy.badge, busy.reason.as_str()), (Badge::Kept, "in use: written 3 days ago"));
        assert_eq!(
            find(&items, "com.example.Running").verdict.as_ref().unwrap().reason,
            "its app is running: running is running"
        );
        assert!(find(&items, "com.example.Tiny").verdict.as_ref().unwrap().hidden, "too small to show");
        assert_eq!(find(&items, "work-tool").verdict.as_ref().unwrap().reason, "always kept: work's tools");
        assert!(!labels.contains(&"com.example.Sandboxed"), "apps' containers aren't read");
        assert_eq!(badge(&items, "clang"), Badge::Safe);
    }
}

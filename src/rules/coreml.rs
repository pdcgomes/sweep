//! Rule 4: Core ML's compiled-model caches,
//! ~/Library/Caches/<app>/com.apple.e5rt.e5bundlecache/<OS build>/<model>.
//! Models nobody has used for a while go, and so does everything compiled
//! for a macOS build other than the running one.

use super::{Sink, gone, idle, idle_for, no_mounts, not_in_use, readable, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk;
use crate::watch::Change;
use std::path::{Path, PathBuf};

const CACHE: &str = "com.apple.e5rt.e5bundlecache";

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![ctx.home.join("Library/Caches")]
}

/// A cache, a macOS build in one, or a model in a build, made or removed.
pub fn lists(_: &Ctx, c: &Change) -> bool {
    c.entries && c.path.ancestors().take(3).any(|a| a.ends_with(CACHE))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let Ok(rd) = std::fs::read_dir(ctx.home.join("Library/Caches")) else { return };
    for app in rd.flatten() {
        let cache = app.path().join(CACHE);
        let Ok(builds) = std::fs::read_dir(&cache) else { continue };
        let owner = app.file_name().to_string_lossy().into_owned();
        for b in builds.flatten() {
            if !b.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let build = b.file_name().to_string_lossy().into_owned();
            if build != ctx.os_build {
                let label = format!("{owner} · macOS {build}");
                sink.item(Kind::CoreMl, b.path(), label, Target::CoreMl { owner: owner.clone(), build, model: None });
                continue;
            }
            let Ok(models) = std::fs::read_dir(b.path()) else { continue };
            for m in models.flatten() {
                let model = m.file_name().to_string_lossy().into_owned();
                let short: String = model.chars().take(12).collect();
                let label = format!("{owner} · {short}");
                let target = Target::CoreMl { owner: owner.clone(), build: build.clone(), model: Some(model) };
                sink.item(Kind::CoreMl, m.path(), label, target);
            }
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, owner: &str, build: &str, is_model: bool) -> Verdict {
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
    if is_model {
        idle(ctx, &mut v, path, &s, ctx.cfg.thresholds.coreml_days);
    } else {
        v.checks.push(Check::pass(format!("compiled for macOS build {build}; this Mac runs {}", ctx.os_build)));
    }
    not_in_use(ctx, &mut v, path);
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge == Badge::Safe {
        v.reason = if is_model {
            format!("compiled model unused for {}; rebuilt on next use", idle_for(ctx, &s))
        } else {
            format!("compiled for macOS {build}, not the running {}", ctx.os_build)
        };
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{Proc, Procs};
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{set_mtime, write};
    use std::time::{Duration, SystemTime};

    #[test]
    fn stale_models_and_other_builds() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let caches = home.join("Library/Caches");
        let m = |app: &str, build: &str, model: &str| caches.join(app).join(CACHE).join(build).join(model);
        write(&m("com.example.Photos", "25G83", "AAAAAAAAAAAAAAAA/model.milhash"), "x");
        write(&m("com.example.Photos", "25G83", "BBBBBBBBBBBBBBBB/model.milhash"), "x");
        write(&m("com.adobe.lightroomCC", "24G90", "CCCC/model.milhash"), "x");
        write(&m("com.apple.remindd", "25B78", "DDDD/model.milhash"), "x");

        let now = SystemTime::now() + Duration::from_secs(40 * 86_400);
        set_mtime(
            &m("com.example.Photos", "25G83", "BBBBBBBBBBBBBBBB/model.milhash"),
            now - Duration::from_secs(86_400 * 3),
        );
        let mut ctx = crate::ctx::test_ctx(&home, now);
        ctx.procs = Procs::from_list(vec![Proc {
            pid: 1,
            cmd: "/usr/libexec/remindd".into(),
            exe: Some("/usr/libexec/remindd".into()),
            cwd: None,
        }]);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(items.len(), 4);
        assert_eq!(badge(&items, "com.example.Photos · AAAAAAAAAAAA"), Badge::Safe);
        assert_eq!(
            find(&items, "com.example.Photos · BBBBBBBBBBBB").verdict.as_ref().unwrap().reason,
            "in use: written 3 days ago"
        );
        let lr = find(&items, "com.adobe.lightroomCC · macOS 24G90").verdict.clone().unwrap();
        assert_eq!(lr.badge, Badge::Safe);
        assert_eq!(lr.reason, "compiled for macOS 24G90, not the running 25G83");
        let rem = find(&items, "com.apple.remindd · macOS 25B78").verdict.clone().unwrap();
        assert_eq!(rem.badge, Badge::Kept, "the caches of a running app stay");
    }
}

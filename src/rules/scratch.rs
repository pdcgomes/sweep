//! Rule 9: the scratch roots named in the config. Top-level folders go once
//! nothing inside has been written for a week, except the ones kept there.

use super::{Found, Sink, gone, idle, idle_for, no_mounts, not_in_use, readable, sized};
use crate::config;
use crate::ctx::Ctx;
use crate::glob;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk;
use std::path::{Path, PathBuf};

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    ctx.cfg.scratch.iter().map(|s| config::expand(&ctx.home, &s.path)).collect()
}

/// The item a top-level folder of a scratch root is.
pub fn entry(ctx: &Ctx, path: &Path) -> Option<Found> {
    let dir = path.parent()?;
    let root = ctx.cfg.scratch.iter().find(|s| config::expand(&ctx.home, &s.path) == dir)?;
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name.starts_with('.') || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let days = root.days.unwrap_or(ctx.cfg.thresholds.scratch_days);
    let kept_by = root.keep.iter().find(|k| glob::matches(k, &name)).cloned();
    let root_name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some((Kind::Scratch, path.to_path_buf(), format!("{root_name}/{name}"), Target::Scratch { kept_by, days }))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    for root in &ctx.cfg.scratch {
        let dir = config::expand(&ctx.home, &root.path);
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => {
                let why = if e.kind() == std::io::ErrorKind::NotFound {
                    "isn't there (not mounted?)".into()
                } else {
                    e.to_string()
                };
                sink.note(Kind::Scratch, format!("{}: {why}", dir.display()));
                continue;
            }
        };
        for e in entries.flatten() {
            if let Some((kind, path, label, target)) = entry(ctx, &e.path()) {
                sink.item(kind, path, label, target);
            }
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, kept_by: Option<&str>, days: f64) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Safe, "");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    if let Some(k) = kept_by {
        v.checks.push(Check::fail(format!("kept in this scratch root ({k})")));
        v.keep(format!("kept in this scratch root ({k})"));
    }
    idle(ctx, &mut v, path, &s, days);
    not_in_use(ctx, &mut v, path);
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge == Badge::Safe {
        v.reason = format!("idle {}", idle_for(ctx, &s));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Scratch;
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{set_mtime, write};
    use std::time::{Duration, SystemTime};

    #[test]
    fn idle_folders_except_the_kept_ones() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let root = home.join("SSD/scratch");
        write(&root.join("old-run/out.bin"), "x");
        write(&root.join("fixtures/a"), "x");
        write(&root.join("models-dev/w"), "x");
        write(&root.join("copy-3/w"), "x");
        write(&root.join("recent/r"), "x");
        write(&root.join("top-level-file.log"), "x");
        let now = SystemTime::now() + Duration::from_secs(9 * 86_400);
        set_mtime(&root.join("recent/r"), now - Duration::from_secs(3 * 86_400));
        let mut ctx = crate::ctx::test_ctx(&home, now);
        ctx.cfg.scratch = vec![
            Scratch {
                path: root.display().to_string(),
                keep: vec!["fixtures".into(), "models-*".into(), "copy-*".into()],
                days: None,
            },
            Scratch { path: "/Volumes/NoSuchVolume-sweep/tmp".into(), keep: vec![], days: None },
        ];
        let (items, notes) = scan(&ctx, discover);
        assert_eq!(items.len(), 5, "top-level files aren't candidates");
        assert_eq!(badge(&items, "scratch/old-run"), Badge::Safe);
        assert_eq!(badge(&items, "scratch/fixtures"), Badge::Kept);
        assert_eq!(badge(&items, "scratch/models-dev"), Badge::Kept);
        assert_eq!(badge(&items, "scratch/copy-3"), Badge::Kept);
        assert_eq!(find(&items, "scratch/recent").verdict.as_ref().unwrap().reason, "in use: written 3 days ago");
        assert!(notes[0].contains("isn't there"));
    }
}

//! Each rule knows where one kind of junk lives, how to show it's safe and how
//! to delete it. Discovery is cheap and streams items out; checking (which
//! measures) and deleting dispatch on the item's target.

pub mod build;
pub mod caches;
pub mod coreml;
pub mod derived;
pub mod docker;
pub mod downloads;
pub mod extensions;
pub mod logs;
pub mod models;
pub mod pkgcache;
pub mod scratch;
pub mod simulators;
pub mod snapshots;
pub mod tmp;
pub mod trash;
pub mod worktrees;
pub mod xcode;

use crate::ctx::Ctx;
use crate::model::{Badge, Check, Item, Kind, ModelSource, Target, Verdict};
use crate::walk::{Newest, Stats};
use crate::watch::Change;
use anyhow::Result;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

pub trait Sink: Sync {
    fn item(&self, kind: Kind, path: PathBuf, label: String, target: Target);
    fn note(&self, kind: Kind, text: String);
}

pub type Discover = fn(&Ctx, &dyn Sink);

/// An item as a rule finds it.
pub type Found = (Kind, PathBuf, String, Target);

/// A rule: the kinds of item it finds, and how changes bear on it while
/// sweep stays open.
pub struct Rule {
    pub kinds: &'static [Kind],
    pub discover: Discover,
    /// The folders its items live in, to watch.
    pub roots: fn(&Ctx) -> Vec<PathBuf>,
    /// For a rule that lists one folder: the item a new entry there is, so
    /// it's added without listing the folder again.
    pub entry: Option<fn(&Ctx, &Path) -> Option<Found>>,
    /// Whether a change can change what it finds in a way `entry` doesn't
    /// cover, rather than only what's inside one of its items.
    pub lists: fn(&Ctx, &Change) -> bool,
    /// The least time between two looks prompted by changes.
    pub gap: Duration,
    /// How often it looks again anyway, for what no folder change shows: a
    /// tool's own list, a lock, a file growing old enough.
    pub every: Duration,
}

const MIN: u64 = 60;
const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}
fn nowhere(_: &Ctx) -> Vec<PathBuf> {
    Vec::new()
}
fn never(_: &Ctx, _: &Change) -> bool {
    false
}

pub const RULES: [Rule; 17] = [
    Rule {
        kinds: &[Kind::Worktree],
        discover: worktrees::discover,
        roots: worktrees::roots,
        entry: None,
        lists: worktrees::lists,
        gap: secs(30),
        every: secs(10 * MIN),
    },
    Rule {
        kinds: &[Kind::Temp],
        discover: tmp::discover,
        roots: tmp::roots,
        entry: Some(tmp::entry),
        lists: never,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::DerivedData],
        discover: derived::discover,
        roots: derived::roots,
        entry: Some(derived::entry),
        lists: derived::lists,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::CoreMl],
        discover: coreml::discover,
        roots: coreml::roots,
        entry: None,
        lists: coreml::lists,
        gap: secs(30),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::SimDevice, Kind::SimRuntime],
        discover: simulators::discover,
        roots: simulators::roots,
        entry: None,
        lists: simulators::lists,
        gap: secs(30),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::Docker],
        discover: docker::discover,
        roots: docker::roots,
        entry: None,
        lists: docker::lists,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::Logs],
        discover: logs::discover,
        roots: logs::roots,
        entry: Some(logs::entry),
        lists: never,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::PackageCache],
        discover: pkgcache::discover,
        roots: pkgcache::roots,
        entry: None,
        lists: pkgcache::lists,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::Scratch],
        discover: scratch::discover,
        roots: scratch::roots,
        entry: Some(scratch::entry),
        lists: never,
        gap: secs(15),
        every: secs(10 * MIN),
    },
    Rule {
        kinds: &[Kind::Downloads],
        discover: downloads::discover,
        roots: downloads::roots,
        entry: None,
        lists: downloads::lists,
        gap: secs(60),
        every: secs(60 * MIN),
    },
    Rule {
        kinds: &[Kind::Snapshot],
        discover: snapshots::discover,
        roots: nowhere,
        entry: None,
        lists: never,
        gap: secs(60),
        every: secs(10 * MIN),
    },
    Rule {
        kinds: &[Kind::AppCache],
        discover: caches::discover,
        roots: caches::roots,
        entry: Some(caches::entry),
        lists: never,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::BuildOutput],
        discover: build::discover,
        roots: build::roots,
        entry: Some(build::entry),
        lists: never,
        gap: secs(30),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::XcodeSupport],
        discover: xcode::discover,
        roots: xcode::roots,
        entry: None,
        lists: xcode::lists,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::Extension],
        discover: extensions::discover,
        roots: extensions::roots,
        entry: None,
        lists: extensions::lists,
        gap: secs(15),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::Model],
        discover: models::discover,
        roots: models::roots,
        entry: None,
        lists: models::lists,
        gap: secs(30),
        every: secs(30 * MIN),
    },
    Rule {
        kinds: &[Kind::Trash],
        discover: trash::discover,
        roots: trash::roots,
        entry: None,
        lists: never,
        gap: secs(15),
        every: secs(10 * MIN),
    },
];

/// Measures an item and decides its badge, reason and checks.
pub fn evaluate(ctx: &Ctx, item: &Item) -> Verdict {
    let mut v = evaluate_now(ctx, item);
    v.as_of = Some(ctx.now);
    v
}

fn evaluate_now(ctx: &Ctx, item: &Item) -> Verdict {
    let mut v = match &item.target {
        Target::Worktree(info) => worktrees::evaluate(ctx, &item.path, info),
        Target::BuildOutput { project, output } => build::evaluate(ctx, &item.path, project, output),
        Target::AppCache { owner, .. } => caches::evaluate(ctx, &item.path, owner),
        Target::Xcode(f) => xcode::evaluate(ctx, &item.path, f),
        Target::Extension { editor, app } => extensions::evaluate(ctx, &item.path, editor, app),
        Target::Model(src) => models::evaluate(ctx, &item.path, src),
        Target::Trash => trash::evaluate(ctx, &item.path),
        Target::Temp { .. } => tmp::evaluate(ctx, &item.path),
        Target::DerivedData { workspace } => derived::evaluate(ctx, &item.path, workspace.as_deref()),
        Target::CoreMl { owner, build, model } => coreml::evaluate(ctx, &item.path, owner, build, model.is_some()),
        Target::SimDevice(d) => simulators::evaluate_device(ctx, &item.path, d),
        Target::SimRuntime(r) => simulators::evaluate_runtime(ctx, r),
        Target::Docker => docker::evaluate(ctx, &item.path),
        Target::Logs => logs::evaluate(ctx, &item.path),
        Target::PackageCache(tool) => pkgcache::evaluate(ctx, &item.path, *tool),
        Target::Scratch { kept_by, days } => scratch::evaluate(ctx, &item.path, kept_by.as_deref(), *days),
        Target::Download => downloads::evaluate(ctx, &item.path),
        Target::Snapshot { volume, name } => snapshots::evaluate(volume, name),
    };
    if !matches!(item.target, Target::SimRuntime(_) | Target::Snapshot { .. }) {
        if let Some(why) = ctx.guard.check(&item.path) {
            v.force_keep(why);
        } else if v.badge != Badge::Kept
            && let Some(why) = cache_owner_running(ctx, &item.path)
        {
            v.force_keep(why);
        }
    }
    v
}

/// The verdict for an item kept for a reason known without measuring it (a
/// keep list, a running app, a project that still exists), so the scan can
/// skip or postpone the walk.
pub fn precheck(ctx: &Ctx, item: &Item) -> Option<Verdict> {
    let static_reason = || -> Option<String> {
        match &item.target {
            Target::Scratch { kept_by: Some(k), .. } => Some(format!("kept in this scratch root ({k})")),
            Target::PackageCache(tool) => pkgcache::kept_without_measuring(ctx, *tool),
            Target::DerivedData { workspace: None } => Some("can't tell which project it belongs to".into()),
            Target::AppCache { owner, .. } => ctx.procs.app_running(owner).map(|w| format!("its app is running: {w}")),
            Target::Xcode(f) => xcode::kept_without_measuring(ctx, f),
            Target::Worktree(info) if info.locked.is_some() => Some(match info.locked.as_deref() {
                Some("") | None => "locked".into(),
                Some(r) => format!("locked: {r}"),
            }),
            Target::Worktree(info) if !info.nested.is_empty() => Some("contains another worktree".into()),
            Target::CoreMl { owner, .. } => ctx.procs.app_running(owner).map(|w| format!("its app is running: {w}")),
            Target::Docker => ctx.procs.find(docker::docker_process).map(|_| "Docker Desktop is running".into()),
            _ => None,
        }
    };
    let why = if matches!(item.target, Target::SimRuntime(_) | Target::Snapshot { .. }) {
        None
    } else {
        ctx.guard.check(&item.path).or_else(|| cache_owner_running(ctx, &item.path)).or_else(static_reason)
    }?;
    let mut v = Verdict::new(Badge::Kept, why.clone());
    v.checks.push(Check::fail(why));
    v.unmeasured = true;
    v.as_of = Some(ctx.now);
    Some(v)
}

/// How a rule judged by idleness judges an item: the folder whose writes
/// count (the item, or the project a build output is in), the days without
/// a write it needs outside and inside its split folder (a worktree's
/// `build`), and what it leaves out: nested trees, and folders by name.
pub struct Idleness<'a> {
    pub root: &'a Path,
    pub days: f64,
    pub build_days: f64,
    pub split: Option<&'static OsStr>,
    pub exclude: &'a [PathBuf],
    pub skip: &'static [&'static str],
}

pub fn idleness<'a>(ctx: &Ctx, item: &'a Item) -> Option<Idleness<'a>> {
    let t = &ctx.cfg.thresholds;
    let plain = |days: f64| Idleness { root: &item.path, days, build_days: days, split: None, exclude: &[], skip: &[] };
    Some(match &item.target {
        Target::Temp { .. } => plain(t.temp_days),
        Target::Scratch { kept_by: None, days } => plain(*days),
        Target::DerivedData { workspace } => plain(if workspace.as_ref().is_some_and(|w| w.exists()) {
            t.derived_data_idle_days
        } else {
            t.derived_data_days
        }),
        Target::CoreMl { model: Some(_), .. } => plain(t.coreml_days),
        Target::AppCache { .. } => plain(t.caches_days),
        Target::Xcode(f) => plain(xcode::idle_days(t, f)?),
        Target::BuildOutput { project, .. } => {
            Idleness { root: project, skip: build::SKIP, ..plain(t.build_output_days) }
        }
        Target::Worktree(info) => Idleness {
            root: &item.path,
            days: t.worktree_edit_days,
            build_days: t.worktree_build_days,
            split: Some(OsStr::new("build")),
            exclude: info.nested.as_slice(),
            skip: &[],
        },
        _ => return None,
    })
}

/// The folder whose changes bear on an item: the item, or the project a
/// build output belongs to.
pub fn watched(item: &Item) -> &Path {
    match &item.target {
        Target::BuildOutput { project, .. } => project,
        _ => &item.path,
    }
}

/// For the rules judged by idleness, a write within the threshold makes an
/// item in use, and a quick probe that stops at the first recent file shows
/// that without measuring the whole tree.
pub fn busy(ctx: &Ctx, item: &Item) -> Option<Verdict> {
    let i = idleness(ctx, item)?;
    let opts =
        crate::walk::Opts { exclude: i.exclude, split: i.split, no_sizes: true, skip: i.skip, ..Default::default() };
    let (n, in_build) = crate::walk::recent(i.root, &opts, ctx.cutoff(i.days), ctx.cutoff(i.build_days))?;
    Some(in_use(ctx, item, i.root, n, in_build))
}

/// The same, looking only at the files that have just changed: one written
/// within the threshold settles it, a lookup each, however big their folders.
pub fn busy_in(ctx: &Ctx, item: &Item, paths: &[PathBuf]) -> Option<Verdict> {
    let i = idleness(ctx, item)?;
    let mut best: Option<(Newest, bool)> = None;
    for p in paths {
        let Ok(inner) = p.strip_prefix(i.root) else { continue };
        let left_out = inner.components().any(|c| i.skip.iter().any(|s| c.as_os_str() == *s));
        if left_out || i.exclude.iter().any(|x| p.starts_with(x)) {
            continue;
        }
        let Ok(a) = crate::fsattr::attrs(p) else { continue };
        let in_build = i.split.is_some_and(|s| inner.components().next().is_some_and(|c| c.as_os_str() == s));
        let cutoff = ctx.cutoff(if in_build { i.build_days } else { i.days });
        let t = a.last_write();
        if a.kind != crate::fsattr::ObjKind::Dir && t >= cutoff && best.as_ref().is_none_or(|(b, _)| t > b.time) {
            best = Some((Newest { time: t, path: p.clone() }, in_build));
        }
    }
    let (n, in_build) = best?;
    Some(in_use(ctx, item, i.root, n, in_build))
}

fn in_use(ctx: &Ctx, item: &Item, root: &Path, n: Newest, in_build: bool) -> Verdict {
    let ago = ctx.ago(n.time);
    let what = rel(root, &n.path);
    let (check, reason) = match (&item.target, in_build) {
        (Target::Worktree(_), true) => (format!("built {ago}: {what}"), format!("in use: built {ago}")),
        (Target::Worktree(_), false) => (format!("edited {ago}: {what}"), format!("in use: edited {ago}")),
        (Target::BuildOutput { .. }, _) => {
            (format!("project edited {ago}: {what}"), format!("in use: project edited {ago}"))
        }
        _ => (format!("written {ago}: {what}"), format!("in use: written {ago}")),
    };
    let mut v = Verdict::new(Badge::Kept, reason);
    v.checks.push(Check::fail(check));
    v.last_write = Some(n.time);
    v.unmeasured = true;
    v.as_of = Some(ctx.now);
    v
}

/// What prompted a second look at an item.
#[derive(Clone, Debug, PartialEq)]
pub enum Why {
    /// These files or folders in it changed (the latest few).
    Changed(Vec<PathBuf>),
    /// Anything inside it may have changed.
    Anything,
    /// Time passed, the running processes changed, or it's new: nothing is
    /// known about its contents.
    Again,
    /// Someone wants it measured.
    Asked,
}

/// Looks at an item again, doing as little as the change allows. `None`
/// when it's gone. An item that stays kept is never measured just because
/// something inside it changed: its size shows as unknown until asked for.
pub fn recheck(ctx: &Ctx, item: &Item, why: &Why) -> Option<Verdict> {
    let path_backed = !matches!(item.target, Target::SimRuntime(_) | Target::Snapshot { .. });
    if path_backed && std::fs::symlink_metadata(&item.path).is_err() {
        return None;
    }
    if *why == Why::Asked {
        return Some(evaluate(ctx, item));
    }
    let content = matches!(why, Why::Changed(_) | Why::Anything);
    let current = item.verdict.as_ref();
    // Writes only make log files younger, so what's old enough to go can only
    // shrink; the hourly look will see to it.
    if content
        && matches!(item.target, Target::Logs)
        && let Some(c) = current
    {
        return Some(c.clone());
    }
    if let Some(v) = precheck(ctx, item) {
        return Some(match current {
            Some(c) if c.badge == Badge::Kept && c.reason == v.reason => {
                if content {
                    c.stale()
                } else {
                    c.clone()
                }
            }
            _ => v,
        });
    }
    if let Some(i) = idleness(ctx, item) {
        if let Why::Changed(paths) = why {
            if let Some(v) = busy_in(ctx, item, paths) {
                return Some(v);
            }
            // Kept for a recent write, which no later change undoes (a
            // removal only makes it idle sooner): it stays kept until that
            // write is old enough, when `due` looks again.
            let recent = |c: &&Verdict| c.last_write.is_some_and(|t| ctx.age_days(t) < i.days.min(i.build_days));
            if let Some(c) = current.filter(|c| c.badge == Badge::Kept).filter(recent) {
                return Some(c.clone());
            }
        }
        if let Some(v) = busy(ctx, item) {
            return Some(v);
        }
        return Some(evaluate(ctx, item));
    }
    if content && let Some(c) = current.filter(|c| c.badge == Badge::Kept) {
        // Kept for something its contents don't decide: booted, Docker running…
        return Some(c.stale());
    }
    Some(evaluate(ctx, item))
}

/// When time alone could change an item's verdict: one in use because of a
/// recent write is idle once that write is old enough; logs grow old.
pub fn due(ctx: &Ctx, item: &Item, v: &Verdict) -> Option<SystemTime> {
    let t = &ctx.cfg.thresholds;
    let after = |at: SystemTime, days: f64| at + Duration::from_secs_f64(days * 86_400.0) + secs(MIN);
    let at = match &item.target {
        Target::Logs => Some(ctx.now + secs(60 * MIN)),
        Target::SimRuntime(r) if v.badge == Badge::Kept => r.last_used.map(|u| after(u, t.runtime_unused_days)),
        Target::Download if v.hidden => v.last_write.map(|w| after(w, t.downloads_days)),
        // Whether its project still exists is a stat, not a folder change.
        Target::DerivedData { workspace: Some(_) } if v.badge == Badge::Kept => Some(ctx.now + secs(10 * MIN)),
        _ => {
            let i = idleness(ctx, item)?;
            if v.badge != Badge::Kept {
                return None;
            }
            let days = if v.reason.starts_with("in use: built") { i.build_days } else { i.days };
            v.last_write.map(|w| after(w, days))
        }
    }?;
    (at > ctx.now).then_some(at)
}

/// For an item kept while some app or tool runs (a cache's owner, Docker,
/// pip), whether it runs; `None` for the rest.
pub fn owner_running(ctx: &Ctx, item: &Item) -> Option<bool> {
    match &item.target {
        Target::Docker => Some(ctx.procs.find(docker::docker_process).is_some()),
        Target::PackageCache(tool) => Some(pkgcache::kept_without_measuring(ctx, *tool).is_some()),
        Target::CoreMl { owner, .. } | Target::AppCache { owner, .. } => Some(ctx.procs.app_running(owner).is_some()),
        Target::Extension { app, .. } | Target::Model(ModelSource::Download { app, .. }) if !app.is_empty() => {
            Some(ctx.procs.app_running(app).is_some())
        }
        Target::Xcode(_) => Some(ctx.procs.app_running("com.apple.dt.Xcode").is_some()),
        _ if item.path.starts_with(ctx.home.join("Library/Caches")) => {
            Some(cache_owner_running(ctx, &item.path).is_some())
        }
        _ => None,
    }
}

/// Options the person deleting chose per item.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeleteOpts {
    /// For a dirty worktree: save the uncommitted work on a backup branch first.
    pub backup: bool,
}

/// Deletes an item whose fresh verdict is `v`. Returns a note for the log.
pub fn delete(ctx: &Ctx, item: &Item, v: &Verdict, opts: DeleteOpts) -> Result<String> {
    match &item.target {
        Target::Worktree(info) => worktrees::delete(&item.path, info, v, opts),
        Target::SimDevice(d) => simulators::delete_device(d),
        Target::SimRuntime(r) => simulators::delete_runtime(r),
        Target::Logs => logs::delete(ctx, &item.path),
        Target::PackageCache(tool) => pkgcache::delete(&item.path, *tool),
        Target::Snapshot { volume, name } => snapshots::delete(volume, name),
        Target::Model(ModelSource::Ollama { .. }) => models::delete(ctx, &item.path),
        // The Trash is emptied, not removed.
        Target::Trash => {
            crate::rmtree::remove_contents(&item.path)?;
            Ok(String::new())
        }
        Target::Temp { .. }
        | Target::BuildOutput { .. }
        | Target::AppCache { .. }
        | Target::DerivedData { .. }
        | Target::Xcode(_)
        | Target::CoreMl { .. }
        | Target::Docker
        | Target::Extension { .. }
        | Target::Model(_)
        | Target::Scratch { .. }
        | Target::Download => {
            crate::rmtree::remove(&item.path)?;
            Ok(String::new())
        }
    }
}

/// Caches under ~/Library/Caches/<owner> stay while their app runs.
fn cache_owner_running(ctx: &Ctx, path: &Path) -> Option<String> {
    let caches = ctx.home.join("Library/Caches");
    let owner = path.strip_prefix(&caches).ok()?.components().next()?;
    let owner = owner.as_os_str().to_string_lossy();
    ctx.procs.app_running(&owner).map(|why| format!("a running app's cache: {why}"))
}

pub fn sized(v: &mut Verdict, s: &Stats) {
    v.bytes = Some(s.freed);
    v.du = Some(s.du);
    v.last_write = s.last_write();
}

pub fn rel(root: &Path, p: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(r) if !r.as_os_str().is_empty() => r.display().to_string(),
        _ => p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string()),
    }
}

/// Passes when nothing inside was written for `days`; otherwise the item is
/// in use and kept. Judged by the newest file, never the folder's own date;
/// only a tree with no files at all falls back to its folders.
pub fn idle(ctx: &Ctx, v: &mut Verdict, root: &Path, s: &Stats, days: f64) -> bool {
    let (time, what) = match (&s.newest_file, s.newest_dir) {
        (Some(n), _) => (n.time, rel(root, &n.path)),
        (None, Some(t)) => (t, "no files inside; its folders' dates".to_string()),
        (None, None) => {
            v.checks.push(Check::fail("couldn't tell when it was last written"));
            v.keep("couldn't tell when it was last written");
            return false;
        }
    };
    let ago = ctx.ago(time);
    if ctx.age_days(time) >= days {
        v.checks.push(Check::pass(format!("nothing written for {} (newest: {what})", crate::fmt::age(age(ctx, time)))));
        true
    } else {
        v.checks.push(Check::fail(format!("written {ago}: {what}")));
        v.keep(format!("in use: written {ago}"));
        false
    }
}

fn age(ctx: &Ctx, t: std::time::SystemTime) -> std::time::Duration {
    ctx.now.duration_since(t).unwrap_or_default()
}

pub fn idle_for(ctx: &Ctx, s: &Stats) -> String {
    s.last_write().map(|t| crate::fmt::age(age(ctx, t))).unwrap_or_else(|| "?".into())
}

/// Passes when no running process names the path or works inside it.
pub fn not_in_use(ctx: &Ctx, v: &mut Verdict, path: &Path) -> bool {
    match ctx.procs.using(path) {
        Some(p) => {
            let who = p.describe();
            v.checks.push(Check::fail(format!("in use by {who}")));
            v.keep(format!("in use by {who}"));
            false
        }
        None => {
            v.checks.push(Check::pass("no running process uses it"));
            true
        }
    }
}

/// Folders that couldn't be read leave the verdict to you.
pub fn readable(v: &mut Verdict, s: &Stats) -> bool {
    if s.errors == 0 {
        return true;
    }
    let first = s.first_error.clone().unwrap_or_default();
    v.checks.push(Check::fail(format!(
        "couldn't read {} inside: {first}",
        crate::fmt::count(s.errors as usize, "folder", "folders")
    )));
    if v.badge == Badge::Safe {
        v.badge = Badge::Review;
        v.reason = "parts of it couldn't be read".into();
    }
    false
}

/// A volume mounted inside keeps the item: deleting must not reach into it.
pub fn no_mounts(v: &mut Verdict, s: &Stats) -> bool {
    match s.mounts.first() {
        Some(m) => {
            v.checks.push(Check::fail(format!("a volume is mounted inside at {}", m.display())));
            v.keep("a volume is mounted inside it");
            false
        }
        None => true,
    }
}

/// The item vanished between discovery and measuring.
pub fn gone(path: &Path) -> Option<Verdict> {
    if std::fs::symlink_metadata(path).is_err() {
        let mut v = Verdict::new(Badge::Kept, "no longer there");
        v.hidden = true;
        return Some(v);
    }
    None
}

/// Runs a tool and returns its stdout, or an error carrying its stderr.
pub fn run_tool(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().map_err(|e| anyhow::anyhow!("couldn't run {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let err = if err.is_empty() { String::from_utf8_lossy(&out.stdout).trim().to_string() } else { err };
        anyhow::bail!("{} failed: {}", cmd.get_program().to_string_lossy(), err);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Like `run_tool`, but gives up on a tool that hangs.
pub fn run_tool_timeout(cmd: &mut Command, limit: std::time::Duration) -> Result<String> {
    use std::io::Read;
    use std::process::Stdio;
    let name = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("couldn't run {name}: {e}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stdout.read_to_end(&mut s);
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = stderr.read_to_end(&mut s);
        s
    });
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{name} didn't finish within {} s", limit.as_secs());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let out = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
    let err = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).trim().to_string();
    if !status.success() {
        anyhow::bail!("{name} failed: {}", if err.is_empty() { out.trim() } else { &err });
    }
    Ok(out)
}

/// Finds a tool on PATH or in the usual install folders, since a launchd
/// agent gets a minimal PATH.
pub fn which(name: &str, home: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(home.join(".local/bin"));
    dirs.push(home.join(".cargo/bin"));
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

/// Simulator and snapshot tools don't take kindly to running side by side.
pub static TOOL_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::model::Item;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct VecSink {
        pub items: Mutex<Vec<(Kind, PathBuf, String, Target)>>,
        pub notes: Mutex<Vec<String>>,
    }

    impl Sink for VecSink {
        fn item(&self, kind: Kind, path: PathBuf, label: String, target: Target) {
            self.items.lock().unwrap().push((kind, path, label, target));
        }
        fn note(&self, _kind: Kind, text: String) {
            self.notes.lock().unwrap().push(text);
        }
    }

    /// Discovers with `rule` and checks every item, as a scan would.
    pub fn scan(ctx: &Ctx, rule: Discover) -> (Vec<Item>, Vec<String>) {
        let sink = VecSink::default();
        rule(ctx, &sink);
        let items = sink
            .items
            .into_inner()
            .unwrap()
            .into_iter()
            .enumerate()
            .map(|(id, (kind, path, label, target))| {
                let mut item = Item { id, kind, path, label, target, verdict: None };
                item.verdict = Some(evaluate(ctx, &item));
                item
            })
            .collect();
        (items, sink.notes.into_inner().unwrap())
    }

    pub fn find<'a>(items: &'a [Item], label: &str) -> &'a Item {
        items
            .iter()
            .find(|i| i.label == label)
            .unwrap_or_else(|| panic!("no item {label:?} in {:?}", items.iter().map(|i| &i.label).collect::<Vec<_>>()))
    }

    pub fn badge(items: &[Item], label: &str) -> Badge {
        find(items, label).badge().unwrap()
    }
}

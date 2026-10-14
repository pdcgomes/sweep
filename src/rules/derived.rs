//! Rule 3: Xcode DerivedData whose project no longer exists.

use super::{Found, Sink, gone, idle, idle_for, no_mounts, not_in_use, readable, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk;
use crate::watch::Change;
use std::path::{Path, PathBuf};

pub fn root(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Developer/Xcode/DerivedData")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![root(ctx)]
}

/// A project's info.plist, written after its folder, names the project.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    c.path.file_name().is_some_and(|n| n == "info.plist")
        && c.path.parent().and_then(Path::parent) == Some(root(ctx).as_path())
}

pub fn entry(ctx: &Ctx, path: &Path) -> Option<Found> {
    if path.parent()? != root(ctx) || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) {
        return None;
    }
    let workspace = workspace_path(&path.join("info.plist"));
    let label = path.file_name()?.to_string_lossy().into_owned();
    Some((Kind::DerivedData, path.to_path_buf(), label, Target::DerivedData { workspace }))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let Ok(rd) = std::fs::read_dir(root(ctx)) else { return };
    for e in rd.flatten() {
        if let Some((kind, path, label, target)) = entry(ctx, &e.path()) {
            sink.item(kind, path, label, target);
        }
    }
}

fn workspace_path(plist: &Path) -> Option<PathBuf> {
    let v = plist::Value::from_file(plist).ok()?;
    v.as_dictionary()?.get("WorkspacePath")?.as_string().map(PathBuf::from)
}

pub fn evaluate(ctx: &Ctx, path: &Path, workspace: Option<&Path>) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Safe, "");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    let ws = match workspace {
        Some(w) => w,
        None => {
            v.checks.push(Check::fail("no info.plist naming its project (WorkspacePath)"));
            v.keep("can't tell which project it belongs to");
            return v;
        }
    };
    let shown = ctx.tilde(ws);
    let t = &ctx.cfg.thresholds;
    // A project that still exists builds it again, so after a month its DerivedData is for review.
    let exists = ws.exists();
    if exists {
        v.checks.push(Check::info(format!("its project still exists: {shown}")));
    } else if let Some(vol) = unmounted_volume(ws) {
        v.checks.push(Check::fail(format!("its project is on {vol}, which isn't mounted")));
        v.keep(format!("project is on {vol}, which isn't mounted"));
    } else {
        v.checks.push(Check::pass(format!("its project is gone: {shown}")));
    }
    idle(ctx, &mut v, path, &s, if exists { t.derived_data_idle_days } else { t.derived_data_days });
    not_in_use(ctx, &mut v, path);
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge == Badge::Safe {
        if exists {
            v.badge = Badge::Review;
            v.reason = format!("nothing built for {}; Xcode builds it again", idle_for(ctx, &s));
        } else {
            v.reason = format!("project gone, idle {}", idle_for(ctx, &s));
        }
    }
    v
}

/// `/Volumes/<name>` when the path lives on a volume that isn't mounted now.
pub fn unmounted_volume(p: &Path) -> Option<String> {
    let mut c = p.components();
    c.next()?;
    if c.next()?.as_os_str() != "Volumes" {
        return None;
    }
    let vol = Path::new("/Volumes").join(c.next()?.as_os_str());
    if vol.exists() { None } else { Some(vol.display().to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{set_mtime, write};
    use std::time::{Duration, SystemTime};

    fn info_plist(dir: &Path, workspace: &str) {
        let mut d = plist::Dictionary::new();
        d.insert("WorkspacePath".into(), plist::Value::String(workspace.into()));
        std::fs::create_dir_all(dir).unwrap();
        plist::Value::Dictionary(d).to_file_xml(dir.join("info.plist")).unwrap();
    }

    #[test]
    fn only_projects_that_are_gone() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let dd = home.join("Library/Developer/Xcode/DerivedData");
        let live_ws = home.join("src/App/App.xcworkspace");
        std::fs::create_dir_all(&live_ws).unwrap();
        info_plist(&dd.join("App-aaaa"), live_ws.to_str().unwrap());
        write(&dd.join("App-aaaa/Build/Products/app"), "bin");
        info_plist(&dd.join("Old-bbbb"), home.join("src/Old/Old.xcodeproj").to_str().unwrap());
        write(&dd.join("Old-bbbb/Build/Products/app"), "bin");
        info_plist(&dd.join("Fresh-cccc"), home.join("src/Fresh/Fresh.xcodeproj").to_str().unwrap());
        write(&dd.join("Fresh-cccc/Index.noindex/x"), "y");
        info_plist(&dd.join("Ext-dddd"), "/Volumes/NoSuchVolume-sweep/proj/P.xcodeproj");
        std::fs::create_dir_all(dd.join("ModuleCache.noindex")).unwrap();
        info_plist(&dd.join("Live-eeee"), live_ws.to_str().unwrap());
        write(&dd.join("Live-eeee/Build/Products/app"), "bin");

        let now = SystemTime::now() + Duration::from_secs(40 * 86_400);
        set_mtime(&dd.join("Fresh-cccc/Index.noindex/x"), now - Duration::from_secs(7200));
        set_mtime(&dd.join("Live-eeee/Build/Products/app"), now - Duration::from_secs(5 * 86_400));
        let ctx = crate::ctx::test_ctx(&home, now);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(
            find(&items, "ModuleCache.noindex").verdict.as_ref().unwrap().reason,
            "can't tell which project it belongs to"
        );
        assert_eq!(badge(&items, "Old-bbbb"), Badge::Safe);
        let idle = find(&items, "App-aaaa").verdict.clone().unwrap();
        assert_eq!(
            (idle.badge, idle.reason.as_str()),
            (Badge::Review, "nothing built for 40 days; Xcode builds it again")
        );
        assert_eq!(find(&items, "Live-eeee").verdict.as_ref().unwrap().reason, "in use: written 5 days ago");
        assert_eq!(find(&items, "Fresh-cccc").verdict.as_ref().unwrap().reason, "in use: written 2 h ago");
        assert!(find(&items, "Ext-dddd").verdict.as_ref().unwrap().reason.contains("isn't mounted"));
    }
}

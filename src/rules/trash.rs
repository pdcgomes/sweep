//! Rule 17: the Trash, for review. What's in it was put there to go, but
//! emptying it is for good. One item per volume's Trash.

use super::{Sink, gone, readable, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk;
use std::path::{Path, PathBuf};

fn home_trash(ctx: &Ctx) -> PathBuf {
    ctx.home.join(".Trash")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![home_trash(ctx)]
}

/// The Trash in the home folder, and each mounted volume's one for this user.
fn trashes(ctx: &Ctx) -> Vec<(PathBuf, String)> {
    let mut out = vec![(home_trash(ctx), "Trash".to_string())];
    let uid = unsafe { libc::getuid() };
    if let Ok(rd) = std::fs::read_dir("/Volumes") {
        for v in rd.flatten() {
            let t = v.path().join(".Trashes").join(uid.to_string());
            if t.is_dir() {
                out.push((t, format!("Trash on {}", v.file_name().to_string_lossy())));
            }
        }
    }
    out
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let mut refused = false;
    for (path, label) in trashes(ctx) {
        match std::fs::read_dir(&path) {
            Ok(mut rd) => {
                if rd.any(|e| e.is_ok_and(|e| e.file_name() != ".DS_Store")) {
                    sink.item(Kind::Trash, path, label, Target::Trash);
                }
            }
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => refused = true,
            Err(_) => {}
        }
    }
    if refused {
        sink.note(
            Kind::Trash,
            "macOS didn't allow reading the Trash; give your terminal Full Disk Access in System Settings → Privacy & Security".into(),
        );
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Review, "emptying the Trash deletes what's in it for good");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    let count = std::fs::read_dir(path).map(|rd| rd.flatten().filter(|e| e.file_name() != ".DS_Store").count());
    let count = count.unwrap_or(0);
    if count == 0 {
        v.hidden = true;
        v.keep("empty");
        return v;
    }
    v.checks.push(Check::info(format!("{} in it", crate::fmt::count(count, "thing", "things"))));
    readable(&mut v, &s);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::testing::{find, scan};
    use crate::rules::{DeleteOpts, delete, evaluate as eval_item};
    use crate::testutil::write_random;
    use std::time::SystemTime;

    #[test]
    fn emptying_keeps_the_trash_itself() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write_random(&home.join(".Trash/old report.pdf"), 100_000);
        write_random(&home.join(".Trash/folder/x"), 100_000);
        let ctx = crate::ctx::test_ctx(&home, SystemTime::now());
        let (items, _) = scan(&ctx, discover);
        let t = find(&items, "Trash");
        assert_eq!(t.badge(), Some(Badge::Review));
        assert!(t.bytes().unwrap() >= 200_000);
        let v = eval_item(&ctx, t);
        delete(&ctx, t, &v, DeleteOpts::default()).unwrap();
        assert!(home.join(".Trash").is_dir(), "the Trash stays");
        assert_eq!(std::fs::read_dir(home.join(".Trash")).unwrap().count(), 0);
    }
}

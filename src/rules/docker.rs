//! Rule 6: Docker Desktop's disk image, only while Docker isn't running.

use super::{Sink, gone, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::procs::Proc;
use crate::walk;
use crate::watch::Change;
use std::path::{Path, PathBuf};

pub fn disk(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    disk(ctx).parent().map(Path::to_path_buf).into_iter().collect()
}

pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    c.entries && c.path == disk(ctx)
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let p = disk(ctx);
    match std::fs::symlink_metadata(&p) {
        Ok(_) => sink.item(Kind::Docker, p, "Docker.raw".into(), Target::Docker),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            sink.note(Kind::Docker, format!("macOS didn't allow reading {}", ctx.tilde(&p)));
        }
        Err(_) => {}
    }
}

/// Docker Desktop's own processes. The privileged helpers in
/// /Library/PrivilegedHelperTools run all the time and don't count.
pub fn docker_process(p: &Proc) -> bool {
    let exe = p.exe.as_ref().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
    if exe.starts_with("/Library/PrivilegedHelperTools/") {
        return false;
    }
    let name = p.name();
    exe.contains("/Docker.app/")
        || name == "com.docker.backend"
        || name == "com.docker.virtualization"
        || name == "com.docker.krun"
        || name == "Docker Desktop"
}

pub fn evaluate(ctx: &Ctx, path: &Path) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Review, "every image, container and volume; Docker makes a new empty disk");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    match ctx.procs.find(docker_process) {
        Some(p) => {
            v.checks.push(Check::fail(format!("Docker Desktop is running: {}", p.describe())));
            v.keep("Docker Desktop is running");
        }
        None => v.checks.push(Check::pass("Docker Desktop isn't running")),
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::Procs;
    use crate::rules::testing::scan;
    use crate::testutil::write_random;
    use std::time::SystemTime;

    #[test]
    fn only_while_docker_is_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let mut ctx = crate::ctx::test_ctx(&home, SystemTime::now());
        write_random(&disk(&ctx), 1 << 20);
        let helper = Proc {
            pid: 1,
            cmd: "/Library/PrivilegedHelperTools/com.docker.vmnetd".into(),
            exe: Some("/Library/PrivilegedHelperTools/com.docker.vmnetd".into()),
            cwd: None,
        };
        ctx.procs = Procs::from_list(vec![helper.clone()]);
        let (items, _) = scan(&ctx, discover);
        let v = items[0].verdict.clone().unwrap();
        assert_eq!(v.badge, Badge::Review);
        assert_eq!(v.bytes, Some(1 << 20));

        let backend = "/Applications/Docker.app/Contents/MacOS/com.docker.backend";
        ctx.procs =
            Procs::from_list(vec![helper, Proc { pid: 2, cmd: backend.into(), exe: Some(backend.into()), cwd: None }]);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(items[0].verdict.as_ref().unwrap().reason, "Docker Desktop is running");
    }
}

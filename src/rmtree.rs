//! Parallel `rm -rf`. Tens of thousands of folders one at a time took hours.
//! Never follows symlinks and never crosses into another volume.

use crate::fsattr::{self, ObjKind};
use rayon::prelude::*;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Removes a file, symlink or folder tree.
pub fn remove(path: &Path) -> io::Result<()> {
    let a = match fsattr::attrs(path) {
        Ok(a) => a,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let parent = path.parent().unwrap_or(Path::new("/"));
    if a.kind == ObjKind::Dir {
        remove_contents_on(path, a.dev)?;
        retry(path, parent, || std::fs::remove_dir(path))
    } else {
        retry(path, parent, || std::fs::remove_file(path))
    }
}

/// Removes everything inside `dir`, keeping `dir` itself.
pub fn remove_contents(dir: &Path) -> io::Result<()> {
    let a = fsattr::attrs(dir)?;
    if a.kind != ObjKind::Dir {
        return Err(io::Error::other(format!("{} is not a folder", dir.display())));
    }
    remove_contents_on(dir, a.dev)
}

fn remove_contents_on(dir: &Path, dev: i32) -> io::Result<()> {
    let entries = match fsattr::read_dir_quick(dir) {
        Ok(e) => e,
        Err(e) if denied(&e) => {
            make_writable(dir);
            fsattr::read_dir_quick(dir)?
        }
        Err(e) => return Err(e),
    };
    let first_error = entries
        .into_par_iter()
        .filter_map(|e| {
            let p = dir.join(&e.name);
            let r = if e.attrs.kind == ObjKind::Dir {
                if e.attrs.dev != dev {
                    Err(io::Error::other(format!("{} is another volume's mount point; left alone", p.display())))
                } else {
                    remove_contents_on(&p, dev).and_then(|_| retry(&p, dir, || std::fs::remove_dir(&p)))
                }
            } else {
                retry(&p, dir, || std::fs::remove_file(&p))
            };
            r.err()
        })
        .reduce_with(|a, _| a);
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

fn denied(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM))
}

/// Runs `f`, and if permissions or a user-immutable flag stop it, lifts them
/// and tries once more.
fn retry(path: &Path, parent: &Path, f: impl Fn() -> io::Result<()>) -> io::Result<()> {
    match f() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) if denied(&e) => {
            make_writable(parent);
            clear_user_flags(path);
            clear_user_flags(parent);
            match f() {
                Err(e2) if e2.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e2) => Err(io::Error::new(e2.kind(), format!("{}: {}", path.display(), e2))),
                Ok(()) => Ok(()),
            }
        }
        Err(e) => Err(io::Error::new(e.kind(), format!("{}: {}", path.display(), e))),
    }
}

fn make_writable(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = std::fs::symlink_metadata(dir) {
        let mode = m.mode() & 0o7777;
        if mode & 0o700 != 0o700 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode | 0o700));
        }
    }
}

fn clear_user_flags(path: &Path) {
    const USER_LOCKS: u32 = 0x2 | 0x4; // UF_IMMUTABLE | UF_APPEND
    if let Ok(m) = std::fs::symlink_metadata(path) {
        let flags = std::os::macos::fs::MetadataExt::st_flags(&m);
        if flags & USER_LOCKS != 0
            && let Ok(c) = fsattr::cpath(path)
        {
            unsafe { libc::lchflags(c.as_ptr(), flags & !USER_LOCKS) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{chmod, write};

    #[test]
    fn removes_trees_with_read_only_folders_and_locked_files() {
        let base = tempfile::tempdir().unwrap();
        let t = base.path().join("modules-1");
        for i in 0..50 {
            write(&t.join(format!("pkg{i}/sub/f{i}.txt")), "x");
        }
        write(&t.join("ro/inner/file"), "y");
        chmod(&t.join("ro/inner"), 0o555);
        chmod(&t.join("ro"), 0o555);
        write(&t.join("locked"), "z");
        let c = fsattr::cpath(&t.join("locked")).unwrap();
        assert_eq!(unsafe { libc::chflags(c.as_ptr(), 0x2) }, 0);
        std::os::unix::fs::symlink(base.path(), t.join("link-out")).unwrap();
        let outside = base.path().join("outside.txt");
        write(&outside, "keep me");

        remove(&t).unwrap();
        assert!(!t.exists());
        assert!(outside.exists(), "symlinks are not followed");
    }

    #[test]
    fn contents_only_keeps_the_folder() {
        let base = tempfile::tempdir().unwrap();
        write(&base.path().join("cache/a/b"), "x");
        write(&base.path().join("cache/c"), "x");
        remove_contents(&base.path().join("cache")).unwrap();
        assert!(base.path().join("cache").is_dir());
        assert_eq!(std::fs::read_dir(base.path().join("cache")).unwrap().count(), 0);
    }

    #[test]
    fn single_files_and_missing_paths() {
        let base = tempfile::tempdir().unwrap();
        let f = base.path().join("cc_trace_x.stop.log");
        write(&f, "x");
        remove(&f).unwrap();
        assert!(!f.exists());
        remove(&f).unwrap();
    }
}

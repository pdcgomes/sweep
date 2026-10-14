//! Measures a tree in parallel: what deleting it frees, what `du` would say,
//! and when anything inside was last written.

use crate::fsattr::{self, Attrs, Entry, ObjKind};
use rayon::prelude::*;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

#[derive(Default)]
pub struct Opts<'a> {
    /// Subtrees to leave out entirely, such as worktrees nested inside another.
    pub exclude: &'a [PathBuf],
    /// A top-level folder whose writes are tracked apart from the rest (`build`).
    pub split: Option<&'a OsStr>,
    /// Also total up the files last written before this time.
    pub old_before: Option<SystemTime>,
    /// Look at dates and types only: private sizes are what make a walk
    /// slow, and the checks just before deleting don't need them.
    pub no_sizes: bool,
    /// Folders and files to leave out wherever they are, by name: a
    /// project's node_modules, or Finder's .DS_Store, judging when the
    /// project's own files were last edited.
    pub skip: &'a [&'a str],
}

#[derive(Clone, Debug, PartialEq)]
pub struct Newest {
    pub time: SystemTime,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    /// Private bytes, i.e. what deleting the tree frees.
    pub freed: u64,
    /// Allocated bytes, as `du` counts them.
    pub du: u64,
    pub files: u64,
    pub dirs: u64,
    /// Newest write to any non-directory.
    pub newest_file: Option<Newest>,
    /// Newest write to a directory, the root included. Only used for trees
    /// with no files at all.
    pub newest_dir: Option<SystemTime>,
    /// Newest write inside the split folder and outside it.
    pub newest_split: Option<Newest>,
    pub newest_outside: Option<Newest>,
    pub old_files: u64,
    pub old_freed: u64,
    pub sockets: Vec<PathBuf>,
    pub mounts: Vec<PathBuf>,
    pub errors: u64,
    pub first_error: Option<String>,
    /// True when the volume reported private sizes for everything.
    pub exact: bool,
}

impl Stats {
    /// When the tree was last written: its newest file, or for a tree with no
    /// files, its newest folder.
    pub fn last_write(&self) -> Option<SystemTime> {
        match &self.newest_file {
            Some(n) => Some(n.time),
            None => self.newest_dir,
        }
    }

    fn error(&mut self, path: &Path, err: std::io::Error) {
        self.errors += 1;
        if self.first_error.is_none() {
            self.first_error = Some(format!("{}: {}", path.display(), err));
        }
    }

    fn merge(mut self, o: Stats) -> Stats {
        self.freed += o.freed;
        self.du += o.du;
        self.files += o.files;
        self.dirs += o.dirs;
        self.newest_file = newer(self.newest_file, o.newest_file);
        self.newest_split = newer(self.newest_split, o.newest_split);
        self.newest_outside = newer(self.newest_outside, o.newest_outside);
        self.newest_dir = match (self.newest_dir, o.newest_dir) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        self.old_files += o.old_files;
        self.old_freed += o.old_freed;
        for (list, more) in [(&mut self.sockets, o.sockets), (&mut self.mounts, o.mounts)] {
            for p in more {
                if list.len() < 8 {
                    list.push(p);
                }
            }
        }
        self.errors += o.errors;
        self.first_error = self.first_error.or(o.first_error);
        self.exact &= o.exact;
        self
    }
}

fn newer(a: Option<Newest>, b: Option<Newest>) -> Option<Newest> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.time > a.time { b } else { a }),
        (a, b) => a.or(b),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Zone {
    Top,
    Split,
    Outside,
}

struct Link {
    seen: u32,
    nlink: u32,
    freed: u64,
    du: u64,
    old: bool,
}

struct Walk<'a> {
    opts: &'a Opts<'a>,
    dev: i32,
    links: Mutex<HashMap<u64, Link>>,
}

/// Measures `root`, which may be a folder, a file or a symlink (never followed).
pub fn measure(root: &Path, opts: &Opts) -> Stats {
    let mut stats = Stats { exact: true, ..Stats::default() };
    let attrs = match fsattr::attrs(root) {
        Ok(a) => a,
        Err(e) => {
            stats.error(root, e);
            return stats;
        }
    };
    let w = Walk { opts, dev: attrs.dev, links: Mutex::new(HashMap::new()) };
    if attrs.kind == ObjKind::Dir {
        stats.dirs = 1;
        stats.newest_dir = Some(attrs.last_write());
        stats = stats.merge(w.dir(root, Zone::Top));
    } else {
        let parent = root.parent().unwrap_or(Path::new("/"));
        let name = root.file_name().unwrap_or_default();
        w.file(&mut stats, parent, name, &attrs, Zone::Top);
    }
    for link in w.links.into_inner().unwrap().into_values() {
        // A hard link frees its data only once every link to it is gone.
        if link.seen >= link.nlink {
            stats.freed += link.freed;
            stats.du += link.du;
            if link.old {
                stats.old_freed += link.freed;
            }
        }
    }
    stats
}

impl Walk<'_> {
    fn dir(&self, dir: &Path, zone: Zone) -> Stats {
        let mut stats = Stats { exact: true, ..Stats::default() };
        let read = if self.opts.no_sizes { fsattr::read_dir_quick } else { fsattr::read_dir };
        let entries = match read(dir) {
            Ok(e) => e,
            Err(e) => {
                stats.error(dir, e);
                return stats;
            }
        };
        let mut subdirs: Vec<(Entry, Zone)> = Vec::new();
        for e in entries {
            if e.attrs.kind == ObjKind::Dir {
                let child = match zone {
                    Zone::Top if Some(e.name.as_os_str()) == self.opts.split => Zone::Split,
                    Zone::Top => Zone::Outside,
                    z => z,
                };
                subdirs.push((e, child));
            } else if !skipped(self.opts, &e.name) {
                self.file(&mut stats, dir, &e.name, &e.attrs, zone);
            }
        }
        let sub = subdirs
            .into_par_iter()
            .map(|(e, zone)| {
                let path = dir.join(&e.name);
                let mut s = Stats { exact: true, ..Stats::default() };
                if self.opts.exclude.contains(&path) || skipped(self.opts, &e.name) {
                    return s;
                }
                if e.attrs.dev != self.dev {
                    s.mounts.push(path);
                    return s;
                }
                s.dirs = 1;
                s.newest_dir = Some(e.attrs.last_write());
                s.merge(self.dir(&path, zone))
            })
            .reduce(Stats::default_exact, Stats::merge);
        stats.merge(sub)
    }

    fn file(&self, stats: &mut Stats, dir: &Path, name: &OsStr, a: &Attrs, zone: Zone) {
        let t = a.last_write();
        stats.files += 1;
        if a.private.is_none() && a.kind == ObjKind::File {
            stats.exact = false;
        }
        let old = self.opts.old_before.is_some_and(|cut| t < cut);
        if a.nlink > 1 && a.kind == ObjKind::File {
            let mut links = self.links.lock().unwrap();
            let l =
                links.entry(a.fileid).or_insert(Link { seen: 0, nlink: a.nlink, freed: a.freed(), du: a.alloc, old });
            l.seen += 1;
        } else {
            stats.freed += a.freed();
            stats.du += a.alloc;
            if old {
                stats.old_freed += a.freed();
            }
        }
        if old {
            stats.old_files += 1;
        }
        if a.kind == ObjKind::Socket && stats.sockets.len() < 8 {
            stats.sockets.push(dir.join(name));
        }
        let beats = |n: &Option<Newest>| n.as_ref().is_none_or(|n| t > n.time);
        if beats(&stats.newest_file) {
            stats.newest_file = Some(Newest { time: t, path: dir.join(name) });
        }
        let slot = if zone == Zone::Split { &mut stats.newest_split } else { &mut stats.newest_outside };
        if beats(slot) {
            *slot = Some(Newest { time: t, path: dir.join(name) });
        }
    }
}

impl Stats {
    fn default_exact() -> Stats {
        Stats { exact: true, ..Stats::default() }
    }
}

fn skipped(opts: &Opts, name: &std::ffi::OsStr) -> bool {
    !opts.skip.is_empty() && opts.skip.iter().any(|s| name == *s)
}

/// A file written at or after the cutoff for its part of the tree (`split`
/// folder or the rest), found without measuring sizes and stopping at the
/// first one. Most busy trees answer from their first folder.
pub fn recent(root: &Path, opts: &Opts, cutoff: SystemTime, split_cutoff: SystemTime) -> Option<(Newest, bool)> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let found: Mutex<Option<(Newest, bool)>> = Mutex::new(None);
    let stop = AtomicBool::new(false);
    let attrs = fsattr::attrs(root).ok()?;
    if attrs.kind != ObjKind::Dir {
        return (attrs.last_write() >= cutoff)
            .then(|| (Newest { time: attrs.last_write(), path: root.to_path_buf() }, false));
    }
    fn go(
        dir: &Path,
        zone: Zone,
        dev: i32,
        opts: &Opts,
        cut: (SystemTime, SystemTime),
        stop: &AtomicBool,
        found: &Mutex<Option<(Newest, bool)>>,
    ) {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entries) = fsattr::read_dir_quick(dir) else { return };
        let mut subdirs = Vec::new();
        for e in entries {
            if e.attrs.kind == ObjKind::Dir {
                let child = match zone {
                    Zone::Top if Some(e.name.as_os_str()) == opts.split => Zone::Split,
                    Zone::Top => Zone::Outside,
                    z => z,
                };
                subdirs.push((e, child));
                continue;
            }
            if skipped(opts, &e.name) {
                continue;
            }
            let limit = if zone == Zone::Split { cut.1 } else { cut.0 };
            if e.attrs.last_write() >= limit {
                stop.store(true, Ordering::Relaxed);
                found.lock().unwrap().get_or_insert((
                    Newest { time: e.attrs.last_write(), path: dir.join(&e.name) },
                    zone == Zone::Split,
                ));
                return;
            }
        }
        subdirs.into_par_iter().for_each(|(e, z)| {
            let p = dir.join(&e.name);
            if e.attrs.dev == dev && !opts.exclude.contains(&p) && !skipped(opts, &e.name) {
                go(&p, z, dev, opts, cut, stop, found);
            }
        });
    }
    go(root, Zone::Top, attrs.dev, opts, (cutoff, split_cutoff), &stop, &found);
    found.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{set_mtime, write_random};
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn a_clone_of_a_file_outside_the_tree_counts_as_nothing() {
        let base = tempfile::tempdir().unwrap();
        let keep = base.path().join("fixtures");
        let copy = base.path().join("fixture-copy");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(copy.join("deep")).unwrap();
        write_random(&keep.join("big.raw"), 3 << 20);
        write_random(&copy.join("own.bin"), 1 << 20);
        let st = Command::new("cp").arg("-c").arg(keep.join("big.raw")).arg(copy.join("deep/big.raw")).status();
        assert!(st.unwrap().success());

        let s = measure(&copy, &Opts::default());
        assert_eq!(s.files, 2);
        assert!(s.du >= 4 << 20, "du-style total counts the clone: {}", s.du);
        assert_eq!(s.freed, 1 << 20, "only the file it owns would be freed");
        assert!(s.exact);
    }

    #[test]
    fn hard_links_count_once_and_only_when_every_link_is_inside() {
        let base = tempfile::tempdir().unwrap();
        let tree = base.path().join("tree");
        std::fs::create_dir_all(tree.join("a")).unwrap();
        write_random(&tree.join("a/both.bin"), 1 << 20);
        std::fs::hard_link(tree.join("a/both.bin"), tree.join("both-again.bin")).unwrap();
        write_random(&tree.join("shared.bin"), 2 << 20);
        std::fs::hard_link(tree.join("shared.bin"), base.path().join("outside.bin")).unwrap();

        let s = measure(&tree, &Opts::default());
        assert_eq!(s.freed, 1 << 20);
    }

    #[test]
    fn newest_write_comes_from_files_not_folder_dates() {
        let base = tempfile::tempdir().unwrap();
        let t = base.path().join("t");
        std::fs::create_dir_all(t.join("sub")).unwrap();
        std::fs::write(t.join("sub/old.txt"), "x").unwrap();
        std::fs::write(t.join("new.txt"), "y").unwrap();
        let now = SystemTime::now();
        let future = now + Duration::from_secs(3600);
        set_mtime(&t.join("new.txt"), future);
        // A folder date in the future must not count as a write.
        set_mtime(&t.join("sub"), now + Duration::from_secs(99_999));
        set_mtime(&t, now + Duration::from_secs(99_999));

        let s = measure(&t, &Opts::default());
        let n = s.newest_file.clone().unwrap();
        assert_eq!(n.path, t.join("new.txt"));
        assert!(n.time >= future);
        assert!(s.last_write().unwrap() < now + Duration::from_secs(99_000));
    }

    #[test]
    fn a_tree_without_files_falls_back_to_its_folders() {
        let base = tempfile::tempdir().unwrap();
        let t = base.path().join("TemporaryDirectory.abc");
        std::fs::create_dir_all(t.join("x/y")).unwrap();
        let s = measure(&t, &Opts::default());
        assert_eq!(s.files, 0);
        assert_eq!(s.dirs, 3);
        assert!(s.last_write().is_some());
    }

    #[test]
    fn split_folder_and_exclusions() {
        let base = tempfile::tempdir().unwrap();
        let wt = base.path().join("wt");
        std::fs::create_dir_all(wt.join("build/obj")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::create_dir_all(wt.join("build/nested-wt")).unwrap();
        std::fs::write(wt.join("src/a.rs"), "a").unwrap();
        std::fs::write(wt.join("build/obj/a.o"), "o").unwrap();
        std::fs::write(wt.join("build/nested-wt/huge"), vec![0u8; 100_000]).unwrap();
        let later = SystemTime::now() + Duration::from_secs(500);
        set_mtime(&wt.join("build/obj/a.o"), later);

        let exclude = [wt.join("build/nested-wt")];
        let s = measure(&wt, &Opts { exclude: &exclude, split: Some(OsStr::new("build")), ..Opts::default() });
        assert_eq!(s.files, 2);
        assert_eq!(s.newest_split.unwrap().path, wt.join("build/obj/a.o"));
        assert_eq!(s.newest_outside.unwrap().path, wt.join("src/a.rs"));
    }

    #[test]
    fn old_files_are_totalled_separately() {
        let base = tempfile::tempdir().unwrap();
        let d = base.path().join("logs");
        std::fs::create_dir_all(&d).unwrap();
        write_random(&d.join("a.log"), 100_000);
        write_random(&d.join("b.log"), 50_000);
        let soon = SystemTime::now() + Duration::from_secs(1000);
        set_mtime(&d.join("b.log"), soon + Duration::from_secs(1000));
        let s = measure(&d, &Opts { old_before: Some(soon), ..Opts::default() });
        assert_eq!(s.old_files, 1);
        assert_eq!(s.old_freed, crate::fsattr::attrs(&d.join("a.log")).unwrap().freed());
    }

    #[test]
    fn the_recency_probe_stops_at_a_recent_file_and_respects_the_split() {
        let base = tempfile::tempdir().unwrap();
        let wt = base.path().join("wt");
        for i in 0..30 {
            std::fs::create_dir_all(wt.join(format!("src/m{i}/deep/er"))).unwrap();
            std::fs::write(wt.join(format!("src/m{i}/deep/er/f.rs")), "x").unwrap();
        }
        std::fs::create_dir_all(wt.join("build/out")).unwrap();
        std::fs::write(wt.join("build/out/app"), "x").unwrap();
        let now = SystemTime::now() + Duration::from_secs(10 * 86_400);
        let old = now - Duration::from_secs(2 * 86_400);
        let opts = Opts { split: Some(OsStr::new("build")), ..Opts::default() };
        assert_eq!(recent(&wt, &opts, old, old), None);

        set_mtime(&wt.join("src/m17/deep/er/f.rs"), now - Duration::from_secs(60));
        let (n, in_build) = recent(&wt, &opts, old, old).unwrap();
        assert_eq!(n.path, wt.join("src/m17/deep/er/f.rs"));
        assert!(!in_build);

        set_mtime(&wt.join("src/m17/deep/er/f.rs"), old - Duration::from_secs(60));
        set_mtime(&wt.join("build/out/app"), now - Duration::from_secs(5 * 86_400));
        assert_eq!(recent(&wt, &opts, old, old), None, "a build older than its own cutoff");
        let (n, in_build) = recent(&wt, &opts, old, now - Duration::from_secs(6 * 86_400)).unwrap();
        assert_eq!(n.path, wt.join("build/out/app"));
        assert!(in_build);
    }

    #[test]
    fn unreadable_folders_are_reported() {
        let base = tempfile::tempdir().unwrap();
        let d = base.path().join("d");
        std::fs::create_dir_all(d.join("locked")).unwrap();
        std::fs::write(d.join("locked/f"), "x").unwrap();
        crate::testutil::chmod(&d.join("locked"), 0o000);
        let s = measure(&d, &Opts::default());
        crate::testutil::chmod(&d.join("locked"), 0o755);
        assert_eq!(s.errors, 1);
        assert!(s.first_error.unwrap().contains("locked"));
    }
}

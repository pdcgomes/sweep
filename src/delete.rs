//! Deletes the chosen items in parallel. Every item is checked again just
//! before it goes, and each one is recorded in ~/Library/Logs/sweep/<date>.log.

use crate::ctx::Ctx;
use crate::fmt;
use crate::model::{Badge, Item, Plan, Verdict};
use crate::rules::{self, DeleteOpts};
use rayon::ThreadPool;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

pub struct Job {
    pub item: Item,
    /// The badge the item had when it was chosen.
    pub seen: Badge,
    pub opts: DeleteOpts,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Deleted { freed: u64, note: String },
    WouldDelete { freed: u64, note: String },
    Skipped(String),
    Failed(String),
}

pub enum Progress {
    Started(usize),
    Finished(usize, Outcome),
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    pub deleted: usize,
    pub freed: u64,
    pub skipped: usize,
    pub failed: usize,
    pub dry_run: bool,
    pub cancelled: bool,
    pub log: Option<PathBuf>,
    /// Free space gained on each volume, measured before and after.
    pub space: Vec<(String, i64)>,
}

pub fn run(
    ctx: &Ctx,
    pool: &ThreadPool,
    jobs: Vec<Job>,
    dry_run: bool,
    cancel: &AtomicBool,
    tx: &Sender<Progress>,
) -> Summary {
    let mut fresh = ctx.refreshed();
    fresh.sizes = false;
    let expected: u64 = jobs.iter().filter_map(|j| j.item.bytes()).sum();
    let log = if dry_run { None } else { Log::open(&fresh) };
    if let Some(l) = &log {
        l.line(&format!(
            "run started: {} selected, {} expected",
            fmt::count(jobs.len(), "item", "items"),
            fmt::bytes(expected)
        ));
    }
    let before = volumes(jobs.iter().map(|j| j.item.path.as_path()));
    let summary = Mutex::new(Summary { dry_run, log: log.as_ref().map(|l| l.path.clone()), ..Summary::default() });
    pool.in_place_scope_fifo(|s| {
        for job in &jobs {
            let (fresh, log, summary) = (&fresh, &log, &summary);
            s.spawn_fifo(move |_| {
                let outcome = if cancel.load(Ordering::SeqCst) {
                    Outcome::Skipped("cancelled".into())
                } else {
                    let _ = tx.send(Progress::Started(job.item.id));
                    one(fresh, job, dry_run)
                };
                if let Some(l) = log {
                    l.record(fresh, &job.item, &outcome);
                }
                {
                    let mut s = summary.lock().unwrap();
                    match &outcome {
                        Outcome::Deleted { freed, .. } | Outcome::WouldDelete { freed, .. } => {
                            s.deleted += 1;
                            s.freed += freed;
                        }
                        Outcome::Skipped(_) => s.skipped += 1,
                        Outcome::Failed(_) => s.failed += 1,
                    }
                }
                let _ = tx.send(Progress::Finished(job.item.id, outcome));
            });
        }
    });
    let mut summary = summary.into_inner().unwrap();
    summary.cancelled = cancel.load(Ordering::SeqCst);
    if !dry_run {
        let after = volumes(jobs.iter().map(|j| j.item.path.as_path()));
        for (key, (name, free)) in &before {
            if let Some((_, now)) = after.get(key) {
                summary.space.push((name.clone(), *now as i64 - *free as i64));
            }
        }
    }
    if let Some(l) = &log {
        let space: Vec<String> = summary
            .space
            .iter()
            .map(|(v, d)| format!("{v} {}{}", if *d < 0 { "-" } else { "+" }, fmt::bytes(d.unsigned_abs())))
            .collect();
        l.line(&format!(
            "run finished: {} deleted, {} freed, {} skipped, {} failed{}; free space {}",
            summary.deleted,
            fmt::bytes(summary.freed),
            summary.skipped,
            summary.failed,
            if summary.cancelled { " (cancelled)" } else { "" },
            space.join(", ")
        ));
    }
    summary
}

/// Checks the item again, then deletes it, or in a dry run says what would happen.
fn one(ctx: &Ctx, job: &Job, dry_run: bool) -> Outcome {
    let v = rules::evaluate(ctx, &job.item);
    if v.hidden && v.badge == Badge::Kept {
        return Outcome::Skipped(v.reason);
    }
    if v.badge == Badge::Kept {
        return Outcome::Skipped(format!("now kept: {}", v.reason));
    }
    if job.seen == Badge::Safe && v.badge == Badge::Review {
        return Outcome::Skipped(format!("changed since the scan: {}", v.reason));
    }
    let freed = job.item.bytes().unwrap_or(0);
    if dry_run {
        return Outcome::WouldDelete { freed, note: plan_note(&v, job.opts) };
    }
    match rules::delete(ctx, &job.item, &v, job.opts) {
        Ok(note) => Outcome::Deleted { freed, note },
        Err(e) => Outcome::Failed(format!("{e:#}")),
    }
}

/// What a deletion will do besides removing the item.
pub fn plan_note(v: &Verdict, opts: DeleteOpts) -> String {
    match &v.plan {
        Plan::Worktree { dirty: true, backup_ref, .. } if opts.backup => {
            format!("saves uncommitted work on {backup_ref} first")
        }
        Plan::Worktree { dirty: true, branch_head: true, backup_ref, .. } => {
            format!("discards uncommitted work; keeps HEAD on {backup_ref}")
        }
        Plan::Worktree { dirty: true, .. } => "discards uncommitted work".into(),
        Plan::Worktree { branch_head: true, backup_ref, .. } => format!("keeps HEAD on {backup_ref} first"),
        _ => String::new(),
    }
}

/// Free bytes per volume, keyed so that APFS volumes sharing a container
/// (and so sharing free space) count once.
fn volumes<'a>(paths: impl Iterator<Item = &'a Path>) -> BTreeMap<(u64, u64), (String, u64)> {
    let mut map = BTreeMap::new();
    for p in paths {
        let probe = p.ancestors().find(|a| a.exists()).unwrap_or(Path::new("/"));
        if let Some(v) = crate::fsattr::volume(probe) {
            map.entry(v.key).or_insert((v.mount, v.free));
        }
    }
    map
}

struct Log {
    path: PathBuf,
    file: Mutex<std::fs::File>,
}

impl Log {
    fn open(ctx: &Ctx) -> Option<Log> {
        let dir = ctx.home.join("Library/Logs/sweep");
        std::fs::create_dir_all(&dir).ok()?;
        let date = chrono::DateTime::<chrono::Local>::from(ctx.now).format("%Y-%m-%d");
        let path = dir.join(format!("{date}.log"));
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
        Some(Log { path, file: Mutex::new(file) })
    }

    fn line(&self, text: &str) {
        let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let mut f = self.file.lock().unwrap();
        let _ = writeln!(f, "{stamp}  {text}");
    }

    fn record(&self, ctx: &Ctx, item: &Item, outcome: &Outcome) {
        let what = format!("[{}] {}", item.kind.slug(), ctx.tilde(&item.path));
        let text = match outcome {
            Outcome::Deleted { freed, note } => {
                let reason = item.verdict.as_ref().map(|v| v.reason.as_str()).unwrap_or("");
                let note = if note.is_empty() { String::new() } else { format!("; {note}") };
                format!("deleted {what}: freed {} ({reason}{note})", fmt::bytes(*freed))
            }
            Outcome::WouldDelete { freed, .. } => format!("would delete {what}: {}", fmt::bytes(*freed)),
            Outcome::Skipped(why) => format!("skipped {what}: {why}"),
            Outcome::Failed(err) => format!("FAILED {what}: {err}"),
        };
        self.line(&text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::testing::scan;
    use crate::rules::tmp;
    use crate::testutil::{set_mtime, write, write_random};
    use std::time::{Duration, SystemTime};

    fn setup(n: usize) -> (tempfile::TempDir, PathBuf, SystemTime) {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        for i in 0..n {
            write(&home.join(format!("T/TemporaryDirectory.{i:04}/a/b/file")), "x");
        }
        write_random(&home.join("T/tmpbig/blob"), 2_000_000);
        write(&home.join("T/tmpchanged/data"), "x");
        (dir, home, SystemTime::now() + Duration::from_secs(5 * 86_400 + 60))
    }

    fn jobs(items: &[Item]) -> Vec<Job> {
        items.iter().map(|i| Job { item: i.clone(), seen: i.badge().unwrap(), opts: DeleteOpts::default() }).collect()
    }

    #[test]
    fn deletes_in_parallel_rechecks_each_item_and_logs_it() {
        let (_dir, home, now) = setup(300);
        let ctx = crate::ctx::test_ctx(&home, now);
        let (items, _) = scan(&ctx, tmp::discover);
        assert!(items.iter().all(|i| i.badge() == Some(Badge::Safe)));
        // Something writes into one of them after the scan.
        set_mtime(&home.join("T/tmpchanged/data"), now - Duration::from_secs(60));

        let (tx, rx) = std::sync::mpsc::channel();
        let summary = run(&ctx, &crate::scan::pool(), jobs(&items), false, &AtomicBool::new(false), &tx);
        drop(tx);
        assert_eq!(summary.deleted, 301);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.failed, 0);
        assert!(summary.freed >= 2_000_000);
        assert!(!home.join("T/TemporaryDirectory.0000").exists());
        assert!(!home.join("T/tmpbig").exists());
        assert!(home.join("T/tmpchanged/data").exists());
        let finished = rx.iter().filter(|p| matches!(p, Progress::Finished(..))).count();
        assert_eq!(finished, 302);

        let log = std::fs::read_to_string(summary.log.unwrap()).unwrap();
        assert_eq!(log.matches("deleted [temp] ").count(), 301);
        assert!(log.contains("skipped [temp] ~/T/tmpchanged: now kept: in use: written 1 min ago"), "{log}");
        assert!(log.contains("deleted [temp] ~/T/tmpbig: freed 2.0 MB"), "{log}");
        assert!(log.contains("run finished: 301 deleted"), "{log}");
    }

    #[test]
    fn a_dry_run_deletes_nothing_and_writes_no_log() {
        let (_dir, home, now) = setup(20);
        let ctx = crate::ctx::test_ctx(&home, now);
        let (items, _) = scan(&ctx, tmp::discover);
        let (tx, rx) = std::sync::mpsc::channel();
        let summary = run(&ctx, &crate::scan::pool(), jobs(&items), true, &AtomicBool::new(false), &tx);
        drop(tx);
        assert_eq!(summary.deleted, 22);
        assert!(summary.log.is_none());
        assert!(home.join("T/TemporaryDirectory.0000").exists());
        assert!(!home.join("Library/Logs/sweep").exists());
        assert!(rx.iter().all(|p| !matches!(p, Progress::Finished(_, Outcome::Deleted { .. }))));
    }
}

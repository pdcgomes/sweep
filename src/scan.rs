//! Runs every rule at once. Items stream out as each rule finds them, and
//! their verdicts follow as the measuring finishes.

use crate::ctx::Ctx;
use crate::model::{Item, Kind, Target, Verdict};
use crate::rules::{self, Sink};
use rayon::{ScopeFifo, ThreadPool};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};

pub enum Event {
    /// A new item, or one whose details changed (it keeps its id).
    Found(Item),
    Measured(usize, Verdict),
    /// The item isn't there any more, or its rule no longer finds it.
    Gone(usize),
    Note(Kind, String),
    /// A rule is looking again: its notes so far no longer stand.
    ClearNotes(Kind),
    /// A rule has found everything it's going to; its measuring may go on.
    Looked(Kind),
    Done,
    /// Whether changes are being watched for, or why they can't be.
    Watching(Result<(), String>),
}

/// Hands out item ids: new ones on a first scan, and the same ones again for
/// the items a later scan finds again.
pub trait Ids: Sync {
    fn id(&self, kind: Kind, path: &Path, label: &str) -> usize;
}

#[derive(Default)]
pub struct Fresh(AtomicUsize);

impl Ids for Fresh {
    fn id(&self, _: Kind, _: &Path, _: &str) -> usize {
        self.0.fetch_add(1, Ordering::SeqCst)
    }
}

/// Metadata calls are I/O bound and scale past the core count. Walks recurse,
/// and a worker waiting on a join runs other walks on the same stack, so the
/// stacks are generous.
pub fn pool() -> Arc<ThreadPool> {
    let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).clamp(8, 24);
    named_pool("sweep", n)
}

/// APFS unlinks fastest with about eight threads at once; more queue on its
/// metadata locks and get slower (16 threads took half as long again, 64
/// five times as long, deleting 60,000 small files).
pub fn delete_pool() -> Arc<ThreadPool> {
    named_pool("sweep-rm", 8)
}

/// A pool of its own keeps one kind of work (deleting, say) from queueing
/// behind another (measuring a million-file folder).
pub fn named_pool(name: &'static str, threads: usize) -> Arc<ThreadPool> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(64 << 20)
        .thread_name(move |i| format!("{name}-{i}"))
        .build();
    Arc::new(pool.expect("thread pool"))
}

/// For work nobody is waiting on: at utility priority macOS gives the
/// threads less CPU and lets everyone else's disk reads go first.
pub fn background_pool(name: &'static str, threads: usize) -> Arc<ThreadPool> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .stack_size(64 << 20)
        .thread_name(move |i| format!("{name}-{i}"))
        .start_handler(|_| background())
        .build();
    Arc::new(pool.expect("thread pool"))
}

/// Puts the calling thread at utility priority.
pub fn background() {
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }
}

struct EngineSink<'r, 'd, 's> {
    ctx: &'d Ctx,
    tx: &'d Sender<Event>,
    scope: &'r ScopeFifo<'s>,
    ids: &'d dyn Ids,
    later: &'d Mutex<Vec<Item>>,
    measure_kept: bool,
}

impl<'r, 'd: 's, 's> Sink for EngineSink<'r, 'd, 's> {
    fn item(&self, kind: Kind, path: PathBuf, label: String, target: Target) {
        let id = self.ids.id(kind, &path, &label);
        let item = Item { id, kind, path, label, target, verdict: None };
        let _ = self.tx.send(Event::Found(item.clone()));
        if let Some(v) = rules::precheck(self.ctx, &item) {
            let _ = self.tx.send(Event::Measured(id, v));
            if self.measure_kept {
                self.later.lock().unwrap().push(item);
            }
            return;
        }
        let (ctx, tx, later, measure_kept) = (self.ctx, self.tx, self.later, self.measure_kept);
        self.scope.spawn_fifo(move |_| {
            if let Some(v) = rules::busy(ctx, &item) {
                let _ = tx.send(Event::Measured(id, v));
                if measure_kept {
                    later.lock().unwrap().push(item);
                }
                return;
            }
            let v = rules::evaluate(ctx, &item);
            let _ = tx.send(Event::Measured(id, v));
        });
    }

    fn note(&self, kind: Kind, text: String) {
        let _ = self.tx.send(Event::Note(kind, text));
    }
}

/// Runs the scan. Items kept for reasons known up front are reported at once;
/// with `measure_kept` their sizes follow after everything else.
pub fn run(ctx: &Ctx, pool: &ThreadPool, tx: &Sender<Event>, measure_kept: bool) {
    run_with(ctx, pool, tx, measure_kept, &Fresh::default());
}

/// The same, with ids from `ids`.
pub fn run_with(ctx: &Ctx, pool: &ThreadPool, tx: &Sender<Event>, measure_kept: bool, ids: &dyn Ids) {
    let later = Mutex::new(Vec::new());
    pool.in_place_scope_fifo(|s| {
        std::thread::scope(|ts| {
            for rule in &rules::RULES {
                let sink = EngineSink { ctx, tx, scope: s, ids, later: &later, measure_kept };
                ts.spawn(move || {
                    (rule.discover)(ctx, &sink);
                    for k in rule.kinds {
                        let _ = tx.send(Event::Looked(*k));
                    }
                });
            }
        });
    });
    let later = later.into_inner().unwrap();
    pool.in_place_scope_fifo(|s| {
        for item in later {
            s.spawn_fifo(move |_| {
                let v = rules::evaluate(ctx, &item);
                let _ = tx.send(Event::Measured(item.id, v));
            });
        }
    });
    let _ = tx.send(Event::Done);
}

/// Scans to the end and returns every item, checked, in discovery order.
pub fn collect(ctx: &Ctx, pool: &ThreadPool, measure_kept: bool) -> (Vec<Item>, Vec<(Kind, String)>) {
    let (tx, rx) = channel();
    run(ctx, pool, &tx, measure_kept);
    drop(tx);
    let mut items: Vec<Option<Item>> = Vec::new();
    let mut notes = Vec::new();
    for ev in rx {
        match ev {
            Event::Found(i) => {
                let id = i.id;
                if items.len() <= id {
                    items.resize_with(id + 1, || None);
                }
                items[id] = Some(i);
            }
            Event::Measured(id, v) => {
                if let Some(Some(i)) = items.get_mut(id) {
                    i.verdict = Some(v);
                }
            }
            Event::Note(k, t) => notes.push((k, t)),
            Event::Gone(_) | Event::ClearNotes(_) | Event::Looked(_) | Event::Done | Event::Watching(_) => {}
        }
    }
    (items.into_iter().flatten().collect(), notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Badge;
    use crate::testutil::write;
    use std::time::{Duration, SystemTime};

    #[test]
    fn kept_items_are_reported_without_measuring_unless_asked() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        write(&home.join("T/tmpold/f"), "x");
        write(&home.join("T/tmpbusy/a/b/f"), "x");
        write(&home.join(".npm/_cacache/x"), "cache");
        let now = SystemTime::now() + Duration::from_secs(9 * 86_400);
        crate::testutil::set_mtime(&home.join("T/tmpbusy/a/b/f"), now - Duration::from_secs(7200));
        let npm = crate::procs::Proc { pid: 5, cmd: "npm install".into(), exe: None, cwd: None };
        let ctx = crate::ctx::test_ctx(&home, now).at(now, crate::procs::Procs::from_list(vec![npm]));
        let pool = pool();
        let mine = |items: &[Item]| -> Vec<(String, Badge, bool)> {
            let mut v: Vec<_> = items
                .iter()
                .filter(|i| i.path.starts_with(&home))
                .map(|i| {
                    let v = i.verdict.as_ref().unwrap();
                    (i.label.clone(), v.badge, v.unmeasured)
                })
                .collect();
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        let (items, _) = collect(&ctx, &pool, false);
        assert_eq!(
            mine(&items),
            [
                ("npm cache · ~/.npm".to_string(), Badge::Kept, true),
                ("tmpbusy".to_string(), Badge::Kept, true),
                ("tmpold".to_string(), Badge::Safe, false)
            ]
        );
        let busy = items.iter().find(|i| i.label == "tmpbusy").unwrap().verdict.clone().unwrap();
        assert_eq!(busy.reason, "in use: written 2 h ago");
        let (items, _) = collect(&ctx, &pool, true);
        for label in ["npm cache · ~/.npm", "tmpbusy"] {
            let v = items.iter().find(|i| i.label == label).unwrap().verdict.clone().unwrap();
            assert_eq!(v.badge, Badge::Kept);
            assert!(!v.unmeasured, "{label} measured afterwards");
            assert!(v.bytes.unwrap() > 0);
        }
    }
}

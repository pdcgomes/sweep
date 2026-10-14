//! Keeps the list up to date while sweep stays open. FSEvents names each
//! file that changes; only the item it's in is looked at again, doing as
//! little as the change allows (a fresh write is one lookup), and a new
//! entry in a folder a rule lists becomes an item without listing the folder
//! again. The clock and the process list are watched too, for verdicts that
//! change with time or with what runs. All of it runs at utility priority,
//! a few threads at most.

use crate::ctx::Ctx;
use crate::model::{Item, Kind, Target, Verdict};
use crate::procs::{Proc, Procs};
use crate::rules::{self, Found, RULES, Sink, Why};
use crate::scan::{self, Event, Ids};
use crate::watch::{self, Change, Watcher};
use rayon::ThreadPool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// What the interface asks for.
#[derive(Debug)]
pub enum Control {
    /// A deletion is under way: stay out of its way until `Resume`.
    Pause,
    Resume,
    /// Look at these again now: they were just deleted, or in a dry run weren't.
    Recheck(Vec<usize>),
    /// Look at everything again.
    Rescan,
    /// Measure a kept item the cursor rests on.
    Measure(usize),
}

#[derive(Clone)]
pub struct Handle(Sender<Msg>);

impl Handle {
    pub fn send(&self, c: Control) {
        let _ = self.0.send(Msg::Control(c));
    }
}

/// FSEvents holds a change this long to report it with the ones after it.
const LATENCY: Duration = Duration::from_secs(1);
const TICK: Duration = Duration::from_millis(500);
/// An item is looked at once its changes stop for this long…
const QUIET: Duration = Duration::from_secs(2);
/// …or this long after the first, if they don't…
const MAX_WAIT: Duration = Duration::from_secs(10);
/// …and never more often than this.
const ITEM_GAP: Duration = Duration::from_secs(5);
const PROCS_EVERY: Duration = Duration::from_secs(60);
/// How often to see whether a watched folder appeared or went (a volume mounted).
const ROOTS_EVERY: Duration = Duration::from_secs(60);
/// Items looked at side by side.
const JOBS: usize = 4;
/// Changed paths remembered per item.
const PATHS: usize = 16;

/// Runs the first scan and then keeps its results up to date, sending both
/// as scan events.
pub fn spawn(ctx: Arc<Ctx>, pool: Arc<ThreadPool>) -> (Receiver<Event>, Handle) {
    let (ui, events) = channel();
    let (tx, rx) = channel();
    let handle = Handle(tx.clone());
    std::thread::Builder::new()
        .name("sweep-live".into())
        .spawn(move || run(ctx, pool, ui, tx, rx))
        .expect("live thread");
    (events, handle)
}

enum Msg {
    Control(Control),
    Changes(Vec<Change>),
    Checked {
        id: usize,
        version: u64,
        asked: bool,
        verdict: Option<Verdict>,
    },
    Listed {
        rule: usize,
        found: Vec<Found>,
        notes: Vec<(Kind, String)>,
    },
    Procs(Procs),
    /// An event from a rescan under way.
    Scan(Event),
}

type Key = (Kind, PathBuf, String);

fn key(i: &Item) -> Key {
    (i.kind, i.path.clone(), i.label.clone())
}

/// Snapshots and runtimes are named by a tool, not by a folder of their own.
fn backed(i: &Item) -> bool {
    !matches!(i.target, Target::SimRuntime(_) | Target::Snapshot { .. })
}

enum Job {
    Check { id: usize, version: u64, item: Box<Item>, why: Why, asked: bool },
    List(usize),
    Procs,
}

impl Job {
    fn asked(&self) -> bool {
        matches!(self, Job::Check { asked: true, .. })
    }
}

fn work(job: Job, ctx: &Ctx) -> Msg {
    match job {
        Job::Check { id, version, item, why, asked } => {
            Msg::Checked { id, version, asked, verdict: rules::recheck(ctx, &item, &why) }
        }
        Job::List(rule) => {
            let sink = Collect::default();
            (RULES[rule].discover)(ctx, &sink);
            Msg::Listed { rule, found: sink.found.into_inner().unwrap(), notes: sink.notes.into_inner().unwrap() }
        }
        Job::Procs => Msg::Procs(Procs::snapshot()),
    }
}

#[derive(Default)]
struct Collect {
    found: Mutex<Vec<Found>>,
    notes: Mutex<Vec<(Kind, String)>>,
}

impl Sink for Collect {
    fn item(&self, kind: Kind, path: PathBuf, label: String, target: Target) {
        self.found.lock().unwrap().push((kind, path, label, target));
    }
    fn note(&self, kind: Kind, text: String) {
        self.notes.lock().unwrap().push((kind, text));
    }
}

/// A rescan hands the items it finds again their old ids.
struct Keyed {
    known: HashMap<Key, usize>,
    next: AtomicUsize,
}

impl Ids for Keyed {
    fn id(&self, kind: Kind, path: &Path, label: &str) -> usize {
        match self.known.get(&(kind, path.to_path_buf(), label.to_string())) {
            Some(&id) => id,
            None => self.next.fetch_add(1, Ordering::SeqCst),
        }
    }
}

/// What has changed about an item since it was last looked at.
struct Pending {
    /// The latest files and folders to change in it.
    paths: Vec<PathBuf>,
    anything: bool,
    /// Time passed, the processes changed, or it's new.
    again: bool,
    first: Instant,
    last: Instant,
}

impl Pending {
    fn settled(&self, now: Instant) -> bool {
        (self.again && self.paths.is_empty() && !self.anything)
            || now.duration_since(self.last) >= QUIET
            || now.duration_since(self.first) >= MAX_WAIT
    }

    fn why(self) -> Why {
        if self.anything {
            Why::Anything
        } else if !self.paths.is_empty() {
            Why::Changed(self.paths)
        } else {
            Why::Again
        }
    }
}

struct Entry {
    item: Item,
    /// Bumped with each new verdict, so a look that began before it is dropped.
    version: u64,
    /// For an item kept while an app or tool runs, whether it was running.
    owner: Option<bool>,
    pending: Option<Pending>,
    checked: Option<Instant>,
    due: Option<SystemTime>,
    busy: bool,
}

struct RuleState {
    /// First and latest change that could alter what it finds.
    pending: Option<(Instant, Instant)>,
    listed: Instant,
    busy: bool,
}

struct Engine {
    base: Arc<Ctx>,
    procs: Procs,
    job_ctx: Option<Arc<Ctx>>,
    entries: HashMap<usize, Entry>,
    keys: HashMap<Key, usize>,
    /// Items by their path, for the changes inside them.
    paths: HashMap<PathBuf, Vec<usize>>,
    next: usize,
    rules: Vec<RuleState>,
    /// Checks the person asked for, ahead of everything else.
    asked: Vec<(usize, Why)>,
    jobs: usize,
    paused: bool,
    /// The ids a rescan under way has found again, and the kinds it had
    /// something to say about.
    rescanning: Option<(HashSet<usize>, HashSet<Kind>)>,
    want_rescan: bool,
    roots_soon: bool,
    procs_at: Instant,
    procs_busy: bool,
    procs_every: Duration,
    /// Each rule's `every`.
    every: Vec<Duration>,
    mono: Instant,
    wall: SystemTime,
    ui: Sender<Event>,
    closed: bool,
}

impl Engine {
    fn new(base: Arc<Ctx>, ui: Sender<Event>) -> Engine {
        let mono = Instant::now();
        Engine {
            procs: base.procs.clone(),
            wall: base.now,
            base,
            job_ctx: None,
            entries: HashMap::new(),
            keys: HashMap::new(),
            paths: HashMap::new(),
            next: 0,
            rules: RULES.iter().map(|_| RuleState { pending: None, listed: mono, busy: false }).collect(),
            asked: Vec::new(),
            jobs: 0,
            paused: false,
            rescanning: None,
            want_rescan: false,
            roots_soon: false,
            procs_at: mono,
            procs_busy: false,
            procs_every: PROCS_EVERY,
            every: RULES.iter().map(|r| r.every).collect(),
            mono,
            ui,
            closed: false,
        }
    }

    fn emit(&mut self, ev: Event) {
        if self.ui.send(ev).is_err() {
            self.closed = true;
        }
    }

    fn clock(&mut self, mono: Instant, wall: SystemTime) {
        self.mono = mono;
        self.wall = wall;
        let stale = self.job_ctx.as_ref().is_some_and(|c| {
            let drift = wall.duration_since(c.now).unwrap_or_else(|e| e.duration());
            drift >= Duration::from_secs(5)
        });
        if stale {
            self.job_ctx = None;
        }
    }

    /// The settings at the engine's clock, with the latest process list.
    fn ctx(&mut self) -> Arc<Ctx> {
        if self.job_ctx.is_none() {
            self.job_ctx = Some(Arc::new(self.base.at(self.wall, self.procs.clone())));
        }
        self.job_ctx.clone().unwrap()
    }

    fn add(&mut self, item: Item) {
        let id = item.id;
        self.next = self.next.max(id + 1);
        if backed(&item) {
            self.paths.entry(rules::watched(&item).to_path_buf()).or_default().push(id);
        }
        self.keys.insert(key(&item), id);
        let owner = rules::owner_running(&self.ctx(), &item);
        let entry = Entry { item, version: 0, owner, pending: None, checked: None, due: None, busy: false };
        self.entries.insert(id, entry);
    }

    /// A new item a rule found: passed on, and looked at.
    fn found(&mut self, (kind, path, label, target): Found) -> usize {
        let item = Item { id: self.next, kind, path, label, target, verdict: None };
        let id = item.id;
        self.emit(Event::Found(item.clone()));
        self.add(item);
        self.touch(id, None, false);
        id
    }

    fn remove(&mut self, id: usize) {
        let Some(e) = self.entries.remove(&id) else { return };
        self.keys.remove(&key(&e.item));
        let watched = rules::watched(&e.item);
        if let Some(ids) = self.paths.get_mut(watched) {
            ids.retain(|x| *x != id);
            if ids.is_empty() {
                self.paths.remove(watched);
            }
        }
        self.emit(Event::Gone(id));
    }

    /// Takes a new verdict, passing it on only if it says something new.
    fn set(&mut self, id: usize, v: Verdict) {
        let ctx = self.ctx();
        let Some(e) = self.entries.get_mut(&id) else { return };
        e.due = rules::due(&ctx, &e.item, &v);
        if e.item.verdict.as_ref().is_some_and(|old| old.same(&v)) {
            return;
        }
        e.item.verdict = Some(v.clone());
        e.version += 1;
        self.emit(Event::Measured(id, v));
    }

    /// The first scan's events: kept track of and passed on.
    fn mirror(&mut self, ev: Event) {
        match &ev {
            Event::Found(i) => self.add(i.clone()),
            Event::Measured(id, v) => {
                let ctx = self.ctx();
                if let Some(e) = self.entries.get_mut(id) {
                    e.due = rules::due(&ctx, &e.item, v);
                    e.item.verdict = Some(v.clone());
                    e.version += 1;
                }
            }
            _ => {}
        }
        self.emit(ev);
    }

    /// Notes that `path` in the item changed, or anything in it, or (with no
    /// path) that it needs a look for another reason.
    fn touch(&mut self, id: usize, path: Option<&Path>, anything: bool) {
        let now = self.mono;
        let Some(e) = self.entries.get_mut(&id) else { return };
        let p = e.pending.get_or_insert(Pending {
            paths: Vec::new(),
            anything: false,
            again: false,
            first: now,
            last: now,
        });
        p.last = now;
        match path {
            None => p.again = true,
            Some(_) if anything || p.anything => {
                p.anything = true;
                p.paths.clear();
            }
            Some(changed) => {
                // The latest few are enough: one fresh write among them settles it.
                p.paths.retain(|x| x != changed);
                if p.paths.len() >= PATHS {
                    p.paths.remove(0);
                }
                p.paths.push(changed.to_path_buf());
            }
        }
    }

    fn changes(&mut self, batch: Vec<Change>) {
        let now = self.mono;
        for c in batch {
            if c.root {
                self.roots_soon = true;
            }
            let mut hit: Vec<usize> = Vec::new();
            for a in c.path.ancestors() {
                if let Some(ids) = self.paths.get(a) {
                    hit.extend(ids);
                }
            }
            if c.subtree {
                for (p, ids) in &self.paths {
                    if p.starts_with(&c.path) {
                        hit.extend(ids);
                    }
                }
            }
            hit.sort_unstable();
            hit.dedup();
            for id in hit {
                self.touch(id, Some(&c.path), c.subtree);
            }
            if c.entries && !c.subtree && !self.paths.contains_key(&c.path) {
                let new = RULES.iter().filter_map(|r| r.entry).find_map(|entry| entry(&self.base, &c.path));
                if let Some(f) = new.filter(|_| std::fs::symlink_metadata(&c.path).is_ok()) {
                    self.found(f);
                }
            }
            for (rule, st) in RULES.iter().zip(&mut self.rules) {
                let under = c.subtree
                    && (rule.roots)(&self.base).iter().any(|r| c.path.starts_with(r) || r.starts_with(&c.path));
                if under || (rule.lists)(&self.base, &c) {
                    let first = st.pending.map_or(now, |(f, _)| f);
                    st.pending = Some((first, now));
                }
            }
        }
    }

    fn control(&mut self, c: Control) {
        match c {
            Control::Pause => self.paused = true,
            Control::Resume => self.paused = false,
            Control::Recheck(ids) => self.asked.extend(ids.into_iter().map(|id| (id, Why::Again))),
            Control::Measure(id) => self.asked.push((id, Why::Asked)),
            Control::Rescan => self.want_rescan = true,
        }
    }

    fn apply(&mut self, msg: Msg) {
        match msg {
            Msg::Control(c) => self.control(c),
            Msg::Changes(batch) => self.changes(batch),
            Msg::Checked { id, version, asked, verdict } => {
                if !asked {
                    self.jobs = self.jobs.saturating_sub(1);
                }
                let now = self.mono;
                let Some(e) = self.entries.get_mut(&id) else { return };
                if !asked {
                    e.busy = false;
                    e.checked = Some(now);
                    if e.version != version {
                        return;
                    }
                }
                match verdict {
                    None => self.remove(id),
                    Some(v) => self.set(id, v),
                }
            }
            Msg::Listed { rule, found, notes } => self.listed(rule, found, notes),
            Msg::Procs(p) => self.new_procs(p),
            Msg::Scan(ev) => self.rescanned(ev),
        }
    }

    fn listed(&mut self, rule: usize, found: Vec<Found>, notes: Vec<(Kind, String)>) {
        let st = &mut self.rules[rule];
        st.busy = false;
        st.listed = self.mono;
        let kinds = RULES[rule].kinds;
        for k in kinds {
            self.emit(Event::ClearNotes(*k));
        }
        let noted: HashSet<Kind> = notes.iter().map(|(k, _)| *k).collect();
        for (k, t) in notes {
            self.emit(Event::Note(k, t));
        }
        let mut seen = HashSet::new();
        for (kind, path, label, target) in found {
            let Some(&id) = self.keys.get(&(kind, path.clone(), label.clone())) else {
                seen.insert(self.found((kind, path, label, target)));
                continue;
            };
            seen.insert(id);
            let e = self.entries.get_mut(&id).unwrap();
            if e.item.target != target {
                e.item.target = target;
                let item = e.item.clone();
                self.emit(Event::Found(item));
                self.touch(id, None, false);
            }
        }
        let missing: Vec<usize> = self
            .entries
            .iter()
            .filter(|(id, e)| kinds.contains(&e.item.kind) && !seen.contains(id))
            .map(|(id, _)| *id)
            .collect();
        self.missing(missing, &noted);
    }

    /// Items a listing didn't find again: gone when their folder is. A tool's
    /// list (snapshots, runtimes) is believed unless it reported a problem.
    fn missing(&mut self, ids: Vec<usize>, noted: &HashSet<Kind>) {
        for id in ids {
            let Some(e) = self.entries.get(&id) else { continue };
            let gone = if backed(&e.item) {
                std::fs::symlink_metadata(&e.item.path).is_err()
            } else {
                !noted.contains(&e.item.kind)
            };
            if gone {
                self.remove(id);
            } else {
                self.touch(id, None, false);
            }
        }
    }

    /// A new process list. Only the processes that started, stopped or changed
    /// are matched against the items, by the paths their command lines and
    /// working folders name; and the items kept while an app runs are asked
    /// whether it still does.
    fn new_procs(&mut self, p: Procs) {
        let old = std::mem::replace(&mut self.procs, p);
        self.procs_busy = false;
        self.procs_at = self.mono;
        self.job_ctx = None;
        let mut hit = Vec::new();
        let moved = moved(&old, &self.procs);
        if !moved.is_empty() {
            let spaced: Vec<(usize, String)> = self
                .entries
                .iter()
                .filter(|(_, e)| backed(&e.item) && e.item.path.as_os_str().to_string_lossy().contains(' '))
                .map(|(id, e)| (*id, e.item.path.to_string_lossy().into_owned()))
                .collect();
            for proc in &moved {
                for named in named_paths(proc) {
                    hit.extend(Path::new(&named).ancestors().filter_map(|a| self.paths.get(a)).flatten());
                }
                // A path with a space in it doesn't survive being split into words.
                hit.extend(spaced.iter().filter(|(_, p)| crate::procs::names_path(&proc.cmd, p)).map(|(id, _)| *id));
            }
        }
        let ctx = self.ctx();
        for (id, e) in &mut self.entries {
            if e.owner.is_some() {
                let now = rules::owner_running(&ctx, &e.item);
                if now != e.owner {
                    e.owner = now;
                    hit.push(*id);
                }
            }
        }
        hit.sort_unstable();
        hit.dedup();
        for id in hit {
            self.touch(id, None, false);
        }
    }

    fn start_rescan(&mut self) -> Keyed {
        self.rescanning = Some((HashSet::new(), HashSet::new()));
        for k in Kind::ALL {
            self.emit(Event::ClearNotes(k));
        }
        Keyed { known: self.keys.clone(), next: AtomicUsize::new(self.next) }
    }

    fn rescanned(&mut self, ev: Event) {
        match ev {
            Event::Found(item) => {
                if let Some((seen, _)) = self.rescanning.as_mut() {
                    seen.insert(item.id);
                }
                match self.entries.get_mut(&item.id) {
                    Some(e) if e.item.target != item.target => {
                        e.item.target = item.target;
                        let item = e.item.clone();
                        self.emit(Event::Found(item));
                    }
                    Some(_) => {}
                    None => {
                        self.emit(Event::Found(item.clone()));
                        self.add(item);
                    }
                }
            }
            Event::Measured(id, v) => self.set(id, v),
            Event::Note(k, t) => {
                if let Some((_, noted)) = self.rescanning.as_mut() {
                    noted.insert(k);
                }
                self.emit(Event::Note(k, t));
            }
            Event::Done => {
                let (seen, noted) = self.rescanning.take().unwrap_or_default();
                let missing = self.entries.keys().filter(|id| !seen.contains(id)).copied().collect();
                self.missing(missing, &noted);
                self.emit(Event::Done);
            }
            _ => {}
        }
    }

    /// What to do now: what was asked for, then whatever has settled or come due.
    fn jobs(&mut self) -> Vec<Job> {
        let mut out = Vec::new();
        for (id, why) in std::mem::take(&mut self.asked) {
            if let Some(e) = self.entries.get(&id) {
                out.push(Job::Check { id, version: e.version, item: Box::new(e.item.clone()), why, asked: true });
            }
        }
        if self.paused || self.rescanning.is_some() {
            return out;
        }
        let (now, wall) = (self.mono, self.wall);
        if !self.procs_busy && now.duration_since(self.procs_at) >= self.procs_every {
            self.procs_busy = true;
            out.push(Job::Procs);
        }
        for (r, (rule, st)) in RULES.iter().zip(&mut self.rules).enumerate() {
            if st.busy || now.duration_since(st.listed) < rule.gap {
                continue;
            }
            let changed = st.pending.is_some_and(|(first, last)| {
                now.duration_since(last) >= QUIET || now.duration_since(first) >= MAX_WAIT
            });
            if changed || now.duration_since(st.listed) >= self.every[r] {
                st.busy = true;
                st.pending = None;
                out.push(Job::List(r));
            }
        }
        let mut ready: Vec<usize> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                !e.busy
                    && (e.pending.as_ref().is_some_and(|p| p.settled(now)) || e.due.is_some_and(|d| d <= wall))
                    && e.checked.is_none_or(|c| now.duration_since(c) >= ITEM_GAP)
            })
            .map(|(id, _)| *id)
            .collect();
        ready.sort_unstable();
        for id in ready {
            if self.jobs >= JOBS {
                break;
            }
            let e = self.entries.get_mut(&id).unwrap();
            let why = e.pending.take().map_or(Why::Again, Pending::why);
            e.due = None;
            e.busy = true;
            self.jobs += 1;
            out.push(Job::Check { id, version: e.version, item: Box::new(e.item.clone()), why, asked: false });
        }
        out
    }
}

/// The processes in one list and not the other: started, stopped, or running
/// something else.
fn moved(old: &Procs, new: &Procs) -> Vec<Proc> {
    let key = |p: &Proc| (p.pid, p.cmd.clone(), p.cwd.clone());
    let before: HashSet<_> = old.list.iter().map(key).collect();
    let after: HashSet<_> = new.list.iter().map(key).collect();
    let gone = old.list.iter().filter(|p| !after.contains(&key(p)));
    let came = new.list.iter().filter(|p| !before.contains(&key(p)));
    gone.chain(came).cloned().collect()
}

/// The absolute paths a process's command line and working folder name, each
/// as written and with or without the /private macOS hides behind /var and
/// /tmp.
fn named_paths(p: &Proc) -> Vec<String> {
    let words = p.cmd.split_whitespace().flat_map(|w| w.split('='));
    let cwd = p.cwd.iter().map(|c| c.to_string_lossy().into_owned());
    let mut out = Vec::new();
    for w in words.map(|w| w.trim_matches(|c| matches!(c, '\'' | '"' | ',' | ';')).to_string()).chain(cwd) {
        if !w.starts_with('/') {
            continue;
        }
        if let Some(rest) = w.strip_prefix("/private").filter(|r| r.starts_with("/var/") || r.starts_with("/tmp/")) {
            out.push(rest.to_string());
        } else if w.starts_with("/var/") || w.starts_with("/tmp/") {
            out.push(format!("/private{w}"));
        }
        out.push(w);
    }
    out
}

/// The folders to watch: every rule's that exist.
fn roots(base: &Ctx) -> Vec<PathBuf> {
    watch::existing(RULES.iter().flat_map(|r| (r.roots)(base)).collect())
}

fn run(base: Arc<Ctx>, pool: Arc<ThreadPool>, ui: Sender<Event>, tx: Sender<Msg>, rx: Receiver<Msg>) {
    scan::background();
    let refresh = scan::background_pool("sweep-live", JOBS);
    let watch = |roots: &[PathBuf]| {
        let tx = tx.clone();
        Watcher::start(roots, LATENCY, move |c| {
            let _ = tx.send(Msg::Changes(c));
        })
        .map_err(|e| format!("{e:#}"))
    };
    let mut watched = roots(&base);
    let mut watcher = watch(&watched);
    let _ = ui.send(Event::Watching(watcher.as_ref().map(|_| ()).map_err(Clone::clone)));

    // The first scan's events go out as they come; changes meanwhile wait in `rx`.
    let mut engine = Engine::new(base.clone(), ui);
    let (stx, srx) = channel();
    let (ctx, p) = (base.clone(), pool.clone());
    let first = std::thread::spawn(move || scan::run(&ctx, &p, &stx, false));
    for ev in srx {
        engine.mirror(ev);
    }
    let _ = first.join();
    let start = Instant::now();
    for st in &mut engine.rules {
        st.listed = start;
    }
    engine.procs_at = start;
    let mut roots_at = start;
    let mut dispatched = start;

    while !engine.closed {
        match rx.recv_timeout(TICK) {
            Ok(m) => engine.apply(m),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(m) = rx.try_recv() {
            engine.apply(m);
        }
        engine.clock(Instant::now(), SystemTime::now());
        if engine.roots_soon || roots_at.elapsed() >= ROOTS_EVERY {
            engine.roots_soon = false;
            roots_at = Instant::now();
            let now = roots(&base);
            if now != watched {
                watched = now;
                drop(watcher);
                watcher = watch(&watched);
                engine.emit(Event::Watching(watcher.as_ref().map(|_| ()).map_err(Clone::clone)));
                for st in &mut engine.rules {
                    st.pending = Some((engine.mono, engine.mono));
                }
            }
        }
        if engine.want_rescan && engine.rescanning.is_none() {
            engine.want_rescan = false;
            let ids = engine.start_rescan();
            let (tx, pool, base) = (tx.clone(), pool.clone(), base.clone());
            std::thread::spawn(move || {
                let (stx, srx) = channel();
                let scan = std::thread::spawn(move || {
                    let ctx = base.at(SystemTime::now(), Procs::snapshot());
                    scan::run_with(&ctx, &pool, &stx, false, &ids);
                });
                for ev in srx {
                    let _ = tx.send(Msg::Scan(ev));
                }
                let _ = scan.join();
            });
        }
        // A look at what's due a couple of times a second; what was asked for at once.
        if engine.asked.is_empty() && dispatched.elapsed() < TICK {
            continue;
        }
        dispatched = Instant::now();
        let jobs = engine.jobs();
        if jobs.is_empty() {
            continue;
        }
        let ctx = engine.ctx();
        for job in jobs {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            let on = if job.asked() { &pool } else { &refresh };
            on.spawn(move || {
                let _ = tx.send(work(job, &ctx));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Badge;
    use crate::procs::Proc;
    use crate::testutil::{set_mtime, write};

    struct Bench {
        engine: Engine,
        events: Receiver<Event>,
        home: PathBuf,
        _dir: tempfile::TempDir,
    }

    /// An engine over a scan of a temporary home whose clock stands ten days
    /// ahead, so everything written now is old.
    fn bench(setup: impl FnOnce(&Path), procs: impl FnOnce(&Path) -> Procs) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        setup(&home);
        let mut ctx = crate::ctx::test_ctx(&home, SystemTime::now() + Duration::from_secs(10 * 86_400));
        ctx.procs = procs(&home);
        let ctx = Arc::new(ctx);
        let (ui, events) = channel();
        let mut engine = Engine::new(ctx.clone(), ui);
        let (tx, rx) = channel();
        scan::run(&ctx, &scan::named_pool("test-scan", 4), &tx, false);
        drop(tx);
        for ev in rx {
            engine.mirror(ev);
        }
        // Only what a test prompts happens: no tools' lists or process lists on a timer.
        engine.every = vec![Duration::MAX; RULES.len()];
        engine.procs_every = Duration::MAX;
        let b = Bench { engine, events, home, _dir: dir };
        b.events.try_iter().count();
        b
    }

    impl Bench {
        fn id(&self, label: &str) -> usize {
            *self.engine.entries.iter().find(|(_, e)| e.item.label == label).unwrap().0
        }

        fn verdict(&self, label: &str) -> Verdict {
            self.engine.entries[&self.id(label)].item.verdict.clone().unwrap()
        }

        fn later(&mut self, d: Duration) {
            let (m, w) = (self.engine.mono + d, self.engine.wall + d);
            self.engine.clock(m, w);
        }

        /// Runs what's due, as the pools would, until nothing more is.
        fn settle(&mut self) -> Vec<Event> {
            for _ in 0..20 {
                let jobs = self.engine.jobs();
                if jobs.is_empty() {
                    break;
                }
                let ctx = self.engine.ctx();
                for job in jobs {
                    let m = work(job, &ctx);
                    self.engine.apply(m);
                }
                self.later(ITEM_GAP);
            }
            self.events.try_iter().collect()
        }

        /// FSEvents saying `path` changed, and whether it was made or removed.
        fn changed(&mut self, path: &Path, entries: bool) {
            self.engine.apply(Msg::Changes(vec![Change { path: path.to_path_buf(), entries, ..Change::default() }]));
        }
    }

    fn measured(events: &[Event], id: usize) -> Option<&Verdict> {
        events.iter().rev().find_map(|e| match e {
            Event::Measured(i, v) if *i == id => Some(v),
            _ => None,
        })
    }

    fn none(_: &Path) -> Procs {
        Procs::default()
    }

    fn found(events: &[Event], label: &str) -> Option<usize> {
        events.iter().find_map(|e| match e {
            Event::Found(i) if i.label == label => Some(i.id),
            _ => None,
        })
    }

    #[test]
    fn a_fresh_write_inside_an_idle_item_makes_it_in_use_from_one_lookup() {
        let mut b = bench(|h| write(&h.join("T/tmp-idle/deep/old.txt"), "x"), none);
        let id = b.id("tmp-idle");
        assert_eq!(b.verdict("tmp-idle").badge, Badge::Safe);
        let new = b.home.join("T/tmp-idle/deep/new.txt");
        write(&new, "y");
        set_mtime(&new, b.engine.wall - Duration::from_secs(120));
        b.changed(&new, true);
        assert!(b.settle().is_empty(), "nothing until the changes settle");
        b.later(QUIET);
        let events = b.settle();
        let v = measured(&events, id).expect("a new verdict");
        assert_eq!((v.badge, v.reason.as_str()), (Badge::Kept, "in use: written 2 min ago"));
        assert_eq!(v.checks[0].text, "written 2 min ago: deep/new.txt");
        assert!(v.unmeasured, "kept, so not measured");
    }

    fn gone(events: &[Event], id: usize) -> bool {
        events.iter().any(|e| matches!(e, Event::Gone(i) if *i == id))
    }

    #[test]
    fn items_come_and_go_with_their_folder() {
        let mut b = bench(|h| write(&h.join("T/tmp-a/f"), "x"), none);
        let a = b.id("tmp-a");
        let t = b.home.join("T");
        write(&t.join("tmp-b/f"), "y");
        write(&t.join("mystery/f"), "z");
        b.changed(&t.join("tmp-b"), true);
        b.changed(&t.join("mystery"), true);
        // A new entry is an item at once, without listing $TMPDIR again.
        let events = b.settle();
        let new = found(&events, "tmp-b").expect("tmp-b found");
        assert_ne!(new, a);
        assert_eq!(measured(&events, new).map(|v| v.badge), Some(Badge::Safe));
        assert_eq!(found(&events, "mystery"), None, "a name sweep doesn't recognise stays alone");

        std::fs::remove_dir_all(t.join("tmp-a")).unwrap();
        b.changed(&t.join("tmp-a/f"), true);
        b.later(QUIET);
        assert!(gone(&b.settle(), a), "tmp-a gone");
        assert!(!b.engine.keys.values().any(|i| *i == a));

        // A change missed altogether turns up when the rule looks again anyway.
        std::fs::remove_dir_all(t.join("tmp-b")).unwrap();
        b.engine.every[1] = RULES[1].gap;
        b.later(RULES[1].gap);
        assert!(gone(&b.settle(), new), "tmp-b gone");
    }

    #[test]
    fn time_alone_turns_an_item_in_use_idle() {
        let mut b = bench(|h| write(&h.join("T/tmp-recent/f"), "x"), none);
        let id = b.id("tmp-recent");
        set_mtime(&b.home.join("T/tmp-recent/f"), b.engine.wall - Duration::from_secs(86_400));
        b.engine.control(Control::Recheck(vec![id]));
        b.settle();
        assert_eq!(b.verdict("tmp-recent").reason, "in use: written 24 h ago");
        b.later(Duration::from_secs(12 * 3600));
        assert!(measured(&b.settle(), id).is_none(), "still in use half a day later");
        b.later(Duration::from_secs(12 * 3600 + 120));
        let v = measured(&b.settle(), id).cloned().expect("a new verdict once the write is two days old");
        assert_eq!(v.badge, Badge::Safe, "{v:?}");
    }

    #[test]
    fn a_process_that_ends_frees_what_it_was_using() {
        let user = |home: &Path| {
            let cmd = format!("python3 work.py {}", home.join("T/tmp-used").display());
            Procs::from_list(vec![Proc { pid: 7, cmd, exe: None, cwd: None }])
        };
        let mut b = bench(|h| write(&h.join("T/tmp-used/f"), "x"), user);
        let id = b.id("tmp-used");
        assert_eq!(b.verdict("tmp-used").reason, "in use by python3 (pid 7)");
        b.engine.apply(Msg::Procs(Procs::default()));
        let v = measured(&b.settle(), id).cloned().expect("a new verdict");
        assert_eq!(v.badge, Badge::Safe, "{v:?}");
    }

    #[test]
    fn kept_items_arent_measured_when_they_change_and_nothing_runs_while_paused() {
        let installing =
            |_: &Path| Procs::from_list(vec![Proc { pid: 5, cmd: "npm install".into(), exe: None, cwd: None }]);
        let mut b = bench(|h| write(&h.join(".npm/_cacache/x"), "cache"), installing);
        let npm = b.id("npm cache · ~/.npm");
        assert_eq!(b.verdict("npm cache · ~/.npm").badge, Badge::Kept);
        b.engine.control(Control::Pause);
        b.changed(&b.home.join(".npm/_cacache/x"), false);
        b.later(QUIET);
        assert!(b.engine.jobs().is_empty(), "paused");
        b.engine.control(Control::Resume);
        let events = b.settle();
        assert!(
            measured(&events, npm).is_none(),
            "the same verdict isn't sent again: {:?}",
            b.verdict("npm cache · ~/.npm")
        );
        b.engine.control(Control::Measure(npm));
        let events = b.settle();
        assert!(measured(&events, npm).and_then(|v| v.bytes).is_some_and(|n| n > 0), "measured when asked");
    }
}

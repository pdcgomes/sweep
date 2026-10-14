//! One screen: a tree of what was found. Kinds at the top, then the
//! repository, pattern or app they come from, then the items. Kept items fold
//! into one row per group. Marking a row marks everything deletable under it.

use crate::config::Thresholds;
use crate::ctx::Ctx;
use crate::delete::{self, Job, Outcome, Progress, Summary};
use crate::disks::{self, Disk};
use crate::fmt;
use crate::live;
use crate::model::{Badge, Item, Kind, Mark, Target};
use crate::report::{self, Totals, grouped, subgroups, totals};
use crate::rules::DeleteOpts;
use crate::scan::{self, Event as ScanEvent};
use crate::theme::Theme;
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, LineGauge, Padding, Paragraph, Wrap};
use rayon::ThreadPool;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Node {
    Kind(Kind),
    Group(Kind, String),
    /// The folded Kept items of a kind, or of one of its groups.
    Kept(Kind, Option<String>),
    Item(usize),
}

#[derive(Clone, Debug, PartialEq)]
struct Row {
    node: Node,
    depth: usize,
}

/// What's under a row that holds others.
#[derive(Clone, Default)]
struct Info {
    totals: Totals,
    count: usize,
    /// Safe and Review items, the ones marking the row marks.
    selectable: Vec<usize>,
    kept_why: Vec<(String, usize)>,
}

impl Info {
    fn of(items: &[&Item]) -> Info {
        Info {
            totals: totals(items.iter().copied()),
            count: items.len(),
            selectable: items.iter().filter(|i| i.selectable()).map(|i| i.id).collect(),
            kept_why: report::kept_gists(items.iter().copied()),
        }
    }
}

#[derive(PartialEq)]
enum Mode {
    Browse,
    Confirm,
    Deleting,
}

struct Deletion {
    rx: Receiver<Progress>,
    done_rx: Receiver<Summary>,
    cancel: Arc<AtomicBool>,
    total: usize,
    total_bytes: u64,
    finished: usize,
    freed: u64,
    running: BTreeSet<usize>,
    recent: Vec<(usize, Outcome)>,
    summary: Option<Summary>,
    started: Instant,
}

pub struct App {
    items: Vec<Option<Item>>,
    notes: Vec<(Kind, String)>,
    thresholds: Thresholds,
    selected: BTreeSet<usize>,
    /// Dirty worktrees: save the uncommitted work first? On unless turned off.
    backup: HashMap<usize, bool>,
    expanded: HashSet<Node>,
    /// Kinds whose rule is still looking for items.
    looking: HashSet<Kind>,
    /// When each group first appeared, which orders them during the scan.
    found_order: HashMap<(Kind, String), usize>,
    rows: Vec<Row>,
    info: HashMap<Node, Info>,
    parent: HashMap<Node, Node>,
    cursor: Option<Node>,
    /// Whether a key has been pressed yet.
    touched: bool,
    offset: usize,
    filter: String,
    editing: bool,
    scanning: bool,
    stale: bool,
    mode: Mode,
    dry_run: bool,
    flash: Option<(String, Instant)>,
    deletion: Option<Deletion>,
    quit: bool,
    no_color: bool,
    /// The physical disks, once surveyed; or why they couldn't be.
    disks: Option<Result<Vec<Disk>, String>>,
    locator: disks::Locator,
    theme: Theme,
    /// What keeps the list up to date, once the scan is done.
    live: Option<live::Handle>,
    /// Whether changes are being watched for, or why not.
    watching: Option<Result<(), String>>,
    rescanning: bool,
    /// Kept items whose size has been asked for.
    asked: BTreeSet<usize>,
    /// Made-up items: deleting only pretends, and frees made-up space.
    demo: bool,
}

/// Where the totals of a group row start, so they line up at any depth.
const SUMMARY_COLUMN: usize = 46;

impl App {
    pub fn new(dry_run: bool, thresholds: Thresholds) -> App {
        App {
            items: Vec::new(),
            notes: Vec::new(),
            thresholds,
            selected: BTreeSet::new(),
            backup: HashMap::new(),
            expanded: HashSet::new(),
            looking: Kind::ALL.into_iter().collect(),
            found_order: HashMap::new(),
            rows: Vec::new(),
            info: HashMap::new(),
            parent: HashMap::new(),
            cursor: None,
            touched: false,
            offset: 0,
            filter: String::new(),
            editing: false,
            scanning: true,
            stale: true,
            mode: Mode::Browse,
            dry_run,
            flash: None,
            deletion: None,
            quit: false,
            no_color: std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()),
            disks: None,
            locator: disks::Locator::default(),
            theme: Theme::tokyo_night(),
            live: None,
            watching: None,
            rescanning: false,
            asked: BTreeSet::new(),
            demo: false,
        }
    }

    /// Made-up items on made-up disks, placed on them by path prefix.
    pub fn demo(
        items: Vec<Item>,
        notes: Vec<(Kind, String)>,
        disks: Vec<Disk>,
        pins: Vec<(PathBuf, usize)>,
        thresholds: Thresholds,
    ) -> App {
        let mut app = App::with_items(items, notes, false, thresholds);
        app.disks = Some(Ok(disks));
        app.locator = disks::Locator::pinned(pins);
        app.watching = Some(Ok(()));
        app.demo = true;
        app
    }

    fn tell(&self, c: live::Control) {
        if let Some(l) = &self.live {
            l.send(c);
        }
    }

    pub fn set_disks(&mut self, survey: Result<Vec<Disk>, String>) {
        if let Ok(d) = &survey {
            self.locator = disks::Locator::new(d);
        }
        self.disks = Some(survey);
    }

    fn refresh_disks(&mut self) {
        if let Some(Ok(d)) = &mut self.disks {
            disks::refresh(d);
        }
    }

    /// The glance panel: a line per disk between its two borders.
    fn glance_height(&self) -> u16 {
        let lines = match &self.disks {
            Some(Ok(d)) => d.len().max(1),
            _ => 1,
        };
        lines as u16 + 2
    }

    pub fn with_items(items: Vec<Item>, notes: Vec<(Kind, String)>, dry_run: bool, thresholds: Thresholds) -> App {
        let mut app = App::new(dry_run, thresholds);
        for i in items {
            app.put(i);
        }
        app.notes = notes;
        app.scanning = false;
        app.looking.clear();
        app.rebuild();
        app
    }

    fn put(&mut self, i: Item) {
        let id = i.id;
        if self.items.len() <= id {
            self.items.resize_with(id + 1, || None);
        }
        self.items[id] = Some(i);
    }

    fn item(&self, id: usize) -> Option<&Item> {
        self.items.get(id).and_then(Option::as_ref)
    }

    fn on_scan(&mut self, ev: ScanEvent) {
        match ev {
            ScanEvent::Found(i) => self.put(i),
            ScanEvent::Measured(id, v) => {
                if let Some(Some(i)) = self.items.get_mut(id) {
                    if v.unmeasured {
                        self.asked.remove(&id);
                    }
                    let unmarked = v.badge == Badge::Kept && self.selected.remove(&id);
                    let note = unmarked.then(|| format!("unmarked {}: {}", i.label, v.reason));
                    i.verdict = Some(v);
                    if let Some(n) = note {
                        self.say(n);
                    }
                }
            }
            ScanEvent::Gone(id) => {
                if let Some(i) = self.items.get_mut(id).and_then(Option::take)
                    && self.selected.remove(&id)
                {
                    self.say(format!("unmarked {}: it's gone", i.label));
                }
                self.backup.remove(&id);
                self.asked.remove(&id);
            }
            ScanEvent::Note(k, t) => self.notes.push((k, t)),
            ScanEvent::ClearNotes(k) => self.notes.retain(|(kind, _)| *kind != k),
            ScanEvent::Looked(k) => {
                self.looking.remove(&k);
            }
            ScanEvent::Done => {
                self.scanning = false;
                self.rescanning = false;
                self.looking.clear();
            }
            ScanEvent::Watching(w) => self.watching = Some(w),
        }
        self.stale = true;
    }

    /// Asks once for the size of a kept item the cursor rests on.
    fn measure(&mut self, id: usize) {
        if self.asked.insert(id) {
            self.tell(live::Control::Measure(id));
        }
    }

    /// What an empty kind's row says: still looking, why it couldn't, or nothing found.
    fn kind_status(&self, kind: Kind) -> String {
        if self.looking.contains(&kind) {
            return "looking…".into();
        }
        match self.notes.iter().rev().find(|(k, _)| *k == kind) {
            Some((_, note)) => note.clone(),
            None => "nothing found".into(),
        }
    }

    fn is_empty(&self, n: &Node) -> bool {
        matches!(n, Node::Kind(_)) && self.info.get(n).is_none_or(|i| i.count == 0)
    }

    fn visible(&self, i: &Item) -> bool {
        if i.verdict.as_ref().is_some_and(|v| v.hidden) {
            return false;
        }
        if self.filter.is_empty() {
            return true;
        }
        let f = self.filter.to_lowercase();
        let reason = i.verdict.as_ref().map(|v| v.reason.to_lowercase()).unwrap_or_default();
        let badge = i.badge().map(|b| b.label().to_lowercase()).unwrap_or_default();
        i.label.to_lowercase().contains(&f)
            || i.path.to_string_lossy().to_lowercase().contains(&f)
            || i.kind.title().to_lowercase().contains(&f)
            || reason.contains(&f)
            || badge == f
    }

    /// Lays the tree out again. While filtering, everything that matches is
    /// shown with its groups opened. While the scan runs, groups and items keep
    /// the order they were found in, so nothing moves under the cursor; they
    /// sort by size once it's done.
    pub fn rebuild(&mut self) {
        let filtering = !self.filter.is_empty();
        let shown: Vec<&Item> = self.items.iter().flatten().filter(|i| self.visible(i)).collect();
        let expanded = &self.expanded;
        let open = |n: &Node| filtering || expanded.contains(n);
        let mut rows = Vec::new();
        let mut info = HashMap::new();
        let mut parent = HashMap::new();
        let mut by_kind: HashMap<Kind, Vec<&Item>> = grouped(shown).into_iter().collect();
        for kind in Kind::ALL {
            let kind_node = Node::Kind(kind);
            // Every kind keeps its row, found or not, so rows never come and go.
            let Some(list) = by_kind.remove(&kind) else {
                if !filtering {
                    info.insert(kind_node.clone(), Info::default());
                    rows.push(Row { node: kind_node, depth: 0 });
                }
                continue;
            };
            info.insert(kind_node.clone(), Info::of(&list));
            rows.push(Row { node: kind_node.clone(), depth: 0 });
            let kind_open = open(&kind_node);
            let mut groups = subgroups(&list);
            if self.scanning {
                for (g, members) in &mut groups {
                    if let Some(g) = g {
                        let next = self.found_order.len();
                        self.found_order.entry((kind, g.clone())).or_insert(next);
                    }
                    members.sort_by_key(|i| i.id);
                }
                groups.sort_by_key(|(g, _)| g.as_ref().map(|g| self.found_order[&(kind, g.clone())]));
            }
            for (group, members) in groups {
                let (holder, depth) = match &group {
                    Some(name) => {
                        let g = Node::Group(kind, name.clone());
                        info.insert(g.clone(), Info::of(&members));
                        parent.insert(g.clone(), kind_node.clone());
                        if kind_open {
                            rows.push(Row { node: g.clone(), depth: 1 });
                        }
                        (g, 2)
                    }
                    None => (kind_node.clone(), 1),
                };
                let holder_open = kind_open && (holder == kind_node || open(&holder));
                let (live, kept): (Vec<&Item>, Vec<&Item>) =
                    members.iter().partition(|i| i.badge() != Some(Badge::Kept));
                // Kept items fold away only where there's something else to see.
                let fold = !live.is_empty() && !kept.is_empty();
                for i in &live {
                    parent.insert(Node::Item(i.id), holder.clone());
                    if holder_open {
                        rows.push(Row { node: Node::Item(i.id), depth });
                    }
                }
                let (kept_holder, kept_depth, kept_open) = if fold {
                    let k = Node::Kept(kind, group.clone());
                    info.insert(k.clone(), Info::of(&kept));
                    parent.insert(k.clone(), holder.clone());
                    if holder_open {
                        rows.push(Row { node: k.clone(), depth });
                    }
                    let k_open = holder_open && open(&k);
                    (k, depth + 1, k_open)
                } else {
                    (holder.clone(), depth, holder_open)
                };
                for i in &kept {
                    parent.insert(Node::Item(i.id), kept_holder.clone());
                    if kept_open {
                        rows.push(Row { node: Node::Item(i.id), depth: kept_depth });
                    }
                }
            }
        }
        self.rows = rows;
        self.info = info;
        self.parent = parent;
        if !self.touched {
            // Until a key is pressed the cursor stays on top, however the
            // rows fill in above whatever the scan found first.
            self.cursor = self.rows.first().map(|r| r.node.clone());
        }
        // A row that folded away hands the cursor to the nearest row still shown.
        let mut c = self.cursor.clone();
        while let Some(n) = &c {
            if self.rows.iter().any(|r| &r.node == n) {
                break;
            }
            c = self.parent.get(n).cloned();
        }
        self.cursor = c.or_else(|| self.rows.first().map(|r| r.node.clone()));
        self.stale = false;
    }

    fn cursor_row(&self) -> Option<usize> {
        let c = self.cursor.as_ref()?;
        self.rows.iter().position(|r| &r.node == c)
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let at = self.cursor_row().unwrap_or(0) as isize;
        let to = (at.saturating_add(delta)).clamp(0, self.rows.len() as isize - 1) as usize;
        self.cursor = Some(self.rows[to].node.clone());
    }

    fn is_open(&self, n: &Node) -> bool {
        !self.filter.is_empty() || self.expanded.contains(n)
    }

    /// Opens the row, or if it's open already, steps onto its first child.
    /// A kind still being looked through can be opened; what's found shows up under it.
    fn open(&mut self) {
        let Some(n) = self.cursor.clone() else { return };
        if matches!(n, Node::Item(_)) {
            return;
        }
        if !self.is_open(&n) {
            self.expanded.insert(n);
            self.stale = true;
            return;
        }
        if let Some(r) = self.cursor_row()
            && self.rows.get(r + 1).is_some_and(|next| next.depth > self.rows[r].depth)
        {
            self.move_cursor(1);
        }
    }

    /// Closes the row if it's open; otherwise closes the group it's in and
    /// moves onto that.
    fn close(&mut self) {
        let Some(n) = self.cursor.clone() else { return };
        if !matches!(n, Node::Item(_)) && self.expanded.remove(&n) {
            self.stale = true;
            return;
        }
        if let Some(p) = self.parent.get(&n).cloned() {
            if self.expanded.remove(&p) {
                self.stale = true;
            }
            self.cursor = Some(p);
        }
    }

    fn toggle_fold(&mut self) {
        let Some(n) = self.cursor.clone() else { return };
        if matches!(n, Node::Item(_)) {
            return;
        }
        if !self.expanded.remove(&n) {
            self.expanded.insert(n);
        }
        self.stale = true;
    }

    pub fn expand_all(&mut self) {
        loop {
            self.rebuild();
            let more: Vec<Node> = self
                .rows
                .iter()
                .map(|r| r.node.clone())
                .filter(|n| matches!(n, Node::Kind(_) | Node::Group(..)) && !self.expanded.contains(n))
                .collect();
            if more.is_empty() {
                break;
            }
            self.expanded.extend(more);
        }
    }

    fn say(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
    }

    /// Marks the item, or everything deletable under the row; again clears it.
    fn toggle(&mut self) {
        let Some(n) = self.cursor.clone() else { return };
        match n {
            Node::Item(id) => {
                let Some(item) = self.item(id) else { return };
                match item.badge() {
                    None => self.say("still measuring this one"),
                    Some(Badge::Kept) => {
                        let why = item
                            .verdict
                            .as_ref()
                            .map(|v| v.aged_text(&v.reason, SystemTime::now()))
                            .unwrap_or_default();
                        self.say(format!("Kept items can't be marked: {why}"));
                    }
                    Some(_) => {
                        if !self.selected.remove(&id) {
                            self.selected.insert(id);
                        }
                    }
                }
            }
            Node::Kept(..) => self.say("Kept items can't be marked; → lists them and why they stay"),
            _ => {
                let ids = self.info.get(&n).map(|i| i.selectable.clone()).unwrap_or_default();
                if ids.is_empty() {
                    self.say("nothing in here can be deleted");
                } else if ids.iter().all(|id| self.selected.contains(id)) {
                    for id in &ids {
                        self.selected.remove(id);
                    }
                    self.say(format!("unmarked {}", fmt::count(ids.len(), "item", "items")));
                } else {
                    let bytes: u64 = ids.iter().filter_map(|&id| self.item(id).and_then(Item::bytes)).sum();
                    self.selected.extend(ids.iter().copied());
                    self.say(format!("marked {} · {}", fmt::count(ids.len(), "item", "items"), fmt::bytes(bytes)));
                }
            }
        }
    }

    /// Marks every Safe item that matches the filter, or clears them if all are.
    fn select_all_safe(&mut self) {
        let safe: Vec<usize> = self
            .items
            .iter()
            .flatten()
            .filter(|i| self.visible(i) && i.badge() == Some(Badge::Safe))
            .map(|i| i.id)
            .collect();
        if safe.is_empty() {
            self.say("no Safe items here");
            return;
        }
        if safe.iter().all(|id| self.selected.contains(id)) {
            for id in &safe {
                self.selected.remove(id);
            }
            self.say(format!("unmarked {}", fmt::count(safe.len(), "Safe item", "Safe items")));
        } else {
            self.selected.extend(safe.iter().copied());
            self.say(format!("marked {}", fmt::count(safe.len(), "Safe item", "Safe items")));
        }
    }

    /// ● everything deletable under a row is marked, ◐ some of it, ○ none.
    fn check(&self, n: &Node) -> &'static str {
        match n {
            Node::Item(id) => match self.item(*id).and_then(Item::badge) {
                Some(Badge::Kept) => " ",
                None => "·",
                _ if self.selected.contains(id) => MARK_ALL,
                _ => MARK_NONE,
            },
            Node::Kept(..) => " ",
            _ => {
                let ids = self.info.get(n).map(|i| i.selectable.as_slice()).unwrap_or(&[]);
                let marked = ids.iter().filter(|id| self.selected.contains(id)).count();
                match (ids.len(), marked) {
                    (0, _) => " ",
                    (_, 0) => MARK_NONE,
                    (all, m) if m == all => MARK_ALL,
                    _ => MARK_SOME,
                }
            }
        }
    }

    fn toggle_backup(&mut self) {
        let Some(Node::Item(id)) = self.cursor else {
            self.say("b only applies to a worktree with uncommitted work");
            return;
        };
        let dirty = self.item(id).and_then(|i| i.verdict.as_ref()).is_some_and(|v| v.dirty_worktree());
        if !dirty {
            self.say("b only applies to a worktree with uncommitted work");
            return;
        }
        let on = !self.backup_for(id);
        self.backup.insert(id, on);
        self.say(if on {
            "uncommitted work will be saved on a backup branch first"
        } else {
            "uncommitted work will be discarded"
        });
    }

    fn backup_for(&self, id: usize) -> bool {
        *self.backup.get(&id).unwrap_or(&true)
    }

    fn chosen(&self) -> Vec<&Item> {
        self.selected.iter().filter_map(|&id| self.item(id)).filter(|i| i.selectable()).collect()
    }

    fn start(&mut self, ctx: &Arc<Ctx>) {
        let jobs: Vec<Job> = self
            .chosen()
            .into_iter()
            .map(|i| Job {
                item: i.clone(),
                seen: i.badge().unwrap(),
                opts: DeleteOpts { backup: self.backup_for(i.id) },
            })
            .collect();
        let total = jobs.len();
        let total_bytes = jobs.iter().filter_map(|j| j.item.bytes()).sum();
        let (tx, rx) = channel();
        let (done_tx, done_rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        if self.demo {
            let volumes: Vec<(usize, u64, String)> = jobs
                .iter()
                .map(|j| {
                    let disk = self.locator.item(&j.item);
                    let volume = match (&self.disks, disk) {
                        (Some(Ok(d)), Some(n)) => d[n].volumes.first().cloned().unwrap_or_default(),
                        _ => String::new(),
                    };
                    (j.item.id, j.item.bytes().unwrap_or(0), volume)
                })
                .collect();
            std::thread::spawn(move || pretend_to_delete(volumes, &tx, &done_tx));
        } else {
            let (ctx, c, dry) = (ctx.clone(), cancel.clone(), self.dry_run);
            std::thread::spawn(move || {
                let s = delete::run(&ctx, &scan::delete_pool(), jobs, dry, &c, &tx);
                let _ = done_tx.send(s);
            });
        }
        self.deletion = Some(Deletion {
            rx,
            done_rx,
            cancel,
            total,
            total_bytes,
            finished: 0,
            freed: 0,
            running: BTreeSet::new(),
            recent: Vec::new(),
            summary: None,
            started: Instant::now(),
        });
        self.mode = Mode::Deleting;
        self.tell(live::Control::Pause);
    }

    fn poll_deletion(&mut self) {
        let Some(d) = self.deletion.as_mut() else { return };
        let mut gone = Vec::new();
        while let Ok(p) = d.rx.try_recv() {
            match p {
                Progress::Started(id) => {
                    d.running.insert(id);
                }
                Progress::Finished(id, outcome) => {
                    d.running.remove(&id);
                    d.finished += 1;
                    if let Outcome::Deleted { freed, .. } | Outcome::WouldDelete { freed, .. } = &outcome {
                        d.freed += freed;
                    }
                    if matches!(outcome, Outcome::Deleted { .. }) {
                        gone.push(id);
                    }
                    d.recent.push((id, outcome));
                }
            }
        }
        if let Ok(s) = d.done_rx.try_recv() {
            d.summary = Some(s);
        }
        for id in gone {
            self.selected.remove(&id);
            if self.demo
                && let Some(i) = self.item(id).cloned()
                && let Some(n) = self.locator.item(&i)
                && let Some(Ok(d)) = &mut self.disks
                && let Some(store) = d[n].stores.first_mut()
            {
                store.free += i.bytes().unwrap_or(0);
            }
            if let Some(Some(i)) = self.items.get_mut(id)
                && let Some(v) = i.verdict.as_mut()
            {
                v.hidden = true;
            }
            self.stale = true;
        }
    }

    fn on_key(&mut self, k: KeyEvent, ctx: &Arc<Ctx>) {
        self.touched = true;
        let ctrl_c = k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL);
        match self.mode {
            Mode::Deleting => {
                let finished = self.deletion.as_ref().is_some_and(|d| d.summary.is_some());
                if finished {
                    if ctrl_c || k.code == KeyCode::Char('q') {
                        self.quit = true;
                    }
                    if !self.dry_run {
                        self.selected.clear();
                    }
                    if let Some(d) = self.deletion.take() {
                        self.tell(live::Control::Recheck(d.recent.iter().map(|(id, _)| *id).collect()));
                    }
                    self.tell(live::Control::Resume);
                    self.mode = Mode::Browse;
                    self.stale = true;
                } else if ctrl_c || matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                    if let Some(d) = &self.deletion {
                        d.cancel.store(true, Ordering::SeqCst);
                    }
                    self.say("stopping: the items already under way finish first");
                }
            }
            Mode::Confirm => match k.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.start(ctx),
                _ if ctrl_c => self.quit = true,
                _ => self.mode = Mode::Browse,
            },
            Mode::Browse => {
                if ctrl_c {
                    self.quit = true;
                    return;
                }
                if self.editing {
                    match k.code {
                        KeyCode::Esc => {
                            self.filter.clear();
                            self.editing = false;
                        }
                        KeyCode::Enter => self.editing = false,
                        KeyCode::Backspace => {
                            self.filter.pop();
                        }
                        KeyCode::Up => self.move_cursor(-1),
                        KeyCode::Down => self.move_cursor(1),
                        KeyCode::Char(c) => self.filter.push(c),
                        _ => {}
                    }
                    self.stale = true;
                    return;
                }
                match k.code {
                    KeyCode::Char('q') => self.quit = true,
                    KeyCode::Esc if !self.filter.is_empty() => {
                        self.filter.clear();
                        self.stale = true;
                    }
                    KeyCode::Esc => self.quit = true,
                    KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
                    KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
                    KeyCode::Right | KeyCode::Char('l') => self.open(),
                    KeyCode::Left | KeyCode::Char('h') => self.close(),
                    KeyCode::Tab => self.toggle_fold(),
                    KeyCode::PageUp => self.move_cursor(-20),
                    KeyCode::PageDown => self.move_cursor(20),
                    KeyCode::Home | KeyCode::Char('g') => self.move_cursor(isize::MIN / 2),
                    KeyCode::End | KeyCode::Char('G') => self.move_cursor(isize::MAX / 2),
                    KeyCode::Char(' ') => self.toggle(),
                    KeyCode::Char('a') => self.select_all_safe(),
                    KeyCode::Char('b') => self.toggle_backup(),
                    KeyCode::Char('r') if self.live.is_some() && !self.scanning && !self.rescanning => {
                        self.rescanning = true;
                        self.tell(live::Control::Rescan);
                        self.say("looking at everything again; the list stays as it is meanwhile");
                    }
                    KeyCode::Char('/') => self.editing = true,
                    KeyCode::Enter => {
                        if self.chosen().is_empty() {
                            self.say("nothing marked: space marks the row under the cursor, a every Safe item");
                        } else {
                            self.mode = Mode::Confirm;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    pub fn draw(&mut self, f: &mut Frame) {
        if self.stale {
            self.rebuild();
        }
        let t = self.theme;
        f.render_widget(Block::new().style(Style::new().bg(t.bg).fg(t.fg)), f.area());
        // A column of air either side, like the gaps between Omarchy's windows.
        let area = f.area().inner(Margin { horizontal: 1, vertical: 0 });
        let [top, body, detail] =
            Layout::vertical([Constraint::Length(self.glance_height()), Constraint::Min(5), Constraint::Length(8)])
                .areas(area);
        self.draw_top(f, top);
        if self.mode == Mode::Deleting {
            self.draw_deletion(f, body.union(detail));
        } else {
            self.draw_list(f, body);
            self.draw_detail(f, detail);
        }
        if self.mode == Mode::Confirm {
            self.draw_confirm(f);
        }
        // With NO_COLOR set, crossterm turns each colour change into a full
        // attribute reset, which would wipe out the reversed cursor row.
        if self.no_color {
            for cell in f.buffer_mut().content.iter_mut() {
                cell.set_fg(Color::Reset);
                cell.set_bg(Color::Reset);
            }
        }
    }

    fn panel(&self, border: Color) -> Block<'static> {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .style(Style::new().bg(self.theme.bg).fg(self.theme.fg))
    }

    fn badge_style(&self, b: Option<Badge>) -> Style {
        let t = &self.theme;
        match b {
            Some(Badge::Safe) => Style::new().fg(t.green),
            Some(Badge::Review) => Style::new().fg(t.yellow),
            Some(Badge::Kept) | None => Style::new().fg(t.comment),
        }
    }

    fn severity_style(&self, s: disks::Severity) -> Style {
        let t = &self.theme;
        match s {
            disks::Severity::Ok => Style::new().fg(t.green),
            disks::Severity::Low => Style::new().fg(t.orange).add_modifier(Modifier::BOLD),
            disks::Severity::Critical => Style::new().fg(t.red).add_modifier(Modifier::BOLD),
        }
    }

    /// Safe, Review and Kept totals as coloured spans, leaving out what's empty.
    fn summary_spans(&self, totals: &Totals) -> Vec<Span<'static>> {
        let t = &self.theme;
        let mut spans = Vec::new();
        let mut part = |text: String, style: Style| {
            if !spans.is_empty() {
                spans.push(Span::raw("   "));
            }
            spans.push(Span::styled(text, style));
        };
        if totals.safe > 0 {
            part(
                format!("Safe {} ({})", fmt::bytes(totals.safe_bytes), fmt::num(totals.safe)),
                Style::new().fg(t.green),
            );
        }
        if totals.review > 0 {
            let text = format!("Review {} ({})", fmt::bytes(totals.review_bytes), fmt::num(totals.review));
            part(text, Style::new().fg(t.yellow));
        }
        if totals.kept > 0 {
            part(format!("Kept {}", totals.kept_label()), Style::new().fg(t.comment));
        }
        if totals.measuring > 0 {
            part(format!("measuring {}…", fmt::num(totals.measuring)), Style::new().fg(t.comment));
        }
        spans
    }

    /// The glance: one bar per physical disk, framed by what was found and
    /// what's marked. The frame takes the colour of the fullest disk.
    fn draw_top(&mut self, f: &mut Frame, area: Rect) {
        let th = self.theme;
        let t = totals(self.items.iter().flatten());
        let chosen = self.chosen();
        let (marked_n, marked_bytes) = (chosen.len(), chosen.iter().filter_map(|i| i.bytes()).sum::<u64>());
        let found = self.items.iter().flatten().filter(|i| !i.verdict.as_ref().is_some_and(|v| v.hidden)).count();

        let mut title = vec![
            Span::raw(" "),
            Span::styled(" sweep ", Style::new().bg(th.blue).fg(th.bg).add_modifier(Modifier::BOLD)),
        ];
        if self.dry_run {
            title.push(Span::raw(" "));
            title.push(Span::styled(" dry run ", Style::new().bg(th.yellow).fg(th.bg).add_modifier(Modifier::BOLD)));
        }
        title.push(Span::styled(format!(" {} found ", fmt::count(found, "item", "items")), Style::new().fg(th.text)));
        if t.measuring > 0 {
            title.push(Span::styled(format!("· measuring {}… ", fmt::num(t.measuring)), Style::new().fg(th.comment)));
        } else if self.scanning {
            title.push(Span::styled("· looking… ", Style::new().fg(th.comment)));
        } else if self.rescanning {
            title.push(Span::styled("· looking again… ", Style::new().fg(th.comment)));
        } else {
            match &self.watching {
                Some(Ok(())) => title.push(Span::styled("· watching for changes ", Style::new().fg(th.comment))),
                Some(Err(e)) => {
                    title.push(Span::styled(format!("· not watching for changes: {e} "), Style::new().fg(th.yellow)))
                }
                None => {}
            }
        }
        let bottom = Line::from(vec![
            Span::styled(format!(" Safe {} ", fmt::bytes(t.safe_bytes)), Style::new().fg(th.green)),
            Span::styled(format!(" Review {} ", fmt::bytes(t.review_bytes)), Style::new().fg(th.yellow)),
            Span::styled(format!(" Kept {} ", t.kept_label()), Style::new().fg(th.comment)),
            Span::styled(
                format!("  Marked {} · {} ", fmt::num(marked_n), fmt::bytes(marked_bytes)),
                Style::new().fg(th.fg).add_modifier(Modifier::BOLD),
            ),
        ]);

        let inner_w = area.width.saturating_sub(2) as usize;
        let quiet = Style::new().fg(th.comment);
        let (lines, worst) = match &self.disks {
            None => (vec![Line::styled(" looking at the disks…", quiet)], disks::Severity::Ok),
            Some(Err(e)) => (vec![Line::styled(format!(" couldn't read the disks: {e}"), quiet)], disks::Severity::Ok),
            Some(Ok(list)) => {
                let list = list.clone();
                let selected = &self.selected;
                let reclaim =
                    self.locator.reclaim(list.len(), self.items.iter().flatten(), |id| selected.contains(&id));
                let worst = list.iter().map(Disk::severity).max().unwrap_or(disks::Severity::Ok);
                // One width for every bar, so the disks compare at a glance.
                let words = list.iter().zip(&reclaim).map(|(d, r)| disk_words(d, r)).max().unwrap_or(0);
                let bar_w = inner_w.saturating_sub(words).clamp(16, 48);
                let lines = list.iter().zip(&reclaim).map(|(d, r)| self.disk_line(d, r, bar_w, inner_w)).collect();
                (lines, worst)
            }
        };
        let border = match worst {
            disks::Severity::Ok => th.gutter,
            disks::Severity::Low => th.orange,
            disks::Severity::Critical => th.red,
        };
        let block = self.panel(border).title(Line::from(title)).title_bottom(bottom);
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// "● Internal · Macintosh HD   ━━━━━━━━━━━━━━──────  63% used · 365 GB free of 1.0 TB  OK   sweep frees …"
    fn disk_line(&self, d: &Disk, r: &disks::Reclaim, bar_w: usize, width: usize) -> Line<'static> {
        let t = &self.theme;
        let sev = d.severity();
        let name_w = DISK_NAME_W;
        let cells = disks::bar_cells(d, r, bar_w);
        // Thin lines like btop's meters; without colour, glyphs that differ by shape.
        let glyphs = if self.no_color { disks::BAR_GLYPHS } else { ["━", "━", "━", "─"] };
        let styles = [t.blue, t.green, t.yellow, t.gutter].map(|c| Style::new().fg(c));
        let mut spans = vec![
            Span::styled(" ● ", self.severity_style(sev)),
            Span::styled(
                format!("{:<name_w$} ", fit_end(&d.title(), name_w)),
                Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
            ),
        ];
        for ((n, glyph), style) in cells.iter().zip(glyphs).zip(styles) {
            spans.push(Span::styled(glyph.repeat(*n), style));
        }
        spans.push(Span::styled(format!("  {}  ", disks::usage(d)), Style::new().fg(t.text)));
        spans.push(Span::styled(format!("{:<8}", sev.label()), self.severity_style(sev)));
        let room = width.saturating_sub(spans.iter().map(|s| s.width()).sum::<usize>() + 2);
        spans.push(Span::styled(format!(" {}", disks::prospects(d, r, room)), Style::new().fg(t.muted)));
        Line::from(spans)
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let t = self.theme;
        let mut title = vec![Span::styled(" found ", Style::new().fg(t.text))];
        if self.scanning {
            title.push(Span::styled("· still looking ", Style::new().fg(t.comment)));
        }
        let mut block =
            self.panel(t.blue).title(Line::from(title)).title_bottom(self.hints()).padding(Padding::horizontal(1));
        if self.editing || !self.filter.is_empty() {
            let cursor = if self.editing { "█" } else { "" };
            let filter = Line::from(vec![
                Span::styled(" / ", Style::new().fg(t.blue).add_modifier(Modifier::BOLD)),
                Span::styled(format!("{}{cursor} ", self.filter), Style::new().fg(t.cyan)),
            ]);
            block = block.title(filter.right_aligned());
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        let h = inner.height as usize;
        if let Some(r) = self.cursor_row() {
            if r < self.offset {
                self.offset = r;
            } else if r >= self.offset + h {
                self.offset = r + 1 - h;
            }
        }
        self.offset = self.offset.min(self.rows.len().saturating_sub(h));
        let w = inner.width as usize;
        let lines: Vec<Line> = self.rows.iter().skip(self.offset).take(h).map(|r| self.row_line(r, w)).collect();
        if lines.is_empty() {
            let msg = if self.scanning { " looking…" } else { " nothing matches" };
            f.render_widget(Paragraph::new(msg).style(Style::new().fg(t.comment)), inner);
            return;
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// The key hints in the list's bottom border, lazygit-style; a passing
    /// message takes their place for a few seconds.
    fn hints(&self) -> Line<'static> {
        let t = &self.theme;
        if let Some((msg, _)) = self.flash.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4)) {
            return Line::from(Span::styled(format!(" {msg} "), Style::new().fg(t.yellow)));
        }
        let keys: &[(&str, &str)] = if self.editing {
            &[("⏎", "keep filter"), ("esc", "clear")]
        } else {
            &[
                ("↑↓", "move"),
                ("→", "open"),
                ("←", "close"),
                ("space", "mark"),
                ("a", "all Safe"),
                ("/", "filter"),
                ("b", "backup"),
                ("⏎", "delete marked"),
                ("r", "rescan"),
                ("q", "quit"),
            ]
        };
        let mut spans = vec![Span::raw(" ")];
        for (key, what) in keys {
            spans.push(Span::styled(*key, Style::new().fg(t.blue).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(format!(" {what}  "), Style::new().fg(t.muted)));
        }
        Line::from(spans)
    }

    fn row_line(&self, row: &Row, width: usize) -> Line<'static> {
        let t = &self.theme;
        let indent = "  ".repeat(row.depth);
        let mark = self.check(&row.node);
        let mark_style = match mark {
            MARK_ALL | MARK_SOME => Style::new().fg(t.green).add_modifier(Modifier::BOLD),
            _ => Style::new().fg(t.comment),
        };
        let line = match &row.node {
            Node::Item(id) => {
                let Some(i) = self.item(*id) else { return Line::raw("") };
                let badge = i.badge();
                let size = match &i.verdict {
                    None => "…".to_string(),
                    Some(v) if v.unmeasured => "—".to_string(),
                    Some(v) => fmt::opt_bytes(v.bytes),
                };
                let reason = match &i.verdict {
                    Some(v) => v.aged_text(&v.reason, SystemTime::now()),
                    None => "measuring".into(),
                };
                let prefix = indent.len() + 2 + 1 + 1 + 6 + 8 + 2;
                let label_w = (width.saturating_sub(prefix) * 45 / 100).clamp(16, 64);
                let text = if badge == Some(Badge::Kept) { t.comment } else { t.fg };
                Line::from(vec![
                    Span::raw(format!("{indent}  ")),
                    Span::styled(mark, mark_style),
                    Span::raw(" "),
                    Span::styled(format!("{:<6}", badge.map(Badge::label).unwrap_or("")), self.badge_style(badge)),
                    Span::styled(format!("{size:>8}  "), Style::new().fg(text)),
                    Span::styled(format!("{:<label_w$}  ", fit(&i.label, label_w)), Style::new().fg(text)),
                    Span::styled(reason, Style::new().fg(t.muted)),
                ])
            }
            node => {
                let info = self.info.get(node).cloned().unwrap_or_default();
                let empty = self.is_empty(node);
                let arrow = if empty {
                    " "
                } else if self.is_open(node) {
                    "▾"
                } else {
                    "▸"
                };
                let (label, style) = match node {
                    Node::Kind(k) if empty => (k.title().to_string(), Style::new().fg(t.comment)),
                    Node::Kind(k) => (k.title().to_string(), Style::new().fg(t.blue).add_modifier(Modifier::BOLD)),
                    Node::Group(_, g) => (g.clone(), Style::new().fg(t.fg)),
                    _ => (format!("Kept · {}", fmt::num(info.count)), Style::new().fg(t.comment)),
                };
                let label_w = SUMMARY_COLUMN.saturating_sub(indent.len() + 4).max(12);
                let mut spans = vec![
                    Span::raw(indent.clone()),
                    Span::styled(format!("{arrow} "), Style::new().fg(t.comment)),
                    Span::styled(mark, mark_style),
                    Span::raw(" "),
                    Span::styled(format!("{:<label_w$}  ", fit(&label, label_w)), style),
                ];
                match node {
                    Node::Kind(k) if empty => {
                        spans.push(Span::styled(self.kind_status(*k), Style::new().fg(t.comment)))
                    }
                    Node::Kept(..) => {
                        spans.push(Span::styled(report::gists_line(&info.kept_why, 3), Style::new().fg(t.comment)))
                    }
                    _ => spans.extend(self.summary_spans(&info.totals)),
                }
                Line::from(spans)
            }
        };
        if self.cursor.as_ref() != Some(&row.node) {
            return line;
        }
        // The cursor row: Tokyo Night's selection, or reverse video without colour.
        let lift = |s: Style| {
            if self.no_color {
                s.add_modifier(Modifier::REVERSED)
            } else {
                s.bg(t.selection).add_modifier(Modifier::BOLD)
            }
        };
        let mut line = line;
        for span in &mut line.spans {
            span.style = lift(span.style);
        }
        let pad = width.saturating_sub(line.width());
        line.spans.push(Span::styled(" ".repeat(pad), lift(Style::new())));
        line
    }

    fn draw_detail(&self, f: &mut Frame, area: Rect) {
        let block = self.panel(self.theme.gutter).title(Span::styled(" details ", Style::new().fg(self.theme.text)));
        let lines = match &self.cursor {
            Some(Node::Item(id)) => self.item(*id).map(|i| self.item_detail(i)).unwrap_or_default(),
            Some(node) => self.group_detail(node),
            None => Vec::new(),
        };
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(block.padding(Padding::horizontal(1))),
            area,
        );
    }

    fn group_detail(&self, node: &Node) -> Vec<Line<'static>> {
        let t = &self.theme;
        let info = self.info.get(node).cloned().unwrap_or_default();
        let bold = Style::new().fg(t.fg).add_modifier(Modifier::BOLD);
        let quiet = Style::new().fg(t.muted);
        let (title, kind) = match node {
            Node::Kind(k) => (k.title().to_string(), *k),
            Node::Group(k, g) => (format!("{} › {g}", k.title()), *k),
            Node::Kept(k, Some(g)) => (format!("Kept · {} › {g}", k.title()), *k),
            Node::Kept(k, None) => (format!("Kept · {}", k.title()), *k),
            Node::Item(_) => unreachable!(),
        };
        let mut lines = vec![Line::styled(title, bold)];
        if let Node::Kept(..) = node {
            lines.push(Line::styled(
                format!(
                    "{} sweep won't delete: {}",
                    fmt::count(info.count, "item", "items"),
                    report::gists_line(&info.kept_why, 6)
                ),
                Style::new().fg(t.text),
            ));
            lines.push(Line::styled("→ lists them, each with its reason and checks", quiet));
            return lines;
        }
        match node {
            Node::Kind(_) => {
                lines.push(Line::styled(report::describe(kind, &self.thresholds), quiet));
                if self.is_empty(node) {
                    lines.push(Line::styled(self.kind_status(kind), Style::new().fg(t.text)));
                    return lines;
                }
                for (_, note) in self.notes.iter().filter(|(k, _)| *k == kind) {
                    lines.push(Line::styled(format!("Note: {note}"), quiet));
                }
            }
            Node::Group(_, g) => {
                if let Some(about) = self.about_group(kind, g) {
                    lines.push(Line::styled(about, quiet));
                }
            }
            _ => {}
        }
        lines.push(Line::from(self.summary_spans(&info.totals)));
        if !info.kept_why.is_empty() {
            lines.push(Line::styled(format!("Kept: {}", report::gists_line(&info.kept_why, 4)), quiet));
        }
        let marked = info.selectable.iter().filter(|id| self.selected.contains(id)).count();
        let marked_bytes: u64 = info
            .selectable
            .iter()
            .filter(|id| self.selected.contains(id))
            .filter_map(|&id| self.item(id).and_then(Item::bytes))
            .sum();
        if info.selectable.is_empty() {
            lines.push(Line::styled("Nothing in here can be deleted.", quiet));
        } else {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(
                        "Marked {} of {} · {}",
                        fmt::num(marked),
                        fmt::num(info.selectable.len()),
                        fmt::bytes(marked_bytes)
                    ),
                    bold,
                ),
                Span::styled("   space marks everything deletable in here", quiet),
            ]));
        }
        lines
    }

    /// What a group is, from one of its items.
    fn about_group(&self, kind: Kind, group: &str) -> Option<String> {
        let first =
            self.items.iter().flatten().find(|i| i.kind == kind && report::group_of(i).as_deref() == Some(group))?;
        match &first.target {
            Target::Temp { family, .. } => Some(format!("{family} · names matching {group}")),
            Target::Worktree(info) => Some(format!("worktrees of the repository at {}", info.main.display())),
            Target::CoreMl { .. } => Some(format!("models compiled for {group}")),
            Target::SimDevice(_) => Some(format!("simulators running {group}")),
            Target::Scratch { .. } => Some(format!("top-level folders of the scratch root {group}")),
            _ => None,
        }
    }

    fn item_detail(&self, i: &Item) -> Vec<Line<'static>> {
        let t = &self.theme;
        let quiet = Style::new().fg(t.muted);
        let mut lines = vec![Line::from(Span::styled(
            i.path.display().to_string(),
            Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
        ))];
        let Some(v) = &i.verdict else {
            lines.push(Line::styled("measuring…", quiet));
            return lines;
        };
        let v = &v.aged(SystemTime::now());
        let mut head = vec![
            Span::styled(v.badge.label(), self.badge_style(Some(v.badge)).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {}", v.reason), Style::new().fg(t.text)),
        ];
        let size = match v.bytes {
            Some(b) if v.badge == Badge::Kept => format!("  ·  holds {}", fmt::bytes(b)),
            Some(b) => format!("  ·  frees {}", fmt::bytes(b)),
            None if v.unmeasured => "  ·  measuring its size…".into(),
            None => "  ·  size unknown".into(),
        };
        head.push(Span::styled(size, Style::new().fg(t.fg)));
        if let (Some(b), Some(du)) = (v.bytes, v.du)
            && du > b + b / 10 + 1_000_000
        {
            head.push(Span::styled(
                format!(" (du says {}; the rest is shared with clones or snapshots and stays)", fmt::bytes(du)),
                quiet,
            ));
        }
        lines.push(Line::from(head));
        if v.dirty_worktree() {
            let on = self.backup_for(i.id);
            lines.push(Line::from(vec![
                Span::styled("b ", Style::new().fg(t.blue).add_modifier(Modifier::BOLD)),
                Span::styled(
                    if on {
                        "save uncommitted work on a backup branch first: on"
                    } else {
                        "save uncommitted work first: OFF, it will be discarded"
                    },
                    Style::new().fg(if on { t.green } else { t.red }),
                ),
            ]));
        }
        let mut checks: Vec<Span> = Vec::new();
        for c in &v.checks {
            let (mark, style) = match c.mark {
                Mark::Pass => ("✓\u{a0}", Style::new().fg(t.green)),
                Mark::Fail => ("✗\u{a0}", Style::new().fg(t.red)),
                Mark::Info => ("·\u{a0}", Style::new().fg(t.comment)),
            };
            // No-break spaces within a check, so lines wrap only between checks.
            checks.push(Span::styled(mark, style));
            checks.push(Span::styled(format!("{}   ", c.text.replace(' ', "\u{a0}")), Style::new().fg(t.text)));
        }
        lines.push(Line::from(checks));
        lines
    }

    fn draw_confirm(&self, f: &mut Frame) {
        let t = &self.theme;
        let chosen = self.chosen();
        let mut by_kind: Vec<(Kind, usize, u64, usize, usize)> = Vec::new();
        for i in &chosen {
            let review = (i.badge() == Some(Badge::Review)) as usize;
            let unknown = i.bytes().is_none() as usize;
            match by_kind.iter_mut().find(|e| e.0 == i.kind) {
                Some(e) => {
                    e.1 += 1;
                    e.2 += i.bytes().unwrap_or(0);
                    e.3 += review;
                    e.4 += unknown;
                }
                None => by_kind.push((i.kind, 1, i.bytes().unwrap_or(0), review, unknown)),
            }
        }
        by_kind.sort_by_key(|e| std::cmp::Reverse(e.2));
        let total: u64 = by_kind.iter().map(|e| e.2).sum();
        let unknown: usize = by_kind.iter().map(|e| e.4).sum();
        let review: usize = by_kind.iter().map(|e| e.3).sum();

        let accent = if self.dry_run { t.yellow } else { t.red };
        let title = if self.dry_run { " dry run: nothing will be deleted " } else { " Delete these? " };
        let text = Style::new().fg(t.text);
        let mut lines = vec![Line::raw("")];
        for (kind, n, bytes, rv, _) in &by_kind {
            let note = if *rv > 0 { format!("  {rv} Review") } else { String::new() };
            lines.push(Line::from(vec![
                Span::styled(format!("{:<30}{:>7}  {:>9}", kind.title(), fmt::num(*n), fmt::bytes(*bytes)), text),
                Span::styled(note, Style::new().fg(t.yellow)),
            ]));
        }
        lines.push(Line::styled("─".repeat(48), Style::new().fg(t.gutter)));
        lines.push(Line::from(Span::styled(
            format!("{:<30}{:>7}  {:>9}", "Frees", fmt::num(chosen.len()), fmt::bytes(total)),
            Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
        )));
        if unknown > 0 {
            lines.push(Line::styled(format!("plus {} of unknown size", fmt::count(unknown, "item", "items")), text));
        }
        lines.push(Line::raw(""));
        let dirty: Vec<&&Item> =
            chosen.iter().filter(|i| i.verdict.as_ref().is_some_and(|v| v.dirty_worktree())).collect();
        let saved = dirty.iter().filter(|i| self.backup_for(i.id)).count();
        if saved > 0 {
            let n = fmt::count(saved, "dirty worktree", "dirty worktrees");
            lines.push(Line::styled(format!("{n}: uncommitted work goes on a backup branch first"), text));
        }
        if dirty.len() > saved {
            let n = fmt::count(dirty.len() - saved, "dirty worktree", "dirty worktrees");
            lines.push(Line::styled(format!("{n}: uncommitted work will be DISCARDED"), Style::new().fg(t.red)));
        }
        let heads = chosen
            .iter()
            .filter(|i| {
                matches!(
                    i.verdict.as_ref().map(|v| &v.plan),
                    Some(crate::model::Plan::Worktree { branch_head: true, .. })
                )
            })
            .count();
        if heads > 0 {
            let n = fmt::count(heads, "detached worktree", "detached worktrees");
            lines.push(Line::styled(format!("{n}: HEAD kept on a backup branch first"), text));
        }
        if review > 0 {
            lines.push(Line::styled(
                format!("{} marked Review: a re-download, or your data", fmt::count(review, "item is", "items are")),
                Style::new().fg(t.yellow),
            ));
        }
        lines.push(Line::styled("Each item is checked again just before it goes.", Style::new().fg(t.muted)));
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled("y", Style::new().fg(accent).add_modifier(Modifier::BOLD)),
            Span::styled(if self.dry_run { " run the dry run   " } else { " delete   " }, Style::new().fg(t.muted)),
            Span::styled("any other key", Style::new().fg(t.blue).add_modifier(Modifier::BOLD)),
            Span::styled(" cancel", Style::new().fg(t.muted)),
        ]));
        let h = (lines.len() as u16 + 2).min(f.area().height);
        let w = 74.min(f.area().width);
        let area = Rect::new((f.area().width - w) / 2, (f.area().height.saturating_sub(h)) / 2, w, h);
        f.render_widget(Clear, area);
        let block = self
            .panel(accent)
            .style(Style::new().bg(t.surface).fg(t.fg))
            .title(Span::styled(title, Style::new().fg(accent).add_modifier(Modifier::BOLD)))
            .padding(Padding::horizontal(2));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_deletion(&self, f: &mut Frame, area: Rect) {
        let t = &self.theme;
        let Some(d) = &self.deletion else { return };
        let accent = if self.dry_run { t.yellow } else { t.green };
        let verb = match (self.dry_run, d.summary.is_some()) {
            (true, _) => " dry run ",
            (false, false) => " deleting ",
            (false, true) => " deleted ",
        };
        let block = self
            .panel(t.blue)
            .title(Span::styled(verb, Style::new().fg(accent).add_modifier(Modifier::BOLD)))
            .title_bottom(self.hints_deleting(d.summary.is_some()))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [gauge, status, _, list] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)])
                .areas(inner);
        let ratio = if d.total_bytes > 0 {
            (d.freed as f64 / d.total_bytes as f64).min(1.0)
        } else if d.total > 0 {
            d.finished as f64 / d.total as f64
        } else {
            1.0
        };
        let label = format!(
            "{} of {}  ·  {} / {}",
            fmt::bytes(d.freed),
            fmt::bytes(d.total_bytes),
            fmt::num(d.finished),
            fmt::num(d.total)
        );
        f.render_widget(
            LineGauge::default()
                .filled_symbol("━")
                .unfilled_symbol("─")
                .filled_style(Style::new().fg(accent))
                .unfilled_style(Style::new().fg(t.gutter))
                .label(Span::styled(label, Style::new().fg(t.fg).add_modifier(Modifier::BOLD)))
                .ratio(ratio),
            gauge,
        );
        let elapsed = d.started.elapsed().as_secs();
        let status_line = match &d.summary {
            Some(s) => {
                let mut text = if s.dry_run {
                    format!(
                        "Dry run done: would free {} across {}.",
                        fmt::bytes(s.freed),
                        fmt::count(s.deleted, "item", "items")
                    )
                } else {
                    format!("Done: freed {} across {}.", fmt::bytes(s.freed), fmt::count(s.deleted, "item", "items"))
                };
                if s.skipped > 0 {
                    text += &format!(" {} skipped.", s.skipped);
                }
                if s.failed > 0 {
                    text += &format!(" {} failed.", s.failed);
                }
                if let Some(log) = &s.log {
                    text += &format!(" Log: {}", log.display());
                }
                Line::styled(text, Style::new().fg(t.fg).add_modifier(Modifier::BOLD))
            }
            None => Line::styled(
                format!("{} under way · {}:{:02} elapsed", d.running.len(), elapsed / 60, elapsed % 60),
                Style::new().fg(t.muted),
            ),
        };
        f.render_widget(Paragraph::new(status_line), status);
        let h = list.height as usize;
        let lines: Vec<Line> = d
            .recent
            .iter()
            .rev()
            .take(h)
            .map(|(id, o)| {
                let label = self.item(*id).map(|i| i.label.clone()).unwrap_or_default();
                let quiet = Style::new().fg(t.muted);
                match o {
                    Outcome::Deleted { freed, note } | Outcome::WouldDelete { freed, note } => Line::from(vec![
                        Span::styled("✓ ", Style::new().fg(t.green)),
                        Span::styled(format!("{:>8}  ", fmt::bytes(*freed)), Style::new().fg(t.fg)),
                        Span::styled(label, Style::new().fg(t.text)),
                        Span::styled(if note.is_empty() { String::new() } else { format!("  {note}") }, quiet),
                    ]),
                    Outcome::Skipped(why) => Line::from(vec![
                        Span::styled("– ", Style::new().fg(t.yellow)),
                        Span::styled(format!("{:>8}  ", "skipped"), Style::new().fg(t.yellow)),
                        Span::styled(label, Style::new().fg(t.text)),
                        Span::styled(format!("  {why}"), quiet),
                    ]),
                    Outcome::Failed(err) => Line::from(vec![
                        Span::styled("✗ ", Style::new().fg(t.red)),
                        Span::styled(format!("{:>8}  ", "failed"), Style::new().fg(t.red)),
                        Span::styled(label, Style::new().fg(t.text)),
                        Span::styled(format!("  {err}"), Style::new().fg(t.red)),
                    ]),
                }
            })
            .collect();
        f.render_widget(Paragraph::new(lines), list);
    }

    fn hints_deleting(&self, done: bool) -> Line<'static> {
        let t = &self.theme;
        if let Some((msg, _)) = self.flash.as_ref().filter(|(_, at)| at.elapsed() < Duration::from_secs(4)) {
            return Line::from(Span::styled(format!(" {msg} "), Style::new().fg(t.yellow)));
        }
        let (key, what) =
            if done { ("any key", "back to the list") } else { ("esc", "stop after the items under way") };
        Line::from(vec![
            Span::raw(" "),
            Span::styled(key, Style::new().fg(t.blue).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {what} "), Style::new().fg(t.muted)),
        ])
    }
}

const MARK_ALL: &str = "●";
const MARK_SOME: &str = "◐";
const MARK_NONE: &str = "○";

/// How wide a disk's name column is.
const DISK_NAME_W: usize = 28;

/// How much of a disk's line goes to words before and after the bar. What
/// sweep could free comes last and is the part cut short on a narrow screen.
/// The width of a disk's line besides its bar, with room for what sweep can
/// free; marking doesn't resize the bars, it shortens the wording.
fn disk_words(d: &Disk, _r: &disks::Reclaim) -> usize {
    let usage = format!("  {}  ", disks::usage(d)).chars().count();
    3 + DISK_NAME_W + 1 + usage + 8 + 40
}

/// Shortens from the end, for names whose start matters.
fn fit_end(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    let head: String = s.chars().take(w.saturating_sub(1)).collect();
    format!("{head}…")
}

fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    let keep = w.saturating_sub(1);
    let tail: String = s.chars().skip(n - keep).collect();
    format!("…{tail}")
}

pub fn run(ctx: Arc<Ctx>, pool: Arc<ThreadPool>, dry_run: bool, hello: Option<String>) -> Result<()> {
    // The scan, and then whatever changes while sweep stays open.
    let (rx, live) = live::spawn(ctx.clone(), pool);
    let mut app = App::new(dry_run, ctx.cfg.thresholds.clone());
    app.live = Some(live);
    if let Some(h) = hello {
        app.say(h);
    }
    let (disk_tx, disk_rx) = channel();
    std::thread::spawn(move || {
        let _ = disk_tx.send(disks::survey().map_err(|e| format!("{e:#}")));
    });
    drive(app, &ctx, Some(rx), Some(disk_rx))
}

/// The interface on an app made up front, with nothing scanning or watching.
pub fn run_demo(app: App, ctx: Arc<Ctx>) -> Result<()> {
    drive(app, &ctx, None, None)
}

type Survey = Result<Vec<Disk>, String>;

fn drive(
    mut app: App,
    ctx: &Arc<Ctx>,
    rx: Option<Receiver<ScanEvent>>,
    disk_rx: Option<Receiver<Survey>>,
) -> Result<()> {
    let (mut rested_on, mut since) = (None, Instant::now());
    let mut terminal = ratatui::init();
    let result = (|| -> Result<()> {
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut disks_read = Instant::now();
        loop {
            let mut changed = false;
            while let Some(ev) = rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                app.on_scan(ev);
                changed = true;
            }
            if let Some(survey) = disk_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
                app.set_disks(survey);
                changed = true;
            }
            // Free space is a statfs per disk: cheap enough to keep live.
            if disks_read.elapsed() > Duration::from_secs(1) {
                app.refresh_disks();
                disks_read = Instant::now();
                changed = true;
            }
            if app.cursor != rested_on {
                (rested_on, since) = (app.cursor.clone(), Instant::now());
            }
            // A kept item is measured once the cursor rests on it after the scan.
            if !app.scanning
                && since.elapsed() > Duration::from_millis(400)
                && let Some(Node::Item(id)) = app.cursor
                && app.item(id).and_then(|i| i.verdict.as_ref()).is_some_and(|v| v.unmeasured)
            {
                app.measure(id);
            }
            if app.deletion.is_some() {
                app.poll_deletion();
                changed = true;
            }
            // Left alone it draws once a second, for free space and passing messages.
            if changed || last.elapsed() > Duration::from_secs(1) {
                terminal.draw(|f| app.draw(f))?;
                last = Instant::now();
            }
            if event::poll(Duration::from_millis(60))?
                && let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                app.on_key(k, ctx);
                terminal.draw(|f| app.draw(f))?;
            }
            if app.quit {
                let busy = app.deletion.as_ref().is_some_and(|d| d.summary.is_none());
                if !busy {
                    break;
                }
                app.quit = false;
                app.say("waiting for the items under way to finish");
            }
        }
        Ok(())
    })();
    ratatui::restore();
    result
}

/// The demo's deletion: each item takes a moment, four at a time, and frees
/// what it said it would.
fn pretend_to_delete(jobs: Vec<(usize, u64, String)>, tx: &Sender<Progress>, done: &Sender<Summary>) {
    let mut s = Summary::default();
    for batch in jobs.chunks(4) {
        for (id, ..) in batch {
            let _ = tx.send(Progress::Started(*id));
        }
        for (id, bytes, volume) in batch {
            std::thread::sleep(Duration::from_millis(90 + (bytes / 1_000_000_000).min(20) * 15));
            let _ = tx.send(Progress::Finished(*id, Outcome::Deleted { freed: *bytes, note: String::new() }));
            s.deleted += 1;
            s.freed += bytes;
            match s.space.iter_mut().find(|(v, _)| v == volume) {
                Some((_, gained)) => *gained += *bytes as i64,
                None => s.space.push((volume.clone(), *bytes as i64)),
            }
        }
    }
    let _ = done.send(s);
}

/// Renders one frame as text, for checking the layout without a terminal.
pub fn snapshot(app: &mut App, width: u16, height: u16, confirm: bool) -> String {
    use ratatui::backend::TestBackend;
    if confirm && !app.chosen().is_empty() {
        app.mode = Mode::Confirm;
    }
    let mut term = ratatui::Terminal::new(TestBackend::new(width, height)).expect("test backend");
    term.draw(|f| app.draw(f)).expect("draw");
    let buf = term.backend().buffer().clone();
    let mut out = String::new();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(&buf[(x, y)].symbol().replace('\u{a0}', " "));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

impl App {
    pub fn select_safe_for_snapshot(&mut self) {
        self.select_all_safe();
        self.flash = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Check, Verdict, WorktreeInfo};
    use std::path::PathBuf;

    fn item(id: usize, label: &str, badge: Badge, bytes: u64, target: Target) -> Item {
        let mut v = Verdict::new(badge, format!("{} reason", badge.label()));
        if badge == Badge::Kept {
            v.reason = "in use: written 3 h ago".into();
        }
        v.bytes = Some(bytes);
        v.checks.push(Check::pass("nothing written for 3 days"));
        let kind = match target {
            Target::Worktree(_) => Kind::Worktree,
            Target::Logs => Kind::Logs,
            _ => Kind::Temp,
        };
        Item { id, kind, path: PathBuf::from(format!("/x/{label}")), label: label.into(), target, verdict: Some(v) }
    }

    fn temp(pattern: &'static str) -> Target {
        Target::Temp { pattern, family: "temp folder" }
    }

    fn wt() -> Target {
        Target::Worktree(WorktreeInfo {
            main: "/src/repo".into(),
            head: String::new(),
            branch: None,
            locked: None,
            nested: vec![],
        })
    }

    fn app() -> App {
        let mut a = untouched();
        a.touched = true;
        a
    }

    fn untouched() -> App {
        App::with_items(
            vec![
                item(0, "tmp-small", Badge::Safe, 1_000_000, temp("tmp*")),
                item(1, "tmp-big", Badge::Safe, 9_000_000_000, temp("tmp*")),
                item(2, "repo-x", Badge::Review, 3_000_000_000, wt()),
                item(3, "tmp-busy", Badge::Kept, 50_000_000_000, temp("tmp*")),
                item(4, "Old", Badge::Safe, 10, Target::Logs),
                item(5, "TemporaryDirectory.a", Badge::Safe, 1_000, temp("TemporaryDirectory.*")),
                item(6, "TemporaryDirectory.b", Badge::Kept, 1_000, temp("TemporaryDirectory.*")),
                item(7, "repo-y", Badge::Kept, 1_000, wt()),
            ],
            vec![],
            false,
            Thresholds::default(),
        )
    }

    /// The rows, leaving out kinds with nothing in them.
    fn nodes(a: &App) -> Vec<(Node, usize)> {
        a.rows.iter().filter(|r| !a.is_empty(&r.node)).map(|r| (r.node.clone(), r.depth)).collect()
    }

    #[test]
    fn every_kind_keeps_its_row_with_what_its_rule_found() {
        let mut a = App::new(false, Thresholds::default());
        a.touched = true;
        a.rebuild();
        let kinds: Vec<Node> = a.rows.iter().map(|r| r.node.clone()).collect();
        assert_eq!(kinds, Kind::ALL.map(Node::Kind), "all kinds, in rule order, from the first frame");
        assert_eq!(a.kind_status(Kind::Docker), "looking…");
        a.on_scan(ScanEvent::Looked(Kind::Docker));
        a.on_scan(ScanEvent::Note(Kind::Downloads, "macOS didn't allow reading ~/Downloads".into()));
        a.on_scan(ScanEvent::Looked(Kind::Downloads));
        assert_eq!(a.kind_status(Kind::Docker), "nothing found");
        assert_eq!(a.kind_status(Kind::Downloads), "macOS didn't allow reading ~/Downloads");
        a.rebuild();
        let screen = snapshot(&mut a, 120, 30, false);
        let docker = screen.lines().find(|l| l.contains("Docker Desktop disk")).unwrap();
        assert!(docker.trim_end_matches(['│', ' ']).ends_with("nothing found"), "{screen}");
        assert!(screen.contains("looking…"), "{screen}");
        a.cursor = Some(Node::Kind(Kind::Docker));
        a.open();
        a.rebuild();
        a.open();
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Docker)), "→ on an open, empty row stays put");

        // A kind opened while its rule is still looking shows what turns up.
        a.cursor = Some(Node::Kind(Kind::Logs));
        a.open();
        a.on_scan(ScanEvent::Found(untouched().items[4].clone().unwrap()));
        a.rebuild();
        assert!(a.rows.iter().any(|r| r.node == Node::Item(4)));
    }

    #[test]
    fn changes_while_open_unmark_what_became_kept_or_went() {
        let mut a = app();
        a.selected.extend([0, 1]);
        let mut busy = Verdict::new(Badge::Kept, "in use: written just now");
        busy.unmeasured = true;
        a.on_scan(ScanEvent::Measured(0, busy));
        assert!(!a.selected.contains(&0));
        assert_eq!(a.flash.as_ref().unwrap().0, "unmarked tmp-small: in use: written just now");
        a.on_scan(ScanEvent::Gone(1));
        assert!(!a.selected.contains(&1) && a.item(1).is_none());
        assert_eq!(a.flash.as_ref().unwrap().0, "unmarked tmp-big: it's gone");
        a.on_scan(ScanEvent::Note(Kind::Temp, "left 3 entries in $TMPDIR alone".into()));
        a.on_scan(ScanEvent::ClearNotes(Kind::Temp));
        assert!(a.notes.is_empty(), "a rule looking again starts its notes afresh");
        a.on_scan(ScanEvent::Watching(Ok(())));
        a.flash = None;
        a.rebuild();
        let screen = snapshot(&mut a, 160, 30, false);
        assert!(screen.contains("watching for changes"), "{screen}");
        assert!(screen.contains("r rescan"), "{screen}");
    }

    #[test]
    fn opens_on_an_overview_of_kinds_in_rule_order() {
        let a = app();
        assert_eq!(
            nodes(&a),
            [(Node::Kind(Kind::Worktree), 0), (Node::Kind(Kind::Temp), 0), (Node::Kind(Kind::Logs), 0)]
        );
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Worktree)));
    }

    #[test]
    fn the_cursor_waits_on_top_until_a_key_is_pressed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(crate::ctx::test_ctx(dir.path(), std::time::SystemTime::now()));
        let mut a = App::new(false, Thresholds::default());
        let all = untouched().items;
        // Logs are found first; worktrees and temp folders turn up later, above them.
        a.on_scan(ScanEvent::Found(all[4].clone().unwrap()));
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Worktree)), "the top row is there before anything is found");
        for i in [0, 2] {
            a.on_scan(ScanEvent::Found(all[i].clone().unwrap()));
        }
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Worktree)), "still on the top row");
        // Past the empty row for build output.
        for _ in 0..2 {
            a.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &ctx);
        }
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Temp)));
        a.on_scan(ScanEvent::Found(all[5].clone().unwrap()));
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Temp)), "once a key is pressed the cursor stays put");
    }

    #[test]
    fn nothing_moves_while_the_scan_runs() {
        let mut a = app();
        a.scanning = true;
        a.expand_all();
        let before = nodes(&a);
        let at = |rows: &[(Node, usize)], n: Node| rows.iter().position(|r| r.0 == n).unwrap();
        assert!(at(&before, Node::Item(0)) < at(&before, Node::Item(1)), "found order while scanning");
        // A late measurement makes the other group the biggest.
        a.items[5].as_mut().unwrap().verdict.as_mut().unwrap().bytes = Some(99_000_000_000);
        a.rebuild();
        assert_eq!(nodes(&a), before);

        a.scanning = false;
        a.rebuild();
        let after = nodes(&a);
        let (tmp, td) =
            (Node::Group(Kind::Temp, "tmp*".into()), Node::Group(Kind::Temp, "TemporaryDirectory.*".into()));
        assert!(at(&after, td) < at(&after, tmp), "groups sort by size once the scan is done");
        assert!(at(&after, Node::Item(1)) < at(&after, Node::Item(0)), "and so do items");
    }

    #[test]
    fn opening_kinds_groups_and_the_kept_fold() {
        let mut a = app();
        a.cursor = Some(Node::Kind(Kind::Temp));
        a.open();
        a.rebuild();
        let tmp = Node::Group(Kind::Temp, "tmp*".into());
        assert_eq!(
            nodes(&a)[1..4],
            [
                (Node::Kind(Kind::Temp), 0),
                (tmp.clone(), 1),
                (Node::Group(Kind::Temp, "TemporaryDirectory.*".into()), 1)
            ]
        );
        a.open(); // already open: steps onto the first group
        assert_eq!(a.cursor, Some(tmp.clone()));
        a.open();
        a.rebuild();
        let kept = Node::Kept(Kind::Temp, Some("tmp*".into()));
        assert_eq!(
            nodes(&a)[2..6],
            [(tmp.clone(), 1), (Node::Item(1), 2), (Node::Item(0), 2), (kept.clone(), 2)],
            "Kept items fold below the ones that can go, however big"
        );
        a.cursor = Some(kept.clone());
        a.open();
        a.rebuild();
        assert!(nodes(&a).contains(&(Node::Item(3), 3)));

        // A kind with one repository skips the group level.
        a.expanded.insert(Node::Kind(Kind::Worktree));
        a.rebuild();
        let rows = nodes(&a);
        let at = rows.iter().position(|r| r.0 == Node::Kind(Kind::Worktree)).unwrap();
        assert_eq!(rows[at + 1], (Node::Item(2), 1));
        assert_eq!(rows[at + 2], (Node::Kept(Kind::Worktree, None), 1));
    }

    #[test]
    fn closing_from_inside_a_group_closes_it_and_lands_on_it() {
        let mut a = app();
        a.expand_all();
        a.cursor = Some(Node::Item(0));
        a.close();
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Group(Kind::Temp, "tmp*".into())));
        assert!(!nodes(&a).contains(&(Node::Item(0), 2)), "one press closes the group");
        a.close();
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Temp)), "a closed group closes the kind it's in");
        assert!(!nodes(&a).iter().any(|(n, _)| matches!(n, Node::Group(Kind::Temp, _))));

        a.expand_all();
        a.cursor = Some(Node::Item(5));
        a.expanded.remove(&Node::Kind(Kind::Temp));
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Temp)), "a row that folds away hands the cursor up");
    }

    #[test]
    fn arrow_keys_open_and_close() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Arc::new(crate::ctx::test_ctx(dir.path(), std::time::SystemTime::now()));
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        let mut a = app();
        // Past the empty row for build output.
        a.on_key(key(KeyCode::Down), &ctx);
        a.on_key(key(KeyCode::Down), &ctx);
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Temp)));
        a.on_key(key(KeyCode::Right), &ctx);
        a.rebuild();
        assert!(nodes(&a).contains(&(Node::Group(Kind::Temp, "tmp*".into()), 1)));
        a.on_key(key(KeyCode::Right), &ctx);
        assert_eq!(a.cursor, Some(Node::Group(Kind::Temp, "tmp*".into())), "→ on an open row steps inside");
        a.on_key(key(KeyCode::Right), &ctx);
        a.rebuild();
        a.on_key(key(KeyCode::Down), &ctx);
        assert_eq!(a.cursor, Some(Node::Item(1)));
        a.on_key(key(KeyCode::Left), &ctx);
        a.rebuild();
        assert_eq!(a.cursor, Some(Node::Group(Kind::Temp, "tmp*".into())));
        a.on_key(key(KeyCode::Left), &ctx);
        a.rebuild();
        assert_eq!(nodes(&a).len(), 3, "back to the overview");
        assert_eq!(a.cursor, Some(Node::Kind(Kind::Temp)));
    }

    #[test]
    fn marking_a_row_marks_everything_deletable_under_it() {
        let mut a = app();
        a.cursor = Some(Node::Kind(Kind::Temp));
        a.toggle();
        assert_eq!(a.selected, BTreeSet::from([0, 1, 5]), "every Safe and Review item, never a Kept one");
        assert_eq!(a.check(&Node::Kind(Kind::Temp)), MARK_ALL);
        a.selected.remove(&0);
        assert_eq!(a.check(&Node::Kind(Kind::Temp)), MARK_SOME);
        assert_eq!(a.check(&Node::Group(Kind::Temp, "tmp*".into())), MARK_SOME);
        assert_eq!(a.check(&Node::Group(Kind::Temp, "TemporaryDirectory.*".into())), MARK_ALL);
        a.toggle();
        assert_eq!(a.selected, BTreeSet::from([0, 1, 5]));
        a.toggle();
        assert!(a.selected.is_empty());

        a.cursor = Some(Node::Kind(Kind::Worktree));
        a.toggle();
        assert_eq!(a.selected, BTreeSet::from([2]), "Review items too, when you mark their group");
        a.cursor = Some(Node::Kept(Kind::Temp, Some("tmp*".into())));
        a.toggle();
        assert_eq!(a.selected, BTreeSet::from([2]));
    }

    #[test]
    fn a_marks_every_safe_item_and_filtering_opens_what_matches() {
        let mut a = app();
        a.select_all_safe();
        assert_eq!(a.selected, BTreeSet::from([0, 1, 4, 5]));
        a.select_all_safe();
        assert!(a.selected.is_empty());

        a.filter = "busy".into();
        a.rebuild();
        assert_eq!(nodes(&a), [(Node::Kind(Kind::Temp), 0), (Node::Item(3), 1)]);
        a.filter = "big".into();
        a.rebuild();
        a.select_all_safe();
        assert_eq!(a.selected, BTreeSet::from([1]), "a marks the Safe items that match");
    }

    fn cursor_row_cells(a: &mut App, keep: impl Fn(&ratatui::buffer::Cell) -> bool) -> usize {
        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(100, 20)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        let buf = term.backend().buffer().clone();
        let text = |y: u16| (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>();
        let row = (0..buf.area.height).find(|&y| text(y).contains("○ Temp folders")).unwrap();
        (0..buf.area.width).filter(|&x| keep(&buf[(x, row)])).count()
    }

    #[test]
    fn the_cursor_row_spans_the_panel() {
        // 100 columns, less a margin, a border and a column of padding on each side.
        let inner = 100 - 2 - 2 - 2;
        let mut a = app();
        a.cursor = Some(Node::Kind(Kind::Temp));
        a.no_color = true;
        assert_eq!(
            cursor_row_cells(&mut a, |c| c.modifier.contains(Modifier::REVERSED)),
            inner,
            "reverse video without colour"
        );
        a.no_color = false;
        let selection = a.theme.selection;
        assert_eq!(cursor_row_cells(&mut a, |c| c.bg == selection), inner, "Tokyo Night's selection with it");
    }

    #[test]
    fn the_glance_shows_every_disk_with_bars_that_compare() {
        use crate::disks::Store;
        let mut a = app();
        a.no_color = false;
        // Items under /x land on whatever disk holds /, so give that one a name.
        let root = disks::parent_disk(&disks::device_of(std::path::Path::new("/")).unwrap()).to_string();
        let disk = |id: &str, store: &str, free: u64, startup: bool, name: &str| Disk {
            id: id.into(),
            model: "SSD".into(),
            internal: startup,
            startup,
            size: 1_000_000_000_000,
            volumes: vec![name.into()],
            stores: vec![Store { id: store.into(), free, probe: None }],
        };
        a.set_disks(Ok(vec![
            disk("disk0", &root, 369_000_000_000, true, "Macintosh HD"),
            disk("disk6", "disk-not-here", 12_000_000_000, false, "SSD"),
        ]));
        a.selected.insert(1);
        let screen = snapshot(&mut a, 220, 30, false);
        let lines: Vec<&str> = screen.lines().filter(|l| l.contains(" ● ")).collect();
        assert_eq!(lines.len(), 2, "{screen}");
        assert!(lines[0].contains("Internal · Macintosh HD"), "{screen}");
        assert!(lines[0].contains("63% used · 369 GB free of 1.0 TB  OK"), "{screen}");
        assert!(lines[0].contains("sweep frees 9.0 GB · 3.0 GB to review · marked 9.0 GB → 378 GB free"), "{screen}");
        assert!(lines[1].contains("External · SSD") && lines[1].contains("CRITICAL"), "{screen}");
        let bar = |l: &str| l.chars().filter(|c| "━─".contains(*c)).count();
        assert!(bar(lines[0]) >= 10, "{screen}");
        assert_eq!(bar(lines[0]), bar(lines[1]), "{screen}");

        use ratatui::backend::TestBackend;
        let mut term = ratatui::Terminal::new(TestBackend::new(220, 30)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        assert_eq!(term.backend().buffer()[(1, 0)].fg, a.theme.red, "a critical disk turns the frame red");
    }

    #[test]
    fn the_screen_and_the_confirmation() {
        let mut a = app();
        a.cursor = Some(Node::Kind(Kind::Temp));
        let overview = snapshot(&mut a, 120, 24, false);
        assert!(overview.contains("▸ ○ Temp folders"), "{overview}");
        assert!(overview.contains("Safe 9.0 GB (3)"), "{overview}");
        assert!(overview.contains("Kept 50 GB (2)"), "{overview}");
        assert!(overview.contains("$TMPDIR entries left by known tools"), "{overview}");

        a.toggle();
        a.expand_all();
        let open = snapshot(&mut a, 120, 30, false);
        assert!(open.contains("▾ ● tmp*"), "{open}");
        assert!(open.contains("● Safe    9.0 GB  tmp-big"), "{open}");
        assert!(open.contains("▸   Kept · 1"), "{open}");
        assert!(open.contains("Marked 3 · 9.0 GB"), "{open}");

        let confirm = snapshot(&mut a, 120, 30, true);
        assert!(confirm.contains("Delete these?"), "{confirm}");
        assert!(confirm.contains("Frees"), "{confirm}");
    }
}

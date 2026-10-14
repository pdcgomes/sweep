use crate::fmt;
use serde::Serialize;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Worktree,
    BuildOutput,
    Temp,
    AppCache,
    DerivedData,
    XcodeSupport,
    CoreMl,
    SimDevice,
    SimRuntime,
    Docker,
    Logs,
    PackageCache,
    Extension,
    Model,
    Scratch,
    Downloads,
    Trash,
    Snapshot,
}

impl Kind {
    pub const ALL: [Kind; 18] = [
        Kind::Worktree,
        Kind::BuildOutput,
        Kind::Temp,
        Kind::AppCache,
        Kind::DerivedData,
        Kind::XcodeSupport,
        Kind::CoreMl,
        Kind::SimDevice,
        Kind::SimRuntime,
        Kind::Docker,
        Kind::Logs,
        Kind::PackageCache,
        Kind::Extension,
        Kind::Model,
        Kind::Scratch,
        Kind::Downloads,
        Kind::Trash,
        Kind::Snapshot,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Kind::Worktree => "Git worktrees",
            Kind::BuildOutput => "Build output in idle projects",
            Kind::Temp => "Temp folders",
            Kind::AppCache => "App caches",
            Kind::DerivedData => "Xcode DerivedData",
            Kind::XcodeSupport => "Xcode support files",
            Kind::CoreMl => "Core ML model caches",
            Kind::SimDevice => "Simulator devices",
            Kind::SimRuntime => "Simulator runtimes",
            Kind::Docker => "Docker Desktop disk",
            Kind::Logs => "Logs",
            Kind::PackageCache => "Package caches",
            Kind::Extension => "Old editor extensions",
            Kind::Model => "AI models",
            Kind::Scratch => "Scratch folders",
            Kind::Downloads => "Downloads",
            Kind::Trash => "Trash",
            Kind::Snapshot => "Time Machine local snapshots",
        }
    }

    /// The name used on the command line and in JSON.
    pub fn slug(self) -> &'static str {
        match self {
            Kind::Worktree => "worktree",
            Kind::BuildOutput => "build-output",
            Kind::Temp => "temp",
            Kind::AppCache => "app-cache",
            Kind::DerivedData => "derived-data",
            Kind::XcodeSupport => "xcode-support",
            Kind::CoreMl => "core-ml",
            Kind::SimDevice => "sim-device",
            Kind::SimRuntime => "sim-runtime",
            Kind::Docker => "docker",
            Kind::Logs => "logs",
            Kind::PackageCache => "package-cache",
            Kind::Extension => "extension",
            Kind::Model => "model",
            Kind::Scratch => "scratch",
            Kind::Downloads => "downloads",
            Kind::Trash => "trash",
            Kind::Snapshot => "snapshot",
        }
    }

    pub fn from_slug(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.slug() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Badge {
    Safe,
    Review,
    Kept,
}

impl Badge {
    pub fn label(self) -> &'static str {
        match self {
            Badge::Safe => "Safe",
            Badge::Review => "Review",
            Badge::Kept => "Kept",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mark {
    Pass,
    Fail,
    /// A fact worth showing that doesn't decide the badge.
    Info,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Check {
    pub mark: Mark,
    pub text: String,
}

impl Check {
    pub fn pass(text: impl Into<String>) -> Check {
        Check { mark: Mark::Pass, text: text.into() }
    }
    pub fn fail(text: impl Into<String>) -> Check {
        Check { mark: Mark::Fail, text: text.into() }
    }
    pub fn info(text: impl Into<String>) -> Check {
        Check { mark: Mark::Info, text: text.into() }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorktreeInfo {
    /// The repository's main checkout, where `git worktree remove` runs.
    pub main: PathBuf,
    pub head: String,
    pub branch: Option<String>,
    pub locked: Option<String>,
    /// Other worktrees inside this one.
    pub nested: Vec<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimDeviceInfo {
    pub udid: String,
    pub name: String,
    pub runtime: String,
    pub state: String,
    pub available: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SimRuntimeInfo {
    pub identifier: String,
    pub name: String,
    pub build: String,
    pub size_bytes: Option<u64>,
    pub last_used: Option<SystemTime>,
    pub deletable: bool,
    pub state: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheTool {
    Pip,
    Uv,
    Homebrew,
    Npm,
    Yarn,
    SwiftPm,
    Playwright,
    NodeGyp,
}

/// Where an app's cache folder is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CachePlace {
    /// ~/Library/Caches.
    Library,
    /// The per-user cache folder macOS keeps beside $TMPDIR.
    User,
}

/// One of the things Xcode keeps beside DerivedData.
#[derive(Clone, Debug, PartialEq)]
pub enum XcodeFile {
    /// Symbols copied from a device; `newer` is a later OS copied from the same kind of device.
    DeviceSupport {
        newer: Option<String>,
    },
    Documentation,
    Products,
    Previews,
    TestDevices,
    Archive,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ModelSource {
    /// An Ollama model, and the blobs (by digest) it's made of.
    Ollama { name: String, blobs: Vec<String> },
    /// A model some app downloaded: the app, and what brings the model back.
    Download { app: &'static str, again: &'static str },
}

/// What an item is, with whatever its rule needs to re-check and delete it.
#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    Worktree(WorktreeInfo),
    /// A project's generated folder, named after what generates it.
    BuildOutput {
        project: PathBuf,
        output: &'static str,
    },
    Temp {
        pattern: &'static str,
        family: &'static str,
    },
    AppCache {
        owner: String,
        place: CachePlace,
    },
    DerivedData {
        workspace: Option<PathBuf>,
    },
    Xcode(XcodeFile),
    CoreMl {
        owner: String,
        build: String,
        model: Option<String>,
    },
    SimDevice(SimDeviceInfo),
    SimRuntime(SimRuntimeInfo),
    Docker,
    Logs,
    PackageCache(CacheTool),
    /// An editor extension's folder the editor no longer uses: the editor, and its bundle identifier.
    Extension {
        editor: &'static str,
        app: &'static str,
    },
    Model(ModelSource),
    Scratch {
        kept_by: Option<String>,
        days: f64,
    },
    Download,
    Trash,
    Snapshot {
        volume: PathBuf,
        name: String,
    },
}

/// Extra steps a deletion takes, decided when the item is checked.
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Plan {
    #[default]
    Remove,
    Worktree {
        head: String,
        dirty: bool,
        /// A detached HEAD that nothing else keeps gets a branch first.
        branch_head: bool,
        backup_ref: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    pub badge: Badge,
    pub reason: String,
    pub checks: Vec<Check>,
    /// What deleting frees; `None` when it can't be known.
    pub bytes: Option<u64>,
    /// What `du` would say, when that differs in kind (clones, snapshots).
    pub du: Option<u64>,
    pub last_write: Option<SystemTime>,
    /// Nothing to do after all (a log folder with no old files).
    pub hidden: bool,
    /// Kept for a reason known without measuring, and not measured (yet).
    pub unmeasured: bool,
    pub plan: Plan,
    /// When it was decided. Its "3 min ago"s were true then; `aged` brings
    /// them up to date.
    pub as_of: Option<SystemTime>,
}

impl Verdict {
    pub fn new(badge: Badge, reason: impl Into<String>) -> Verdict {
        Verdict {
            badge,
            reason: reason.into(),
            checks: Vec::new(),
            bytes: None,
            du: None,
            last_write: None,
            hidden: false,
            unmeasured: false,
            plan: Plan::Remove,
            as_of: None,
        }
    }

    /// The same verdict with its size out of date, for an item that changed
    /// inside but stays kept.
    pub fn stale(&self) -> Verdict {
        Verdict { bytes: None, du: None, unmeasured: true, ..self.clone() }
    }

    /// Its reason and checks with their ages as of `now`: "written 3 min ago"
    /// says "written 1 h ago" an hour later. The ages count from the last
    /// write, which is what they describe.
    pub fn aged(&self, now: SystemTime) -> Verdict {
        let mut v = self.clone();
        v.reason = self.aged_text(&self.reason, now);
        for c in &mut v.checks {
            c.text = self.aged_text(&c.text, now);
        }
        v.as_of = Some(now);
        v
    }

    /// One of its texts with its age as of `now`.
    pub fn aged_text(&self, text: &str, now: SystemTime) -> String {
        let (Some(at), Some(w)) = (self.as_of, self.last_write) else { return text.to_string() };
        let then = at.duration_since(w).unwrap_or_default();
        let later = now.duration_since(w).unwrap_or_default();
        for (old, new) in [(fmt::ago(then), fmt::ago(later)), (fmt::age(then), fmt::age(later))] {
            if old != new && text.contains(old.as_str()) {
                return text.replacen(old.as_str(), &new, 1);
            }
        }
        text.to_string()
    }

    /// Whether two verdicts say the same now: a later write that still reads
    /// "written just now" changes nothing anyone sees.
    pub fn same(&self, other: &Verdict) -> bool {
        let at = self.as_of.max(other.as_of).unwrap_or(SystemTime::UNIX_EPOCH);
        let (a, b) = (self.aged(at), other.aged(at));
        a == Verdict { last_write: a.last_write, ..b }
    }

    /// Marks the item kept. The first reason given stays the headline.
    pub fn keep(&mut self, reason: impl Into<String>) {
        if self.badge != Badge::Kept {
            self.badge = Badge::Kept;
            self.reason = reason.into();
        }
    }

    /// Kept for a reason that outranks whatever the rule found.
    pub fn force_keep(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.checks.insert(0, Check::fail(reason.clone()));
        self.badge = Badge::Kept;
        self.reason = reason;
    }

    pub fn dirty_worktree(&self) -> bool {
        matches!(self.plan, Plan::Worktree { dirty: true, .. })
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub id: usize,
    pub kind: Kind,
    pub path: PathBuf,
    pub label: String,
    pub target: Target,
    /// `None` until measured.
    pub verdict: Option<Verdict>,
}

impl Item {
    pub fn badge(&self) -> Option<Badge> {
        self.verdict.as_ref().map(|v| v.badge)
    }

    pub fn bytes(&self) -> Option<u64> {
        self.verdict.as_ref().and_then(|v| v.bytes)
    }

    pub fn selectable(&self) -> bool {
        matches!(self.badge(), Some(Badge::Safe | Badge::Review))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_ages_in_a_verdict_move_on_with_the_clock() {
        let now = SystemTime::now();
        let mut v = Verdict::new(Badge::Kept, "in use: written 3 min ago");
        v.checks.push(Check::fail("written 3 min ago: deep/new.txt"));
        v.last_write = Some(now - Duration::from_secs(180));
        v.as_of = Some(now);
        let later = v.aged(now + Duration::from_secs(3600));
        assert_eq!(later.reason, "in use: written 1 h ago");
        assert_eq!(later.checks[0].text, "written 1 h ago: deep/new.txt");
        assert!(v.same(&later), "the same verdict an hour on");
        assert!(!v.same(&Verdict { badge: Badge::Safe, ..later }));

        let mut idle = Verdict::new(Badge::Safe, "idle 10 days");
        idle.last_write = Some(now - Duration::from_secs(10 * 86_400 + 60));
        idle.as_of = Some(now);
        assert_eq!(idle.aged(now + Duration::from_secs(86_400)).reason, "idle 11 days");
        let untimed = Verdict::new(Badge::Safe, "idle 10 days");
        assert_eq!(untimed.aged(now).reason, "idle 10 days", "nothing to go on");
    }
}

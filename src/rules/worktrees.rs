//! Rule 1: linked git worktrees of the repositories under ~/src.
//!
//! Activity is judged by the files in the worktree, never by the index (git
//! status rewrites it) or logs/HEAD (gc rewrites them all at once).

use super::{Sink, gone, idle_for, no_mounts, not_in_use, readable, rel, run_tool, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Plan, Target, Verdict, WorktreeInfo};
use crate::walk::{self, Opts};
use crate::watch::Change;
use anyhow::{Result, bail};
use rayon::prelude::*;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_DEPTH: usize = 3;

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let repos = find_repos(&ctx.src_root);
    if repos.is_empty() {
        sink.note(Kind::Worktree, format!("no git repositories under {}", ctx.tilde(&ctx.src_root)));
        return;
    }
    let lists: Vec<(PathBuf, Vec<Listed>)> = repos
        .par_iter()
        .filter_map(|r| {
            let out = Git::at(r).out_raw(&["worktree", "list", "--porcelain", "-z"]).ok()?;
            Some((r.clone(), parse_porcelain(&out)))
        })
        .collect();
    let all: Vec<PathBuf> = lists.iter().flat_map(|(_, l)| l.iter().map(|w| w.path.clone())).collect();
    for (main, worktrees) in lists {
        // The first entry is always the main checkout, which stays.
        for w in worktrees.into_iter().skip(1) {
            if w.prunable || w.bare {
                continue;
            }
            let nested = all.iter().filter(|p| **p != w.path && p.starts_with(&w.path)).cloned().collect();
            let label = match w.path.strip_prefix(&ctx.src_root) {
                Ok(r) => r.display().to_string(),
                Err(_) => ctx.tilde(&w.path),
            };
            let info = WorktreeInfo { main: main.clone(), head: w.head, branch: w.branch, locked: w.locked, nested };
            sink.item(Kind::Worktree, w.path, label, Target::Worktree(info));
        }
    }
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![ctx.src_root.clone()]
}

/// Worktrees come and go, and are locked, in a repository's .git/worktrees;
/// new repositories turn up in the folders above the checkouts.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    let Some(dir) = c.path.parent() else { return false };
    if dir.ends_with(".git/worktrees") {
        return c.entries;
    }
    if dir.parent().is_some_and(|d| d.ends_with(".git/worktrees")) {
        return c.path.file_name().is_some_and(|n| n == "locked");
    }
    let Ok(inner) = dir.strip_prefix(&ctx.src_root) else { return false };
    c.entries
        && c.dir
        && inner.components().count() < MAX_DEPTH
        && !dir.ancestors().take_while(|a| *a != ctx.src_root.as_path()).any(|a| a.join(".git").exists())
}

/// Main checkouts under `root`, plus the repositories that own any linked
/// worktree found there.
pub fn find_repos(root: &Path) -> Vec<PathBuf> {
    let mut repos = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            if !e.file_type().is_ok_and(|t| t.is_dir()) || e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            let p = e.path();
            match std::fs::symlink_metadata(p.join(".git")) {
                Ok(m) if m.is_dir() => {
                    repos.insert(p);
                }
                Ok(m) if m.is_file() => {
                    if let Some(main) = main_of_linked(&p.join(".git")) {
                        repos.insert(main);
                    }
                }
                _ if depth + 1 < MAX_DEPTH => stack.push((p, depth + 1)),
                _ => {}
            }
        }
    }
    repos.into_iter().collect()
}

/// `gitdir: <main>/.git/worktrees/<name>` names the main checkout; a
/// submodule's `.git` file points into `modules/` instead.
fn main_of_linked(git_file: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(git_file).ok()?;
    let gitdir = PathBuf::from(text.trim().strip_prefix("gitdir:")?.trim());
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let dot_git = worktrees.parent()?;
    if dot_git.file_name()? != ".git" {
        return None;
    }
    dot_git.parent().map(Path::to_path_buf)
}

#[derive(Debug, Default, PartialEq)]
struct Listed {
    path: PathBuf,
    head: String,
    branch: Option<String>,
    locked: Option<String>,
    prunable: bool,
    bare: bool,
}

fn parse_porcelain(out: &str) -> Vec<Listed> {
    let mut list = Vec::new();
    let mut cur: Option<Listed> = None;
    for field in out.split('\0') {
        if field.is_empty() {
            list.extend(cur.take());
            continue;
        }
        let (key, val) = field.split_once(' ').unwrap_or((field, ""));
        if key == "worktree" {
            list.extend(cur.take());
            cur = Some(Listed { path: PathBuf::from(val), ..Listed::default() });
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        match key {
            "HEAD" => c.head = val.to_string(),
            "branch" => c.branch = Some(val.strip_prefix("refs/heads/").unwrap_or(val).to_string()),
            "locked" => c.locked = Some(val.to_string()),
            "prunable" => c.prunable = true,
            "bare" => c.bare = true,
            _ => {}
        }
    }
    list.extend(cur);
    list
}

pub fn evaluate(ctx: &Ctx, path: &Path, info: &WorktreeInfo) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let t = &ctx.cfg.thresholds;
    let mut v = Verdict::new(Badge::Safe, "");
    let stats =
        walk::measure(path, &Opts { exclude: &info.nested, split: Some(OsStr::new("build")), ..ctx.walk_opts() });
    sized(&mut v, &stats);

    if let Some(reason) = &info.locked {
        v.checks.push(Check::fail("locked with git worktree lock"));
        v.keep(if reason.is_empty() { "locked".to_string() } else { format!("locked: {reason}") });
    }
    if let Some(n) = info.nested.first() {
        v.checks.push(Check::fail(format!("contains the worktree {}", ctx.tilde(n))));
        v.keep("contains another worktree");
    }

    let git = Git::at(path);
    let changes = match git.status() {
        Ok(s) => {
            v.checks.push(if s.is_empty() { Check::pass("nothing uncommitted") } else { Check::fail(describe(&s)) });
            s.len()
        }
        Err(e) => {
            v.checks.push(Check::fail(format!("git status failed: {e}")));
            v.keep("git status failed");
            0
        }
    };

    match &stats.newest_outside {
        Some(n) if ctx.age_days(n.time) < t.worktree_edit_days => {
            let ago = ctx.ago(n.time);
            v.checks.push(Check::fail(format!("edited {ago}: {}", rel(path, &n.path))));
            v.keep(format!("in use: edited {ago}"));
        }
        Some(n) => v.checks.push(Check::pass(format!(
            "no file outside build/ edited for {} (newest: {})",
            crate::fmt::age(ctx.now.duration_since(n.time).unwrap_or_default()),
            rel(path, &n.path)
        ))),
        None => v.checks.push(Check::pass("no files outside build/")),
    }
    match &stats.newest_split {
        Some(n) if ctx.age_days(n.time) < t.worktree_build_days => {
            let ago = ctx.ago(n.time);
            v.checks.push(Check::fail(format!("built {ago}: {}", rel(path, &n.path))));
            v.keep(format!("in use: built {ago}"));
        }
        Some(n) => v.checks.push(Check::pass(format!(
            "nothing built for {}",
            crate::fmt::age(ctx.now.duration_since(n.time).unwrap_or_default())
        ))),
        None => v.checks.push(Check::info("no build/ folder")),
    }
    readable(&mut v, &stats);
    no_mounts(&mut v, &stats);
    not_in_use(ctx, &mut v, path);

    let head = git.out(&["rev-parse", "--verify", "-q", "HEAD"]).ok().filter(|h| !h.is_empty());
    let branch = git.out(&["symbolic-ref", "-q", "--short", "HEAD"]).ok().filter(|b| !b.is_empty());
    let backup_ref = backup_name(&git, path);
    let mut branch_head = false;
    match (&branch, &head) {
        (Some(b), _) => v.checks.push(Check::pass(format!("branch {b} keeps the commits"))),
        (None, Some(h)) => {
            let short = &h[..h.len().min(8)];
            if let Some(r) = git.containing_ref(h) {
                v.checks.push(Check::pass(format!("detached at {short}, which {r} contains")));
            } else {
                // merge-tree writes tree objects, so it only runs when its answer matters.
                let default = default_branch(&git);
                match default.as_deref().and_then(|d| content_in(&git, d, h).map(|m| (d, m))) {
                    Some((d, true)) => {
                        v.checks.push(Check::pass(format!("detached at {short}; its content is already in {d}")));
                    }
                    found => {
                        branch_head = true;
                        if let Some((d, false)) = found {
                            v.checks.push(Check::info(format!("its content isn't in {d}")));
                        }
                        v.checks
                            .push(Check::pass(format!("detached at {short} on no branch: kept on {backup_ref} first")));
                    }
                }
            }
        }
        (None, None) => {
            v.checks.push(Check::fail("HEAD has no commit"));
            v.keep("HEAD has no commit");
        }
    }

    let dirty = changes > 0;
    if v.badge != Badge::Kept {
        let idle = idle_for(ctx, &stats);
        if dirty {
            v.badge = Badge::Review;
            v.reason =
                format!("{}, idle {idle}", crate::fmt::count(changes, "uncommitted change", "uncommitted changes"));
        } else if branch_head {
            v.reason = format!("clean, idle {idle}; HEAD goes on {backup_ref}");
        } else {
            v.reason = format!("clean, idle {idle}");
        }
    }
    v.plan = Plan::Worktree { head: head.unwrap_or_default(), dirty, branch_head, backup_ref };
    v
}

fn describe(status: &[String]) -> String {
    let untracked = status.iter().filter(|s| s.starts_with("??")).count();
    let changed = status.len() - untracked;
    let mut parts = Vec::new();
    if changed > 0 {
        parts.push(format!("{changed} changed"));
    }
    if untracked > 0 {
        parts.push(format!("{untracked} untracked"));
    }
    format!("uncommitted work: {}", parts.join(", "))
}

pub fn delete(path: &Path, info: &WorktreeInfo, v: &Verdict, opts: super::DeleteOpts) -> Result<String> {
    let Plan::Worktree { head, dirty, branch_head, backup_ref } = &v.plan else { bail!("no worktree plan") };
    let git = Git::at(path);
    let mut notes = Vec::new();
    if *dirty && opts.backup {
        let commit = snapshot(&git, path, head)?;
        git.create_branch(backup_ref, &commit, path)?;
        notes.push(format!("uncommitted work saved on {backup_ref}"));
    } else if *branch_head {
        git.create_branch(backup_ref, head, path)?;
        notes.push(format!("detached HEAD kept on {backup_ref}"));
    }
    if *dirty && !opts.backup {
        notes.push("uncommitted work discarded".into());
    }
    let mut rm = Command::new(git_bin());
    rm.arg("-C").arg(&info.main).args(["worktree", "remove", "--force"]).arg(path).stdin(Stdio::null());
    run_tool(&mut rm)?;
    if std::fs::symlink_metadata(path).is_ok() {
        bail!("git worktree remove left {} behind", path.display());
    }
    Ok(notes.join("; "))
}

/// Commits the worktree as it stands, untracked files included, without
/// touching its index or running hooks: a scratch index, `write-tree`, then
/// `commit-tree` on top of HEAD.
fn snapshot(git: &Git, path: &Path, head: &str) -> Result<String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let index = git.out(&["rev-parse", "--git-path", "index"])?;
    let index = if Path::new(&index).is_absolute() { PathBuf::from(index) } else { path.join(index) };
    let tmp =
        std::env::temp_dir().join(format!("sweep-index-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    if index.exists() {
        std::fs::copy(&index, &tmp)?;
    }
    let result = (|| {
        run_tool(git.cmd(&["add", "-A"]).env("GIT_INDEX_FILE", &tmp))?;
        let tree = run_tool(git.cmd(&["write-tree"]).env("GIT_INDEX_FILE", &tmp))?.trim().to_string();
        let msg = format!("sweep: uncommitted work in {} before removing the worktree", path.display());
        let mut args = vec!["commit-tree", "--no-gpg-sign", tree.as_str(), "-m", msg.as_str()];
        if !head.is_empty() {
            args.extend(["-p", head]);
        }
        let mut cmd = git.cmd(&args);
        if git.out(&["config", "user.email"]).unwrap_or_default().is_empty() {
            for k in ["GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME"] {
                cmd.env(k, "sweep");
            }
            for k in ["GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL"] {
                cmd.env(k, "sweep@localhost");
            }
        }
        Ok(run_tool(&mut cmd)?.trim().to_string())
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// `backup/worktree-<folder>`, with a number added if that branch exists.
fn backup_name(git: &Git, path: &Path) -> String {
    let raw = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut name: String =
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect();
    while name.contains("..") {
        name = name.replace("..", ".");
    }
    let name = name.trim_matches(|c| c == '.' || c == '-').trim_end_matches(".lock").to_string();
    let base = format!("backup/worktree-{}", if name.is_empty() { "unnamed" } else { &name });
    let taken = |b: &str| git.ok(&["show-ref", "--verify", "--quiet", &format!("refs/heads/{b}")]);
    if !taken(&base) {
        return base;
    }
    (2..1000).map(|n| format!("{base}-{n}")).find(|b| !taken(b)).unwrap_or(base)
}

fn default_branch(git: &Git) -> Option<String> {
    if let Ok(s) = git.out(&["rev-parse", "--abbrev-ref", "origin/HEAD"])
        && !s.is_empty()
        && s != "origin/HEAD"
    {
        return Some(s);
    }
    ["origin/main", "origin/master"]
        .into_iter()
        .find(|b| git.ok(&["rev-parse", "--verify", "-q", &format!("refs/remotes/{b}")]))
        .map(String::from)
}

/// Whether merging `head` into `base` would change nothing: `git merge-tree
/// --write-tree base head` gives back base's own tree when head's content is
/// already there, as after a squash merge.
fn content_in(git: &Git, base: &str, head: &str) -> Option<bool> {
    let tree = git.out(&["rev-parse", &format!("{base}^{{tree}}")]).ok()?;
    let out = git.cmd(&["merge-tree", "--write-tree", base, head]).stderr(Stdio::null()).output().ok()?;
    match out.status.code() {
        Some(0) => Some(String::from_utf8_lossy(&out.stdout).lines().next() == Some(tree.as_str())),
        Some(1) => Some(false),
        _ => None,
    }
}

pub fn git_bin() -> &'static Path {
    static GIT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    GIT.get_or_init(|| {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        super::which("git", &home).unwrap_or_else(|| PathBuf::from("git"))
    })
}

pub struct Git {
    dir: PathBuf,
}

impl Git {
    pub fn at(dir: &Path) -> Git {
        Git { dir: dir.to_path_buf() }
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(git_bin());
        c.arg("-C")
            .arg(&self.dir)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .stdin(Stdio::null());
        c
    }

    fn out_raw(&self, args: &[&str]) -> Result<String> {
        run_tool(&mut self.cmd(args))
    }

    pub fn out(&self, args: &[&str]) -> Result<String> {
        Ok(self.out_raw(args)?.trim().to_string())
    }

    pub fn ok(&self, args: &[&str]) -> bool {
        self.cmd(args).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }

    /// One entry per uncommitted change, untracked files included.
    fn status(&self) -> Result<Vec<String>> {
        let out =
            self.out_raw(&["status", "--porcelain=v1", "-z", "--untracked-files=normal", "--ignore-submodules=none"])?;
        let mut entries = Vec::new();
        let mut skip = false;
        for rec in out.split('\0').filter(|r| !r.is_empty()) {
            if std::mem::take(&mut skip) {
                continue;
            }
            if rec.starts_with('R') || rec.starts_with('C') {
                skip = true; // a rename's source path follows
            }
            entries.push(rec.to_string());
        }
        Ok(entries)
    }

    fn containing_ref(&self, commit: &str) -> Option<String> {
        let out = self
            .out(&[
                "for-each-ref",
                "--count=1",
                "--format=%(refname:short)",
                &format!("--contains={commit}"),
                "refs/heads",
                "refs/remotes",
                "refs/tags",
            ])
            .ok()?;
        out.lines().next().filter(|l| !l.is_empty()).map(String::from)
    }

    fn create_branch(&self, name: &str, commit: &str, path: &Path) -> Result<()> {
        let msg = format!("sweep: before removing the worktree {}", path.display());
        run_tool(&mut self.cmd(&["update-ref", "-m", &msg, &format!("refs/heads/{name}"), commit, ""]))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Badge, Item, Mark};
    use crate::procs::{Proc, Procs};
    use crate::rules::testing::{badge, find, scan};
    use crate::rules::{DeleteOpts, evaluate};
    use crate::testutil::{run, set_mtime, write};
    use std::time::{Duration, SystemTime};

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        repo: PathBuf,
        now: SystemTime,
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let mut a = vec!["git"];
        a.extend(args);
        run(dir, &a)
    }

    /// A repository with an origin and a worktree in each state. The clock is
    /// moved 30 days ahead so everything written now counts as old, and
    /// "recent" files are dated just before that.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let src = home.join("src");
        let origin = home.join("origin.git");
        let repo = src.join("repo");
        std::fs::create_dir_all(&src).unwrap();
        git(&home, &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()]);
        git(&src, &["init", "-q", "-b", "main", "repo"]);
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        write(&repo.join("README.md"), "hello\n");
        write(&repo.join(".gitignore"), "build/\n");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        git(&repo, &["push", "-q", "origin", "HEAD:main"]);
        git(&repo, &["remote", "set-head", "origin", "main"]);

        // `git worktree add [options] <path> [<commit>]`
        let wt = |name: &str, args: &[&str]| {
            let p = src.join(name);
            let mut a = vec!["worktree", "add", "-q"];
            let (opts, commit) = match args {
                ["--detach", c] => (&args[..1], Some(*c)),
                _ => (args, None),
            };
            a.extend(opts);
            a.push(p.to_str().unwrap());
            a.extend(commit);
            git(&repo, &a);
            p
        };
        let clean = wt("repo-clean", &["-b", "feature-clean"]);
        write(&clean.join("feature.txt"), "feature\n");
        git(&clean, &["add", "-A"]);
        git(&clean, &["commit", "-q", "-m", "feature"]);
        write(&clean.join("build/out.o"), "object");

        let dirty = wt("repo-dirty", &["-b", "feature-dirty"]);
        write(&dirty.join("README.md"), "hello, edited\n");
        write(&dirty.join("notes.txt"), "untracked notes\n");

        wt("repo-det-on-main", &["--detach", "main"]);

        let orphan = wt("repo-det-orphan", &["--detach", "main"]);
        write(&orphan.join("experiment.txt"), "only here\n");
        git(&orphan, &["add", "-A"]);
        git(&orphan, &["commit", "-q", "-m", "experiment"]);

        let squashed = wt("repo-det-squashed", &["--detach", "main"]);
        write(&squashed.join("landed.txt"), "landed\n");
        git(&squashed, &["add", "-A"]);
        git(&squashed, &["commit", "-q", "-m", "landed, original"]);
        write(&repo.join("landed.txt"), "landed\n");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", "landed (squashed)"]);
        git(&repo, &["push", "-q", "origin", "HEAD:main"]);

        wt("repo-edited", &["-b", "feature-edited"]);
        let built = wt("repo-built", &["-b", "feature-built"]);
        write(&built.join("build/app/binary"), "bin");
        wt("repo-locked", &["-b", "feature-locked"]);
        git(&repo, &["worktree", "lock", "--reason", "release work", src.join("repo-locked").to_str().unwrap()]);
        wt("repo-busy", &["-b", "feature-busy"]);
        wt("repo-config-kept", &["-b", "feature-config"]);
        let outer = wt("repo-outer", &["-b", "feature-outer"]);
        let inner = outer.join("build/wt-inner");
        git(&repo, &["worktree", "add", "-q", "-b", "feature-inner", inner.to_str().unwrap()]);

        let now = SystemTime::now() + Duration::from_secs(30 * 86_400);
        set_mtime(&src.join("repo-edited/README.md"), now - Duration::from_secs(3600));
        set_mtime(&built.join("build/app/binary"), now - Duration::from_secs(1800));
        Fixture { _dir: dir, home, repo, now }
    }

    fn ctx(f: &Fixture) -> Ctx {
        let busy = f.home.join("src/repo-busy");
        let procs = Procs::from_list(vec![Proc {
            pid: 4242,
            cmd: format!("node {}/server.js", busy.display()),
            exe: Some("/usr/local/bin/node".into()),
            cwd: None,
        }]);
        let mut cfg = crate::config::Config { scratch: Vec::new(), ..Default::default() };
        cfg.keep = vec![crate::config::Keep::Path("~/src/repo-config-kept".into())];
        Ctx::new(f.home.clone(), f.home.join("T"), f.now, cfg, procs, "25G83".into())
    }

    fn plan(i: &Item) -> (bool, bool, String) {
        match &i.verdict.as_ref().unwrap().plan {
            Plan::Worktree { dirty, branch_head, backup_ref, .. } => (*dirty, *branch_head, backup_ref.clone()),
            p => panic!("unexpected plan {p:?}"),
        }
    }

    #[test]
    fn worktrees_in_each_state() {
        let f = fixture();
        let ctx = ctx(&f);
        let (items, _) = scan(&ctx, discover);
        let labels: BTreeSet<_> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(!labels.contains("repo"), "the main checkout is never a candidate");

        assert_eq!(badge(&items, "repo-clean"), Badge::Safe);
        assert_eq!(plan(find(&items, "repo-clean")), (false, false, "backup/worktree-repo-clean".into()));
        let clean = find(&items, "repo-clean").verdict.clone().unwrap();
        assert!(clean.checks.iter().any(|c| c.text == "branch feature-clean keeps the commits"));
        assert!(clean.bytes.unwrap() > 0);
        let orphan = find(&items, "repo-det-orphan").verdict.clone().unwrap();
        assert!(orphan.checks.iter().any(|c| c.mark == Mark::Info && c.text == "its content isn't in origin/main"));

        assert_eq!(badge(&items, "repo-dirty"), Badge::Review);
        assert!(plan(find(&items, "repo-dirty")).0);
        assert!(find(&items, "repo-dirty").verdict.as_ref().unwrap().reason.starts_with("2 uncommitted changes"));

        assert_eq!(badge(&items, "repo-det-on-main"), Badge::Safe);
        assert!(!plan(find(&items, "repo-det-on-main")).1);

        assert_eq!(badge(&items, "repo-det-orphan"), Badge::Safe);
        assert!(plan(find(&items, "repo-det-orphan")).1, "an orphaned detached HEAD gets a branch first");

        assert_eq!(badge(&items, "repo-det-squashed"), Badge::Safe);
        let sq = find(&items, "repo-det-squashed");
        assert!(!plan(sq).1, "content already in origin/main needs no branch");
        assert!(sq.verdict.as_ref().unwrap().checks.iter().any(|c| c.text.contains("already in origin/main")));

        let edited = find(&items, "repo-edited").verdict.clone().unwrap();
        assert_eq!(edited.badge, Badge::Kept);
        assert!(edited.reason.starts_with("in use: edited 1 h ago"), "{}", edited.reason);

        let built = find(&items, "repo-built").verdict.clone().unwrap();
        assert_eq!(built.badge, Badge::Kept);
        assert!(built.reason.starts_with("in use: built 30 min ago"), "{}", built.reason);

        assert_eq!(find(&items, "repo-locked").verdict.as_ref().unwrap().reason, "locked: release work");
        assert!(find(&items, "repo-busy").verdict.as_ref().unwrap().reason.contains("node (pid 4242)"));
        assert_eq!(badge(&items, "repo-config-kept"), Badge::Kept);
        assert_eq!(find(&items, "repo-outer").verdict.as_ref().unwrap().reason, "contains another worktree");
        assert_eq!(badge(&items, "repo-outer/build/wt-inner"), Badge::Safe);
    }

    #[test]
    fn the_index_date_and_git_status_do_not_count_as_activity() {
        let f = fixture();
        let ctx = ctx(&f);
        let wt = f.home.join("src/repo-clean");
        // Running git status rewrites the worktree's index in .git; it isn't
        // inside the worktree and doesn't make it look busy.
        git(&wt, &["status"]);
        let index = PathBuf::from(git(&wt, &["rev-parse", "--git-path", "index"]));
        let index = if index.is_absolute() { index } else { wt.join(index) };
        set_mtime(&index, f.now);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(badge(&items, "repo-clean"), Badge::Safe);
    }

    fn item(ctx: &Ctx, label: &str) -> Item {
        let (items, _) = scan(ctx, discover);
        find(&items, label).clone()
    }

    fn remove(ctx: &Ctx, it: &Item, backup: bool) -> String {
        let v = evaluate(ctx, it);
        super::super::delete(ctx, it, &v, DeleteOpts { backup }).unwrap()
    }

    #[test]
    fn deleting_keeps_branches_and_saves_what_nothing_else_keeps() {
        let f = fixture();
        let ctx = ctx(&f);

        let clean = item(&ctx, "repo-clean");
        assert_eq!(remove(&ctx, &clean, true), "");
        assert!(!clean.path.exists());
        assert!(!git(&f.repo, &["branch", "--list", "feature-clean"]).is_empty(), "the branch stays");

        let orphan = item(&ctx, "repo-det-orphan");
        let orphan_head = git(&orphan.path, &["rev-parse", "HEAD"]);
        assert_eq!(remove(&ctx, &orphan, true), "detached HEAD kept on backup/worktree-repo-det-orphan");
        assert_eq!(git(&f.repo, &["rev-parse", "backup/worktree-repo-det-orphan"]), orphan_head);

        let dirty = item(&ctx, "repo-dirty");
        let dirty_head = git(&dirty.path, &["rev-parse", "HEAD"]);
        assert_eq!(remove(&ctx, &dirty, true), "uncommitted work saved on backup/worktree-repo-dirty");
        assert!(!dirty.path.exists());
        let b = "backup/worktree-repo-dirty";
        assert_eq!(git(&f.repo, &["rev-parse", &format!("{b}^")]), dirty_head);
        assert_eq!(git(&f.repo, &["show", &format!("{b}:README.md")]), "hello, edited");
        assert_eq!(git(&f.repo, &["show", &format!("{b}:notes.txt")]), "untracked notes");

        let listed = git(&f.repo, &["worktree", "list"]);
        assert!(
            !listed.contains("repo-clean") && !listed.contains("repo-dirty") && !listed.contains("repo-det-orphan")
        );
        assert!(f.repo.join("README.md").exists(), "the main checkout is untouched");
        assert_eq!(git(&f.repo, &["status", "--porcelain"]), "");
    }

    #[test]
    fn a_second_backup_gets_a_new_name() {
        let f = fixture();
        let ctx = ctx(&f);
        git(&f.repo, &["branch", "backup/worktree-repo-det-orphan", "main"]);
        let orphan = item(&ctx, "repo-det-orphan");
        assert_eq!(plan(&orphan).2, "backup/worktree-repo-det-orphan-2");
    }

    #[test]
    fn a_dirty_worktree_without_backup_is_discarded_on_request() {
        let f = fixture();
        let ctx = ctx(&f);
        let dirty = item(&ctx, "repo-dirty");
        assert_eq!(remove(&ctx, &dirty, false), "uncommitted work discarded");
        assert!(!dirty.path.exists());
        assert!(git(&f.repo, &["branch", "--list", "backup/*"]).is_empty());
    }

    #[test]
    fn porcelain_parsing() {
        let out = "worktree /r\0HEAD aaa\0branch refs/heads/main\0\0worktree /r-x\0HEAD bbb\0detached\0locked\0\0worktree /r-y\0HEAD ccc\0branch refs/heads/y\0prunable gitdir file points to non-existent location\0\0";
        let l = parse_porcelain(out);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].branch.as_deref(), Some("main"));
        assert_eq!(l[1].branch, None);
        assert_eq!(l[1].locked.as_deref(), Some(""));
        assert!(l[2].prunable);
    }

    #[test]
    fn listings_change_above_the_checkouts_and_in_git_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let src = home.join("src");
        std::fs::create_dir_all(src.join("repo/.git/worktrees/wt1")).unwrap();
        std::fs::create_dir_all(src.join("repo/build/deep")).unwrap();
        std::fs::create_dir_all(src.join("team/proj/deep")).unwrap();
        let ctx = crate::ctx::test_ctx(&home, SystemTime::now());
        let made = |p: PathBuf| Change { path: p, dir: true, entries: true, ..Change::default() };
        let written = |p: PathBuf| Change { path: p, ..Change::default() };
        assert!(lists(&ctx, &made(src.join("new-clone"))));
        assert!(lists(&ctx, &made(src.join("team/new-clone"))), "in a folder of repositories");
        assert!(lists(&ctx, &made(src.join("repo/.git/worktrees/wt2"))), "a worktree added");
        assert!(lists(&ctx, &written(src.join("repo/.git/worktrees/wt1/locked"))), "a worktree locked");
        assert!(!lists(&ctx, &written(src.join("repo/.git/worktrees/wt1/index"))), "git status in a worktree");
        assert!(!lists(&ctx, &made(src.join("repo/src"))), "a folder made in a checkout");
        assert!(!lists(&ctx, &made(src.join("repo/build/out"))), "a build in a checkout");
        assert!(!lists(&ctx, &written(src.join("team/notes.txt"))), "a file saved beside the repositories");
        assert!(!lists(&ctx, &made(src.join("team/proj/deep/more"))), "deeper than repositories are looked for");
        assert!(!lists(&ctx, &made(home.join("Downloads/x"))));
    }
}

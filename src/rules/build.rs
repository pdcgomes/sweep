//! Rule 13: what builds and installs leave in the projects under the source
//! folder, once nobody has edited the project for two weeks. Output a build
//! makes again is Safe; installed dependencies, which need a download to
//! come back, are for review. Only folders git ignores count, and linked
//! worktrees are their own rule's.

use super::{Found, Sink, gone, idle, no_mounts, not_in_use, readable, rel, sized};
use crate::ctx::Ctx;
use crate::fsattr::{self, ObjKind};
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk::{self, Opts};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub struct Output {
    pub name: &'static str,
    /// Files one of which sits beside it in its project.
    beside: &'static [&'static str],
    /// A file inside that shows what it is: a virtualenv's pyvenv.cfg.
    inside: Option<&'static str>,
    /// Bringing it back needs a download.
    pub review: bool,
    /// How it comes back.
    pub again: &'static str,
}

const fn made(name: &'static str, beside: &'static [&'static str], again: &'static str) -> Output {
    Output { name, beside, inside: None, review: false, again }
}

pub const OUTPUTS: [Output; 12] = [
    made(".next", &["package.json"], "next build makes it again"),
    made(".nuxt", &["package.json"], "nuxt build makes it again"),
    made(".svelte-kit", &["package.json"], "the next build makes it again"),
    made(".turbo", &["package.json", "turbo.json"], "turbo makes it again"),
    made(".parcel-cache", &["package.json"], "parcel makes it again"),
    made("target", &["Cargo.toml"], "cargo build makes it again"),
    made(".build", &["Package.swift"], "swift build makes it again"),
    made(
        ".gradle",
        &["build.gradle", "build.gradle.kts", "settings.gradle", "settings.gradle.kts"],
        "gradle makes it again",
    ),
    Output {
        name: "node_modules",
        beside: &["package.json"],
        inside: None,
        review: true,
        again: "npm install brings it back, from the registry",
    },
    Output {
        name: ".venv",
        beside: &[],
        inside: Some("pyvenv.cfg"),
        review: true,
        again: "uv sync or pip install brings it back",
    },
    Output {
        name: "venv",
        beside: &[],
        inside: Some("pyvenv.cfg"),
        review: true,
        again: "uv sync or pip install brings it back",
    },
    Output { name: "Pods", beside: &["Podfile"], inside: None, review: true, again: "pod install brings it back" },
];

/// Left out when judging when a project was last edited: its output
/// wherever it is, git's own folder, and what Finder writes just from
/// looking at a folder.
pub const SKIP: &[&str] = &[
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    "target",
    ".build",
    ".gradle",
    "node_modules",
    ".venv",
    "venv",
    "Pods",
    ".git",
    ".DS_Store",
];

/// Projects deeper than this under the source folder aren't looked for.
const MAX_DEPTH: usize = 6;

/// Below this an output folder isn't worth a row.
const SMALL: u64 = 1_000_000;

pub fn output(name: &str) -> Option<&'static Output> {
    OUTPUTS.iter().find(|o| o.name == name)
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![ctx.src_root.clone()]
}

/// Whether `path`, a folder in `project`, is the output named after it.
fn shows(o: &Output, path: &Path, project: &Path) -> bool {
    o.beside.iter().any(|b| project.join(b).is_file()) || o.inside.is_some_and(|f| path.join(f).is_file())
}

/// A linked worktree, which the worktree rule measures whole.
fn worktree(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok_and(|m| m.is_file())
}

fn look(ctx: &Ctx, dir: &Path, depth: usize, found: &Mutex<Vec<(PathBuf, &'static Output)>>) {
    let Ok(entries) = fsattr::read_dir_quick(dir) else { return };
    let mut deeper = Vec::new();
    for e in entries.iter().filter(|e| e.attrs.kind == ObjKind::Dir) {
        let path = dir.join(&e.name);
        let name = e.name.to_string_lossy();
        if let Some(o) = output(&name) {
            if shows(o, &path, dir) {
                found.lock().unwrap().push((path, o));
            }
            continue;
        }
        if name.starts_with('.') || depth + 1 >= MAX_DEPTH || worktree(&path) || ctx.guard.check(&path).is_some() {
            continue;
        }
        deeper.push(path);
    }
    deeper.par_iter().for_each(|d| look(ctx, d, depth + 1, found));
}

/// The repository a path is in, if any.
fn repo(path: &Path) -> Option<&Path> {
    path.ancestors().skip(1).find(|a| a.join(".git").exists())
}

/// Of these folders in one repository, the ones git ignores. None if git
/// couldn't say.
fn ignored(root: &Path, dirs: &[&Path]) -> Option<HashSet<PathBuf>> {
    let mut child = Command::new("git")
        .args(["check-ignore", "--stdin", "-z"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut input = Vec::new();
    for d in dirs {
        input.extend_from_slice(d.as_os_str().as_encoded_bytes());
        input.extend_from_slice(b"/\0");
    }
    child.stdin.take()?.write_all(&input).ok()?;
    let out = child.wait_with_output().ok()?;
    // 0: some are ignored, 1: none are; anything else is a failure.
    if !matches!(out.status.code(), Some(0 | 1)) {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    Some(s.split('\0').filter(|p| !p.is_empty()).map(|p| PathBuf::from(p.trim_end_matches('/'))).collect())
}

fn found(ctx: &Ctx, path: PathBuf, o: &'static Output) -> Found {
    let project = path.parent().unwrap_or(&path).to_path_buf();
    let label = path.strip_prefix(&ctx.src_root).map(|r| r.display().to_string()).unwrap_or_else(|_| ctx.tilde(&path));
    (Kind::BuildOutput, path, label, Target::BuildOutput { project, output: o.name })
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let all = Mutex::new(Vec::new());
    look(ctx, &ctx.src_root, 0, &all);
    let mut by_repo: BTreeMap<Option<PathBuf>, Vec<(PathBuf, &'static Output)>> = BTreeMap::new();
    for (path, o) in all.into_inner().unwrap() {
        by_repo.entry(repo(&path).map(Path::to_path_buf)).or_default().push((path, o));
    }
    let mut unsure = 0;
    for (root, list) in by_repo {
        let keep: Vec<(PathBuf, &'static Output)> = match &root {
            // Outside any repository there's nothing to say it's committed.
            None => list,
            Some(r) => match ignored(r, &list.iter().map(|(p, _)| p.as_path()).collect::<Vec<_>>()) {
                Some(set) => list.into_iter().filter(|(p, _)| set.contains(p)).collect(),
                None => {
                    unsure += list.len();
                    Vec::new()
                }
            },
        };
        for (path, o) in keep {
            let (kind, path, label, target) = found(ctx, path, o);
            sink.item(kind, path, label, target);
        }
    }
    if unsure > 0 {
        sink.note(
            Kind::BuildOutput,
            format!(
                "left {} alone: git couldn't say whether it ignores them",
                crate::fmt::count(unsure, "folder", "folders")
            ),
        );
    }
}

/// The item a folder made under the source folder is, when it's output a
/// project's git ignores.
pub fn entry(ctx: &Ctx, path: &Path) -> Option<Found> {
    let o = output(&path.file_name()?.to_string_lossy())?;
    let project = path.parent()?;
    let inner = project.strip_prefix(&ctx.src_root).ok()?;
    if inner.components().count() >= MAX_DEPTH || !shows(o, path, project) || ctx.guard.check(path).is_some() {
        return None;
    }
    if project.ancestors().take_while(|a| a.starts_with(&ctx.src_root)).any(worktree) {
        return None;
    }
    if let Some(root) = repo(path) {
        ignored(root, &[path])?.contains(path).then_some(())?;
    }
    Some(found(ctx, path.to_path_buf(), o))
}

pub fn evaluate(ctx: &Ctx, path: &Path, project: &Path, name: &str) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let Some(o) = output(name) else { return Verdict::new(Badge::Kept, "not a kind of output sweep knows") };
    let days = ctx.cfg.thresholds.build_output_days;
    let mut v = Verdict::new(if o.review { Badge::Review } else { Badge::Safe }, "");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    // The project's own files, its output left out wherever it is.
    let p = walk::measure(project, &Opts { skip: SKIP, no_sizes: true, ..Default::default() });
    let shown = ctx.tilde(project);
    let mut edited = String::new();
    match &p.newest_file {
        Some(n) if ctx.age_days(n.time) < days => {
            let ago = ctx.ago(n.time);
            v.checks.push(Check::fail(format!("project edited {ago}: {}", rel(project, &n.path))));
            v.keep(format!("in use: project edited {ago}"));
            v.last_write = v.last_write.max(Some(n.time));
        }
        Some(n) => {
            edited = crate::fmt::age(ctx.now.duration_since(n.time).unwrap_or_default());
            v.checks.push(Check::pass(format!(
                "nothing in {shown} edited for {edited} (newest: {})",
                rel(project, &n.path)
            )));
        }
        None => v.checks.push(Check::pass(format!("no files in {shown} besides its output"))),
    }
    idle(ctx, &mut v, path, &s, days);
    not_in_use(ctx, &mut v, path);
    if let Some(p) = ctx.procs.using(project) {
        let who = p.describe();
        v.checks.push(Check::fail(format!("{who} works in the project")));
        v.keep(format!("in use by {who}"));
    }
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge != Badge::Kept {
        v.reason = if edited.is_empty() { o.again.to_string() } else { format!("{}; project idle {edited}", o.again) };
        if s.freed < SMALL {
            v.hidden = true;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{Proc, Procs};
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{run, set_mtime, write, write_random};
    use std::time::{Duration, SystemTime};

    #[test]
    fn every_output_is_left_out_when_judging_edits() {
        for o in &OUTPUTS {
            assert!(SKIP.contains(&o.name), "{}", o.name);
        }
    }

    #[test]
    fn output_of_idle_projects_that_git_ignores() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let src = home.join("src");
        // An idle Next.js app in a repository that ignores its output.
        let web = src.join("web");
        write(&web.join("package.json"), "{}");
        write(&web.join(".gitignore"), ".next/\nnode_modules/\n");
        write(&web.join("app/page.tsx"), "x");
        write_random(&web.join(".next/cache/a"), 2_000_000);
        write_random(&web.join("node_modules/react/index.js"), 2_000_000);
        run(&web, &["git", "init", "-q"]);
        // A Rust project someone edits, outside any repository.
        let busy = src.join("tools/busy");
        write(&busy.join("Cargo.toml"), "[package]");
        write(&busy.join("src/main.rs"), "fn main() {}");
        write_random(&busy.join("target/debug/busy"), 2_000_000);
        // Committed node_modules stay; so does a `target` that isn't Rust's.
        let vendored = src.join("action");
        write(&vendored.join("package.json"), "{}");
        write_random(&vendored.join("node_modules/dep/index.js"), 2_000_000);
        run(&vendored, &["git", "init", "-q"]);
        write_random(&src.join("notes/target/x"), 2_000_000);
        // A virtualenv, and output inside a linked worktree, which its own rule covers.
        write(&src.join("ml/.venv/pyvenv.cfg"), "home = /usr/bin");
        write_random(&src.join("ml/.venv/lib/x"), 2_000_000);
        write(&src.join("ml/train.py"), "x");
        write(&src.join("repo/wt/.git"), "gitdir: /elsewhere");
        write(&src.join("repo/wt/package.json"), "{}");
        write_random(&src.join("repo/wt/node_modules/x"), 2_000_000);

        let now = SystemTime::now() + Duration::from_secs(30 * 86_400);
        set_mtime(&busy.join("src/main.rs"), now - Duration::from_secs(3 * 86_400));
        // Finder looking at the folder yesterday isn't anyone working on it.
        write(&web.join(".DS_Store"), "x");
        set_mtime(&web.join(".DS_Store"), now - Duration::from_secs(86_400));
        let mut ctx = crate::ctx::test_ctx(&home, now);
        let (items, _) = scan(&ctx, discover);
        let mut labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        labels.sort();
        assert_eq!(labels, ["ml/.venv", "tools/busy/target", "web/.next", "web/node_modules"]);
        let next = find(&items, "web/.next").verdict.clone().unwrap();
        assert_eq!(
            (next.badge, next.reason.as_str()),
            (Badge::Safe, "next build makes it again; project idle 30 days")
        );
        assert_eq!(badge(&items, "web/node_modules"), Badge::Review);
        assert_eq!(badge(&items, "ml/.venv"), Badge::Review);
        let rust = find(&items, "tools/busy/target").verdict.clone().unwrap();
        assert_eq!((rust.badge, rust.reason.as_str()), (Badge::Kept, "in use: project edited 3 days ago"));

        // A dev server working in the project keeps its output.
        ctx.procs =
            Procs::from_list(vec![Proc { pid: 4, cmd: "node server.js".into(), exe: None, cwd: Some(web.clone()) }]);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(find(&items, "web/.next").verdict.as_ref().unwrap().reason, "in use by node (pid 4)");
    }
}

//! Rule 8: package managers' caches, for review: pip, uv, Homebrew, npm,
//! Yarn, SwiftPM, Playwright's browsers and node-gyp's headers. Each tool
//! downloads again what it needs, so a cache goes unless its tool is running.

use super::{Sink, gone, not_in_use, readable, run_tool, sized, which};
use crate::ctx::Ctx;
use crate::model::{Badge, CacheTool, Check, Kind, Target, Verdict};
use crate::procs::Proc;
use crate::walk;
use crate::watch::Change;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn locations(ctx: &Ctx) -> Vec<(CacheTool, PathBuf)> {
    let h = &ctx.home;
    vec![
        (CacheTool::Pip, env_dir("PIP_CACHE_DIR").unwrap_or_else(|| h.join("Library/Caches/pip"))),
        (CacheTool::Uv, env_dir("UV_CACHE_DIR").unwrap_or_else(|| h.join(".cache/uv"))),
        (CacheTool::Homebrew, env_dir("HOMEBREW_CACHE").unwrap_or_else(|| h.join("Library/Caches/Homebrew"))),
        (CacheTool::Npm, env_dir("npm_config_cache").unwrap_or_else(|| h.join(".npm"))),
        (CacheTool::Yarn, h.join("Library/Caches/Yarn")),
        (CacheTool::SwiftPm, h.join("Library/Caches/org.swift.swiftpm")),
        (CacheTool::SwiftPm, h.join("Library/org.swift.swiftpm")),
        (
            CacheTool::Playwright,
            env_dir("PLAYWRIGHT_BROWSERS_PATH").unwrap_or_else(|| h.join("Library/Caches/ms-playwright")),
        ),
        (CacheTool::NodeGyp, h.join("Library/Caches/node-gyp")),
    ]
}

fn name(tool: CacheTool) -> &'static str {
    match tool {
        CacheTool::Pip => "pip",
        CacheTool::Uv => "uv",
        CacheTool::Homebrew => "Homebrew",
        CacheTool::Npm => "npm",
        CacheTool::Yarn => "Yarn",
        CacheTool::SwiftPm => "SwiftPM",
        CacheTool::Playwright => "Playwright",
        CacheTool::NodeGyp => "node-gyp",
    }
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    locations(ctx).into_iter().map(|(_, p)| p).collect()
}

pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    c.entries && locations(ctx).iter().any(|(_, p)| *p == c.path)
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    for (tool, path) in locations(ctx) {
        if path.is_dir() {
            let label = format!("{} cache · {}", name(tool), ctx.tilde(&path));
            sink.item(Kind::PackageCache, path, label, Target::PackageCache(tool));
        }
    }
}

pub fn kept_without_measuring(ctx: &Ctx, tool: CacheTool) -> Option<String> {
    ctx.procs.find(|p| tool_running(tool, p)).map(|_| format!("{} is running", name(tool)))
}

fn tool_running(tool: CacheTool, p: &Proc) -> bool {
    let n = p.name();
    match tool {
        CacheTool::Pip => n.starts_with("pip") || p.cmd.contains(" -m pip"),
        CacheTool::Uv => n == "uv" || n == "uvx",
        CacheTool::Homebrew => n == "brew" || p.cmd.contains("/Library/Homebrew/brew.rb"),
        CacheTool::Npm => n == "npm" || n == "npx" || p.cmd.contains("npm-cli.js") || p.cmd.contains("npx-cli.js"),
        CacheTool::Yarn => n == "yarn" || p.cmd.contains("/yarn.js") || p.cmd.contains("/yarn.cjs"),
        CacheTool::SwiftPm => n == "swift-build" || n == "swift-package" || p.cmd.starts_with("swift build"),
        CacheTool::Playwright => p.cmd.contains("playwright"),
        CacheTool::NodeGyp => n == "node-gyp" || p.cmd.contains("node-gyp"),
    }
}

fn again(tool: CacheTool) -> String {
    match tool {
        CacheTool::Playwright => "Playwright downloads its browsers again on the next install".into(),
        CacheTool::NodeGyp => "node-gyp downloads Node's headers again on the next native build".into(),
        t => format!("{} downloads it again when needed", name(t)),
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, tool: CacheTool) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Review, again(tool));
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    match ctx.procs.find(|p| tool_running(tool, p)) {
        Some(p) => {
            v.checks.push(Check::fail(format!("{} is running: {}", name(tool), p.describe())));
            v.keep(format!("{} is running", name(tool)));
        }
        None => v.checks.push(Check::pass(format!("{} isn't running", name(tool)))),
    }
    not_in_use(ctx, &mut v, path);
    readable(&mut v, &s);
    v
}

pub fn delete(path: &Path, tool: CacheTool) -> Result<String> {
    match tool {
        CacheTool::Uv => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            if let Some(uv) = which("uv", &home) {
                run_tool(Command::new(uv).args(["cache", "clean", "--cache-dir"]).arg(path))?;
                return Ok("uv cache clean".into());
            }
            crate::rmtree::remove(path)?;
        }
        CacheTool::Homebrew => crate::rmtree::remove_contents(path)?,
        _ => crate::rmtree::remove(path)?,
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Keep;
    use crate::procs::Procs;
    use crate::rules::testing::{find, scan};
    use crate::testutil::write_random;
    use std::time::SystemTime;

    #[test]
    fn package_caches_for_review_unless_their_tool_runs_or_they_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        for p in [
            "Library/Caches/pip/http/a",
            ".cache/uv/wheels/b",
            "Library/Caches/Homebrew/downloads/c",
            ".npm/_cacache/d",
            "Library/Caches/Yarn/e",
            "Library/Caches/ms-playwright/f",
        ] {
            write_random(&home.join(p), 10_000);
        }
        let mut ctx = crate::ctx::test_ctx(&home, SystemTime::now());
        ctx.cfg.keep = vec![Keep::Why { path: "~/Library/Caches/Yarn".into(), why: "offline installs need it".into() }];
        ctx = ctx.at(
            ctx.now,
            Procs::from_list(vec![Proc {
                pid: 3,
                cmd: "uv run server.py".into(),
                exe: Some("/Users/u/.local/bin/uv".into()),
                cwd: None,
            }]),
        );
        let (items, _) = scan(&ctx, discover);
        let by = |l: &str| find(&items, l).verdict.clone().unwrap();
        assert_eq!(by("pip cache · ~/Library/Caches/pip").badge, Badge::Review);
        assert_eq!(by("Homebrew cache · ~/Library/Caches/Homebrew").badge, Badge::Review);
        assert_eq!(by("npm cache · ~/.npm").reason, "npm downloads it again when needed");
        assert_eq!(by("uv cache · ~/.cache/uv").reason, "uv is running");
        assert_eq!(by("Yarn cache · ~/Library/Caches/Yarn").reason, "always kept: offline installs need it");
        assert_eq!(by("Playwright cache · ~/Library/Caches/ms-playwright").badge, Badge::Review);
        assert!(by("pip cache · ~/Library/Caches/pip").bytes.unwrap() >= 10_000);
    }
}

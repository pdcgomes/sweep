mod agent;
mod config;
mod ctx;
mod delete;
mod demo;
mod disks;
mod fmt;
mod fsattr;
mod glob;
mod live;
mod model;
mod procs;
mod report;
mod rmtree;
mod rules;
mod scan;
#[cfg(test)]
mod testutil;
mod theme;
mod tui;
mod walk;
mod watch;

use anyhow::{Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use ctx::Ctx;
use delete::{Job, Outcome, Progress};
use model::{Badge, Item, Kind};
use rules::DeleteOpts;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use theme::hex;

#[derive(Parser)]
#[command(
    name = "sweep",
    version,
    about = "Finds junk on this Mac, shows what deleting each item would really free, and deletes what you select."
)]
struct Cli {
    /// Go through the motions, checks included, without deleting anything.
    #[arg(long, global = true)]
    dry_run: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List what sweep finds, without the interface.
    Scan {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
        /// Every item, not just the largest few of each group.
        #[arg(long)]
        all: bool,
        /// Also measure items kept for reasons known up front (keep lists, kept
        /// caches, running apps). Slower; their size doesn't change what's deletable.
        #[arg(long)]
        kept_sizes: bool,
    },
    /// Delete without the interface, for scripts. Needs --yes, or --dry-run to only report.
    Clean {
        /// Every Safe item.
        #[arg(long)]
        safe: bool,
        /// Only these kinds: worktree, build-output, temp, app-cache, derived-data, xcode-support,
        /// core-ml, sim-device, sim-runtime, docker, logs, package-cache, extension, model,
        /// scratch, downloads, trash, snapshot.
        #[arg(long = "kind", value_name = "KIND")]
        kinds: Vec<String>,
        /// Also these listed items, by path (Safe or Review).
        #[arg(long = "path", value_name = "PATH")]
        paths: Vec<PathBuf>,
        /// Discard dirty worktrees' uncommitted work instead of saving it on a backup branch.
        #[arg(long)]
        no_backup: bool,
        /// Really delete.
        #[arg(long)]
        yes: bool,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Scan and post a notification with the total found. The weekly agent runs this; it never deletes.
    Notify,
    /// The weekly launchd agent.
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Show where the settings live and what they say.
    Config,
    /// Render one frame of the interface as text.
    #[command(hide = true)]
    Snapshot {
        #[arg(long, default_value_t = 140)]
        width: u16,
        #[arg(long, default_value_t = 45)]
        height: u16,
        #[arg(long)]
        select_safe: bool,
        #[arg(long)]
        confirm: bool,
        #[arg(long)]
        expand: bool,
    },
    /// The interface on a made-up Mac, for screenshots. Deleting only pretends.
    #[command(hide = true)]
    Demo,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Scan weekly and post a notification with the total found.
    Install {
        /// 0 is Sunday, 1 Monday … 6 Saturday.
        #[arg(long, default_value_t = 1)]
        weekday: u8,
        #[arg(long, default_value_t = 10)]
        hour: u8,
    },
    Uninstall,
    Status,
}

fn main() -> ExitCode {
    let matches = Cli::command().styles(theme::help_styles()).get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => e.exit(),
    };
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sweep: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn run(cli: Cli) -> Result<ExitCode> {
    let home = home();
    let (cfg, cfg_path, created) = config::load_or_create(&home)?;
    match cli.cmd {
        None => {
            let ctx = Arc::new(Ctx::real(cfg));
            let hello = created.then(|| format!("wrote default settings to {}", ctx.tilde(&cfg_path)));
            tui::run(ctx, scan::pool(), cli.dry_run, hello)?;
        }
        Some(Cmd::Scan { json, all, kept_sizes }) => {
            let ctx = Ctx::real(cfg);
            let survey = std::thread::spawn(disks::survey);
            let (items, notes) = scan::collect(&ctx, &scan::pool(), kept_sizes);
            let disks = survey.join().ok().and_then(Result::ok).unwrap_or_default();
            if json {
                println!("{}", report::json(&ctx, &items, &notes, &disks));
            } else {
                print!("{}", report::text(&ctx, &items, &notes, all, Some(&disks), theme::Paint::stdout()));
            }
        }
        Some(Cmd::Clean { safe, kinds, paths, no_backup, yes, json }) => {
            return clean(Ctx::real(cfg), safe, &kinds, &paths, no_backup, yes, json, cli.dry_run);
        }
        Some(Cmd::Notify) => {
            let ctx = Ctx::real(cfg);
            let survey = std::thread::spawn(disks::survey);
            let (items, _) = scan::collect(&ctx, &scan::pool(), false);
            let disks = survey.join().ok().and_then(Result::ok).unwrap_or_default();
            let msg = agent::message(&items, &disks);
            println!("{}  {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M"));
            agent::notify(&msg)?;
        }
        Some(Cmd::Agent { action }) => {
            let ctx = Ctx::new(
                home.clone(),
                PathBuf::new(),
                std::time::SystemTime::now(),
                cfg,
                Default::default(),
                String::new(),
            );
            match action {
                AgentCmd::Install { weekday, hour } => {
                    if weekday > 6 || hour > 23 {
                        bail!("--weekday takes 0–6 and --hour 0–23");
                    }
                    agent::install(&ctx, weekday, hour)?;
                }
                AgentCmd::Uninstall => agent::uninstall(&ctx)?,
                AgentCmd::Status => agent::status(&ctx),
            }
        }
        Some(Cmd::Config) => {
            println!("{}{}", cfg_path.display(), if created { " (just written with the defaults)" } else { "" });
            println!();
            print!("{}", std::fs::read_to_string(&cfg_path).unwrap_or_else(|_| config::TEMPLATE.to_string()));
        }
        Some(Cmd::Snapshot { width, height, select_safe, confirm, expand }) => {
            let ctx = Ctx::real(cfg);
            let survey = std::thread::spawn(disks::survey);
            let (items, notes) = scan::collect(&ctx, &scan::pool(), false);
            let mut app = tui::App::with_items(items, notes, cli.dry_run, ctx.cfg.thresholds.clone());
            app.set_disks(
                survey.join().unwrap_or_else(|_| Err(anyhow::anyhow!("survey panicked"))).map_err(|e| format!("{e:#}")),
            );
            if select_safe {
                app.select_safe_for_snapshot();
            }
            if expand {
                app.expand_all();
            }
            print!("{}", tui::snapshot(&mut app, width, height, confirm));
        }
        Some(Cmd::Demo) => {
            let thresholds = config::Config::default().thresholds;
            let ctx = Ctx::new(
                PathBuf::from("/Users/alex"),
                PathBuf::new(),
                std::time::SystemTime::now(),
                config::Config::default(),
                Default::default(),
                String::new(),
            );
            tui::run_demo(demo::app(thresholds), Arc::new(ctx))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Serialize)]
struct CleanItem {
    kind: Kind,
    path: String,
    outcome: &'static str,
    bytes: u64,
    detail: String,
}

#[allow(clippy::too_many_arguments)]
fn clean(
    ctx: Ctx,
    safe: bool,
    kinds: &[String],
    paths: &[PathBuf],
    no_backup: bool,
    yes: bool,
    json: bool,
    dry_run: bool,
) -> Result<ExitCode> {
    if !yes && !dry_run {
        bail!("sweep clean deletes only with --yes; add --dry-run to see what it would do");
    }
    if !safe && paths.is_empty() {
        bail!("say what to delete: --safe for every Safe item, and/or --path for particular ones");
    }
    let mut want = Vec::new();
    for k in kinds {
        match Kind::from_slug(k) {
            Some(kind) => want.push(kind),
            None => bail!("unknown kind {k:?}"),
        }
    }
    let pool = scan::pool();
    let (items, _) = scan::collect(&ctx, &pool, false);
    let listed = |i: &Item| !i.verdict.as_ref().is_some_and(|v| v.hidden);
    let chosen: Vec<&Item> = items
        .iter()
        .filter(|i| listed(i) && (want.is_empty() || want.contains(&i.kind)))
        .filter(|i| (safe && i.badge() == Some(Badge::Safe)) || (paths.contains(&i.path) && i.selectable()))
        .collect();
    for p in paths {
        match items.iter().find(|i| &i.path == p && listed(i)) {
            None => eprintln!("sweep: {} isn't something sweep lists", p.display()),
            Some(i) if !i.selectable() => {
                eprintln!(
                    "sweep: {} is kept: {}",
                    p.display(),
                    i.verdict.as_ref().map(|v| v.reason.as_str()).unwrap_or("")
                )
            }
            _ => {}
        }
    }
    let names: HashMap<usize, (Kind, String)> = chosen.iter().map(|i| (i.id, (i.kind, ctx.tilde(&i.path)))).collect();
    let jobs: Vec<Job> = chosen
        .iter()
        .map(|i| Job { item: (*i).clone(), seen: i.badge().unwrap(), opts: DeleteOpts { backup: !no_backup } })
        .collect();
    let (tx, rx) = std::sync::mpsc::channel();
    let p = theme::Paint::stdout();
    let printer = std::thread::spawn(move || {
        let mut out = Vec::new();
        for ev in rx {
            let Progress::Finished(id, o) = ev else { continue };
            let (kind, path) = names.get(&id).cloned().unwrap_or((Kind::Temp, String::new()));
            let (outcome, bytes, detail) = match &o {
                Outcome::Deleted { freed, note } => ("deleted", *freed, note.clone()),
                Outcome::WouldDelete { freed, note } => ("would-delete", *freed, note.clone()),
                Outcome::Skipped(why) => ("skipped", 0, why.clone()),
                Outcome::Failed(err) => ("failed", 0, err.clone()),
            };
            if !json {
                let (mark, colour) = match outcome {
                    "deleted" | "would-delete" => ("✓", hex::GREEN),
                    "skipped" => ("–", hex::YELLOW),
                    _ => ("✗", hex::RED),
                };
                let size = if mark == "✓" { fmt::bytes(bytes) } else { outcome.to_string() };
                let detail = if detail.is_empty() { String::new() } else { format!("  {detail}") };
                println!(
                    "{} {}  {} {}{}",
                    p.fg(colour, mark),
                    p.fg(if mark == "✓" { hex::FG } else { colour }, format!("{size:>8}")),
                    p.fg(hex::COMMENT, format!("[{}]", kind.slug())),
                    p.fg(hex::TEXT, &path),
                    p.fg(hex::MUTED, detail)
                );
            }
            out.push(CleanItem { kind, path, outcome, bytes, detail });
        }
        out
    });
    let summary = delete::run(&ctx, &scan::delete_pool(), jobs, dry_run, &AtomicBool::new(false), &tx);
    drop(tx);
    let results = printer.join().unwrap_or_default();
    if json {
        #[derive(Serialize)]
        struct Out {
            dry_run: bool,
            freed_bytes: u64,
            deleted: usize,
            skipped: usize,
            failed: usize,
            log: Option<String>,
            items: Vec<CleanItem>,
        }
        let out = Out {
            dry_run,
            freed_bytes: summary.freed,
            deleted: summary.deleted,
            skipped: summary.skipped,
            failed: summary.failed,
            log: summary.log.as_ref().map(|p| p.display().to_string()),
            items: results,
        };
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        let verb = if dry_run { "would free" } else { "freed" };
        println!(
            "\n{} {}{}",
            p.bold(p.fg(
                hex::GREEN,
                format!("{} {verb} {}", fmt::count(summary.deleted, "item", "items"), fmt::bytes(summary.freed))
            )),
            p.fg(hex::GUTTER, " · "),
            p.fg(hex::MUTED, format!("{} skipped · {} failed", summary.skipped, summary.failed))
        );
        for (vol, d) in &summary.space {
            let change = format!("{}{}", if *d < 0 { "-" } else { "+" }, fmt::bytes(d.unsigned_abs()));
            println!("{} {}", p.fg(hex::MUTED, format!("free space on {vol}:")), p.fg(hex::FG, change));
        }
        if let Some(log) = &summary.log {
            println!("{} {}", p.fg(hex::MUTED, "log:"), p.fg(hex::TEXT, log.display().to_string()));
        }
    }
    Ok(if summary.failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS })
}

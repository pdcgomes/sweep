//! `sweep scan` output, for people and for scripts.

use crate::config::Thresholds;
use crate::ctx::Ctx;
use crate::disks::{self, Disk};
use crate::fmt;
use crate::model::{Badge, CachePlace, Item, Kind, Target, XcodeFile};
use crate::theme::{Paint, hex};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Totals {
    pub safe_bytes: u64,
    pub review_bytes: u64,
    pub kept_bytes: u64,
    pub safe: usize,
    pub review: usize,
    pub kept: usize,
    /// Safe or Review items whose size can't be known (snapshots).
    pub unknown_size: usize,
    /// Kept items whose size wasn't measured.
    pub kept_unmeasured: usize,
    pub measuring: usize,
}

impl Totals {
    /// "34 GB (4)", or just a count when some Kept items weren't measured.
    pub fn kept_label(&self) -> String {
        if self.kept_unmeasured > 0 {
            fmt::count(self.kept, "item", "items")
        } else if self.kept == 0 {
            fmt::bytes(0)
        } else {
            format!("{} ({})", fmt::bytes(self.kept_bytes), fmt::num(self.kept))
        }
    }
}

pub fn totals<'a>(items: impl IntoIterator<Item = &'a Item>) -> Totals {
    let mut t = Totals::default();
    for i in items {
        let Some(v) = &i.verdict else {
            t.measuring += 1;
            continue;
        };
        if v.hidden {
            continue;
        }
        let b = v.bytes.unwrap_or(0);
        match v.badge {
            Badge::Safe => {
                t.safe += 1;
                t.safe_bytes += b;
            }
            Badge::Review => {
                t.review += 1;
                t.review_bytes += b;
            }
            Badge::Kept => {
                t.kept += 1;
                t.kept_bytes += b;
                if v.unmeasured {
                    t.kept_unmeasured += 1;
                }
            }
        }
        if v.bytes.is_none() && v.badge != Badge::Kept {
            t.unknown_size += 1;
        }
    }
    t
}

/// Visible items grouped by kind, kinds in rule order so the list never
/// reshuffles as sizes come in. Within a kind, Safe items come first, then
/// Review, then Kept, each by size.
pub fn grouped<'a>(items: impl IntoIterator<Item = &'a Item>) -> Vec<(Kind, Vec<&'a Item>)> {
    let mut groups: BTreeMap<Kind, Vec<&Item>> = BTreeMap::new();
    for i in items.into_iter().filter(|i| !i.verdict.as_ref().is_some_and(|v| v.hidden)) {
        groups.entry(i.kind).or_default().push(i);
    }
    let mut out: Vec<(Kind, Vec<&Item>)> = groups.into_iter().collect();
    for (_, list) in &mut out {
        // Items still being measured sit with the Safe ones until they're known.
        let rank = |i: &Item| i.badge().unwrap_or(Badge::Safe);
        list.sort_by(|a, b| {
            rank(a)
                .cmp(&rank(b))
                .then_with(|| b.bytes().unwrap_or(0).cmp(&a.bytes().unwrap_or(0)))
                .then_with(|| a.label.cmp(&b.label))
        });
    }
    out
}

/// The sub-group an item sits in under its kind: the repository of a
/// worktree or build output, the name pattern of a temp entry, where an app
/// cache is, the app owning a Core ML cache, a simulator's runtime, a
/// scratch folder's root, the editor or app a model or extension is for.
pub fn group_of(item: &Item) -> Option<String> {
    match &item.target {
        Target::Worktree(info) => info.main.file_name().map(|n| n.to_string_lossy().into_owned()),
        Target::BuildOutput { .. } => item.label.split('/').next().map(str::to_string),
        Target::Temp { pattern, .. } => Some(pattern.to_string()),
        Target::AppCache { place, .. } => Some(
            match place {
                CachePlace::Library => "~/Library/Caches",
                CachePlace::User => "developer tools",
            }
            .to_string(),
        ),
        Target::Xcode(XcodeFile::DeviceSupport { .. }) => Some("device symbols".into()),
        Target::Xcode(XcodeFile::Archive) => Some("archives".into()),
        Target::CoreMl { owner, .. } => Some(owner.clone()),
        Target::SimDevice(d) => Some(d.runtime.clone()),
        Target::Extension { editor, .. } => Some(editor.to_string()),
        Target::Model(_) => item.label.split(" · ").next().map(str::to_string),
        Target::Scratch { .. } => item.path.parent().map(|p| p.display().to_string()),
        _ => None,
    }
}

/// One kind's items split into sub-groups, the ones that could free most
/// first. A kind whose items all fall in one sub-group isn't split.
pub fn subgroups<'a>(list: &[&'a Item]) -> Vec<(Option<String>, Vec<&'a Item>)> {
    let mut map: BTreeMap<Option<String>, Vec<&Item>> = BTreeMap::new();
    for i in list {
        map.entry(group_of(i)).or_default().push(i);
    }
    if map.len() <= 1 {
        return vec![(None, list.to_vec())];
    }
    let mut out: Vec<(Option<String>, Vec<&Item>)> = map.into_iter().collect();
    out.sort_by_key(|(g, members)| {
        let t = totals(members.iter().copied());
        (
            std::cmp::Reverse(t.safe_bytes + t.review_bytes),
            std::cmp::Reverse(t.safe + t.review),
            std::cmp::Reverse(members.len()),
            g.clone(),
        )
    });
    out
}

/// The gist of a reason, for counting why things stay:
/// "in use: written 3 h ago" → "in use".
pub fn gist(reason: &str) -> &str {
    let cut = reason.find([':', '(']).unwrap_or(reason.len());
    reason[..cut].trim()
}

/// Kept items' reasons by gist, most common first.
pub fn kept_gists<'a>(items: impl IntoIterator<Item = &'a Item>) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for i in items {
        if let Some(v) = i.verdict.as_ref().filter(|v| v.badge == Badge::Kept && !v.hidden) {
            *counts.entry(gist(&v.reason).to_string()).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = counts.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v
}

/// "in use 19,084 · always kept 2 · other reasons 3".
pub fn gists_line(gists: &[(String, usize)], limit: usize) -> String {
    let mut parts: Vec<String> = gists.iter().take(limit).map(|(g, n)| format!("{g} {}", fmt::num(*n))).collect();
    let rest: usize = gists.iter().skip(limit).map(|(_, n)| n).sum();
    if rest > 0 {
        parts.push(format!("other reasons {}", fmt::num(rest)));
    }
    parts.join(" · ")
}

/// What a rule looks for, in a line.
pub fn describe(kind: Kind, t: &Thresholds) -> String {
    match kind {
        Kind::Worktree => format!(
            "Linked worktrees of the repositories under the source folder. Safe once clean and not edited or built for {} days.",
            t.worktree_edit_days
        ),
        Kind::Temp => format!(
            "$TMPDIR entries left by known tools and tests, once nothing inside was written for {} days.",
            t.temp_days
        ),
        Kind::BuildOutput => format!(
            "What builds and installs leave in projects nobody has edited for {} days: output is Safe, installed dependencies are for review.",
            t.build_output_days
        ),
        Kind::AppCache => format!(
            "Apps' caches, once their app isn't running and nothing was written for {} days. Apps make them again.",
            t.caches_days
        ),
        Kind::DerivedData => format!(
            "Xcode's build products for projects that no longer exist, and for review those of projects idle {} days.",
            t.derived_data_idle_days
        ),
        Kind::XcodeSupport => {
            "Symbols copied from devices, Xcode's documentation, preview and test devices; archives for review.".into()
        }
        Kind::Extension => "Versions of VS Code's extensions it no longer uses.".into(),
        Kind::Model => "AI models downloaded to run here, for review: each comes back with a download.".into(),
        Kind::Trash => "What's in the Trash, for review: emptying it is for good.".into(),
        Kind::CoreMl => {
            format!("Compiled Core ML models unused for {} days, or compiled for another macOS build.", t.coreml_days)
        }
        Kind::SimDevice => "Simulators: unavailable ones are Safe; the rest hold apps and data to review.".into(),
        Kind::SimRuntime => format!("Simulator runtimes unused for {} days, for review.", t.runtime_unused_days),
        Kind::Docker => {
            "Docker Desktop's disk image: every image, container and volume. Only while Docker is stopped.".into()
        }
        Kind::Logs => format!("Files in ~/Library/Logs not written for {} days.", t.logs_days),
        Kind::PackageCache => {
            "pip, uv, Homebrew, npm, Yarn, SwiftPM, Playwright and node-gyp download again what they need.".into()
        }
        Kind::Scratch => {
            format!("Top-level folders of the scratch roots in config.toml, idle for {} days.", t.scratch_days)
        }
        Kind::Downloads => format!(
            "Files in ~/Downloads over {} MB, last changed over {} days ago, for review.",
            t.downloads_min_mb, t.downloads_days
        ),
        Kind::Snapshot => "Time Machine's local snapshots, for review. Their size can't be known.".into(),
    }
}

/// "Safe 1.9 GB (1,128) · Review 3.6 GB (3) · Kept 19,084 items", leaving out what's empty.
pub fn summary_painted(t: &Totals, p: Paint) -> String {
    let mut parts = Vec::new();
    if t.safe > 0 {
        parts.push(p.fg(hex::GREEN, format!("Safe {} ({})", fmt::bytes(t.safe_bytes), fmt::num(t.safe))));
    }
    if t.review > 0 {
        parts.push(p.fg(hex::YELLOW, format!("Review {} ({})", fmt::bytes(t.review_bytes), fmt::num(t.review))));
    }
    if t.kept > 0 {
        parts.push(p.fg(hex::COMMENT, format!("Kept {}", t.kept_label())));
    }
    if t.measuring > 0 {
        parts.push(p.fg(hex::COMMENT, format!("measuring {}", t.measuring)));
    }
    parts.join(&p.fg(hex::GUTTER, " · "))
}

/// One line per physical disk: a bar, how full it is, and what sweep could free there.
pub fn disk_lines(disks: &[Disk], items: &[Item], p: Paint) -> String {
    let mut loc = disks::Locator::new(disks);
    let reclaim = loc.reclaim(disks.len(), items, |_| false);
    let (glyphs, colours) = if p.on() {
        (["━", "━", "━", "─"], [hex::BLUE, hex::GREEN, hex::YELLOW, hex::GUTTER])
    } else {
        (disks::BAR_GLYPHS, [hex::FG; 4])
    };
    let mut s = String::new();
    for (d, r) in disks.iter().zip(&reclaim) {
        let bar: String = disks::bar_cells(d, r, 30)
            .iter()
            .zip(glyphs)
            .zip(colours)
            .map(|((n, g), c)| p.fg(c, g.repeat(*n)))
            .collect();
        let sev = d.severity();
        let sev_colour = match sev {
            disks::Severity::Ok => hex::GREEN,
            disks::Severity::Low => hex::ORANGE,
            disks::Severity::Critical => hex::RED,
        };
        let title: String = d.title().chars().take(30).collect();
        s += &format!(
            "  {} {}  {bar}  {}  {}  {}\n",
            p.fg(sev_colour, "●"),
            p.bold(p.fg(hex::FG, format!("{title:<30}"))),
            p.fg(hex::TEXT, disks::usage(d)),
            p.bold(p.fg(sev_colour, format!("{:<8}", sev.label()))),
            p.fg(hex::MUTED, disks::prospects(d, r, usize::MAX))
        );
    }
    s
}

pub fn text(
    ctx: &Ctx,
    items: &[Item],
    notes: &[(Kind, String)],
    all: bool,
    disks: Option<&[Disk]>,
    p: Paint,
) -> String {
    let mut s = String::new();
    let t = totals(items);
    let when = chrono::DateTime::<chrono::Local>::from(ctx.now).format("%Y-%m-%d %H:%M");
    s += &format!(
        "{}{}\n",
        p.bold(p.fg(hex::BLUE, "sweep")),
        p.fg(hex::MUTED, format!(" · {when} · macOS {}", ctx.os_build))
    );
    if let Some(d) = disks.filter(|d| !d.is_empty()) {
        s += &disk_lines(d, items, p);
    }
    s += &format!("{}\n", summary_painted(&t, p));
    if t.unknown_size > 0 {
        let more = format!("plus {} of unknown size", fmt::count(t.unknown_size, "item", "items"));
        s += &format!("{}\n", p.fg(hex::MUTED, more));
    }
    let line = |i: &Item| {
        let v = i.verdict.as_ref().unwrap();
        let size = if v.unmeasured { "—".to_string() } else { fmt::opt_bytes(v.bytes) };
        let badge = match v.badge {
            Badge::Safe => hex::GREEN,
            Badge::Review => hex::YELLOW,
            Badge::Kept => hex::COMMENT,
        };
        format!(
            "{} {}  {}{}{}",
            p.fg(badge, format!("{:<6}", v.badge.label())),
            p.fg(hex::FG, format!("{size:>8}")),
            p.fg(hex::FG, &i.label),
            p.fg(hex::GUTTER, "  ·  "),
            p.fg(hex::MUTED, &v.reason)
        )
    };
    for (kind, list) in grouped(items) {
        s += &format!(
            "\n{}  {}\n",
            p.bold(p.fg(hex::BLUE, format!("{:<28}", kind.title()))),
            summary_painted(&totals(list.iter().copied()), p)
        );
        s += &format!("  {}\n", p.fg(hex::MUTED, describe(kind, &ctx.cfg.thresholds)));
        let subs = subgroups(&list);
        for (g, members) in &subs {
            let indent = if g.is_some() { "    " } else { "  " };
            if let Some(g) = g {
                s += &format!(
                    "  {}  {}\n",
                    p.fg(hex::FG, format!("{g:<26}")),
                    summary_painted(&totals(members.iter().copied()), p)
                );
            }
            if all {
                for i in members {
                    s += &format!("{indent}{}\n", line(i));
                }
            }
        }
        if !all {
            let live: Vec<&&Item> = list.iter().filter(|i| i.selectable()).collect();
            for i in live.iter().take(8) {
                s += &format!("  {}\n", line(i));
            }
            if live.len() > 8 {
                let rest: u64 = live[8..].iter().filter_map(|i| i.bytes()).sum();
                let more =
                    format!("{:>15}  … and {} more", fmt::bytes(rest), fmt::count(live.len() - 8, "item", "items"));
                s += &format!("  {}\n", p.fg(hex::MUTED, more));
            }
            let gists = kept_gists(list.iter().copied());
            if !gists.is_empty() {
                s += &format!("  {}\n", p.fg(hex::COMMENT, format!("Kept: {}", gists_line(&gists, 4))));
            }
        }
    }
    if !notes.is_empty() {
        s += &format!("\n{}\n", p.bold(p.fg(hex::YELLOW, "Notes")));
        for (k, n) in notes {
            s += &format!("  {} {}\n", p.fg(hex::FG, format!("{}:", k.title())), p.fg(hex::MUTED, n));
        }
    }
    s
}

#[derive(Serialize)]
struct JsonItem<'a> {
    id: usize,
    kind: Kind,
    path: String,
    label: &'a str,
    badge: Badge,
    reason: &'a str,
    /// What deleting frees; null when unknown or not measured.
    bytes: Option<u64>,
    measured: bool,
    du_bytes: Option<u64>,
    last_write: Option<String>,
    checks: &'a [crate::model::Check],
}

#[derive(Serialize)]
struct JsonNote<'a> {
    kind: Kind,
    text: &'a str,
}

#[derive(Serialize)]
struct JsonDisk<'a> {
    id: &'a str,
    model: &'a str,
    internal: bool,
    startup: bool,
    volumes: &'a [String],
    size_bytes: u64,
    free_bytes: u64,
    used_bytes: u64,
    severity: &'static str,
    /// What sweep found on this disk: Safe and Review bytes.
    reclaimable: disks::Reclaim,
}

#[derive(Serialize)]
struct JsonReport<'a> {
    scanned_at: String,
    os_build: &'a str,
    disks: Vec<JsonDisk<'a>>,
    totals: Totals,
    items: Vec<JsonItem<'a>>,
    notes: Vec<JsonNote<'a>>,
}

pub fn json(ctx: &Ctx, items: &[Item], notes: &[(Kind, String)], disks: &[Disk]) -> String {
    let reclaim = disks::Locator::new(disks).reclaim(disks.len(), items, |_| false);
    let disks = disks
        .iter()
        .zip(reclaim)
        .map(|(d, r)| JsonDisk {
            id: &d.id,
            model: &d.model,
            internal: d.internal,
            startup: d.startup,
            volumes: &d.volumes,
            size_bytes: d.size,
            free_bytes: d.free(),
            used_bytes: d.used(),
            severity: d.severity().label(),
            reclaimable: r,
        })
        .collect();
    let stamp = |t: std::time::SystemTime| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339();
    let mut list = Vec::new();
    for (_, group) in grouped(items) {
        for i in group {
            let v = i.verdict.as_ref().expect("scan finished");
            list.push(JsonItem {
                id: i.id,
                kind: i.kind,
                path: i.path.display().to_string(),
                label: &i.label,
                badge: v.badge,
                reason: &v.reason,
                bytes: v.bytes,
                measured: !v.unmeasured,
                du_bytes: v.du,
                last_write: v.last_write.map(stamp),
                checks: &v.checks,
            });
        }
    }
    let report = JsonReport {
        scanned_at: stamp(ctx.now),
        os_build: &ctx.os_build,
        disks,
        totals: totals(items),
        items: list,
        notes: notes.iter().map(|(k, t)| JsonNote { kind: *k, text: t }).collect(),
    };
    serde_json::to_string_pretty(&report).expect("serialisable")
}

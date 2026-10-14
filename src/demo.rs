//! `sweep demo`: the interface on a made-up Mac, for screenshots that show
//! nothing of the one they're taken on. Deleting only pretends.

use crate::config::Thresholds;
use crate::disks::{Disk, Store};
use crate::model::{
    Badge, CachePlace, Check, Item, Kind, ModelSource, Plan, SimDeviceInfo, SimRuntimeInfo, Target, Verdict,
    WorktreeInfo, XcodeFile,
};
use crate::tui::App;
use std::path::PathBuf;

const GB: u64 = 1_000_000_000;
const MB: u64 = 1_000_000;
const HOME: &str = "/Users/alex";
const TMP: &str = "/var/folders/k2/7q1wz3xn5bd0000gn/T";

pub fn app(thresholds: Thresholds) -> App {
    let mut d = Demo { items: Vec::new() };
    d.fill();
    let notes = vec![(Kind::Temp, "left 214 entries in $TMPDIR alone: names sweep doesn't recognise".to_string())];
    let pins = vec![(PathBuf::from("/Volumes/Work"), 1), (PathBuf::from("/"), 0)];
    App::demo(d.items, notes, disks(), pins, thresholds)
}

fn disks() -> Vec<Disk> {
    let disk = |id: &str, model: &str, internal: bool, size: u64, free: u64, volume: &str| Disk {
        id: id.into(),
        model: model.into(),
        internal,
        startup: internal,
        size,
        volumes: vec![volume.into()],
        stores: vec![Store { id: format!("{id}s2"), free, probe: None }],
    };
    vec![
        disk("disk0", "APPLE SSD AP1024Z", true, 1_000_555_581_440, 38 * GB, "Macintosh HD"),
        disk("disk4", "Samsung T7", false, 2_000_398_934_016, 702 * GB, "Work"),
    ]
}

struct Demo {
    items: Vec<Item>,
}

/// A check: `+` passed, `-` failed, `·` for information.
fn check(s: &str) -> Check {
    match s.split_at(2) {
        ("+ ", t) => Check::pass(t),
        ("- ", t) => Check::fail(t),
        (_, t) => Check::info(t),
    }
}

#[allow(clippy::too_many_arguments)]
impl Demo {
    fn add(
        &mut self,
        kind: Kind,
        path: String,
        label: &str,
        target: Target,
        badge: Badge,
        reason: &str,
        bytes: u64,
        checks: &[&str],
    ) {
        let mut v = Verdict::new(badge, reason);
        v.bytes = (bytes > 0).then_some(bytes);
        v.checks = checks.iter().map(|c| check(c)).collect();
        let id = self.items.len();
        self.items.push(Item { id, kind, path: PathBuf::from(path), label: label.into(), target, verdict: Some(v) });
    }

    fn worktree(
        &mut self,
        repo: &str,
        name: &str,
        branch: &str,
        badge: Badge,
        reason: &str,
        bytes: u64,
        checks: &[&str],
    ) {
        let info = WorktreeInfo {
            main: format!("{HOME}/src/{repo}").into(),
            head: "4f1c2e9".into(),
            branch: Some(branch.into()),
            locked: None,
            nested: Vec::new(),
        };
        self.add(
            Kind::Worktree,
            format!("{HOME}/src/{name}"),
            name,
            Target::Worktree(info),
            badge,
            reason,
            bytes,
            checks,
        );
        let dirty = reason.contains("uncommitted");
        if let Some(v) = self.items.last_mut().and_then(|i| i.verdict.as_mut()) {
            v.plan = Plan::Worktree { head: "4f1c2e9".into(), dirty, branch_head: false, backup_ref: String::new() };
        }
    }

    fn temp(
        &mut self,
        name: &str,
        pattern: &'static str,
        family: &'static str,
        badge: Badge,
        reason: &str,
        bytes: u64,
        newest: &str,
    ) {
        let checks = match badge {
            Badge::Kept => vec![format!("- {}: {newest}", reason.trim_start_matches("in use: "))],
            _ => vec![
                format!("+ nothing written for {} (newest: {newest})", reason.trim_start_matches("idle ")),
                "+ no running process uses it".into(),
            ],
        };
        let checks: Vec<&str> = checks.iter().map(String::as_str).collect();
        self.add(
            Kind::Temp,
            format!("{TMP}/{name}"),
            name,
            Target::Temp { pattern, family },
            badge,
            reason,
            bytes,
            &checks,
        );
    }

    fn cache(&mut self, owner: &str, place: CachePlace, badge: Badge, reason: &str, bytes: u64, checks: &[&str]) {
        let path = match place {
            CachePlace::Library => format!("{HOME}/Library/Caches/{owner}"),
            CachePlace::User => format!("/var/folders/k2/7q1wz3xn5bd0000gn/C/{owner}"),
        };
        let target = Target::AppCache { owner: owner.into(), place };
        self.add(Kind::AppCache, path, owner, target, badge, reason, bytes, checks);
    }

    fn fill(&mut self) {
        use Badge::{Kept, Review, Safe};
        let xcode = format!("{HOME}/Library/Developer/Xcode");

        self.worktree(
            "acme-web",
            "acme-web-checkout",
            "feature/checkout",
            Safe,
            "clean, idle 6 days",
            3_400 * MB,
            &[
                "+ nothing uncommitted",
                "+ no file outside build/ edited for 6 days (newest: src/checkout/Summary.tsx)",
                "+ nothing built for 6 days",
                "+ no running process uses it",
                "+ branch feature/checkout keeps the commits",
            ],
        );
        self.worktree(
            "acme-web",
            "acme-web-search",
            "fix/search-ranking",
            Safe,
            "clean, idle 3 days",
            2_100 * MB,
            &[
                "+ nothing uncommitted",
                "+ no file outside build/ edited for 3 days (newest: src/search/rank.ts)",
                "· no build/ folder",
                "+ no running process uses it",
                "+ branch fix/search-ranking keeps the commits",
            ],
        );
        self.worktree(
            "acme-web",
            "acme-web-payments",
            "feature/payments",
            Review,
            "2 uncommitted changes, idle 9 days",
            1_800 * MB,
            &[
                "- uncommitted work: 1 changed, 1 untracked",
                "+ no file outside build/ edited for 9 days (newest: src/payments/Stripe.ts)",
                "+ no running process uses it",
                "+ branch feature/payments keeps the commits",
            ],
        );
        self.worktree(
            "acme-web",
            "acme-web-hotfix",
            "hotfix/login",
            Kept,
            "in use: edited 2 h ago",
            1_200 * MB,
            &["+ nothing uncommitted", "- edited 2 h ago: src/auth/session.ts"],
        );
        self.worktree(
            "atlas-ios",
            "atlas-ios-onboarding",
            "feature/onboarding",
            Safe,
            "clean, idle 9 days",
            5_600 * MB,
            &[
                "+ nothing uncommitted",
                "+ no file outside build/ edited for 9 days (newest: Atlas/Onboarding/WelcomeView.swift)",
                "+ nothing built for 9 days",
                "+ no running process uses it",
                "+ branch feature/onboarding keeps the commits",
            ],
        );
        self.worktree(
            "atlas-ios",
            "atlas-ios-widgets",
            "feature/widgets",
            Kept,
            "in use: built 25 min ago",
            4_100 * MB,
            &["+ nothing uncommitted", "- built 25 min ago: build/Debug-iphonesimulator/AtlasWidgets.appex"],
        );
        self.worktree(
            "atlas-ios",
            "atlas-ios-release",
            "release/2.4",
            Kept,
            "always kept: the release checkout",
            3_900 * MB,
            &["- always kept: the release checkout"],
        );

        let build = |project: &str, output: &'static str| Target::BuildOutput {
            project: format!("{HOME}/src/{project}").into(),
            output,
        };
        let src = |p: &str| format!("{HOME}/src/{p}");
        self.add(
            Kind::BuildOutput,
            src("game-jam/target"),
            "game-jam/target",
            build("game-jam", "target"),
            Safe,
            "cargo build makes it again; project idle 7 weeks",
            7_900 * MB,
            &[
                "+ nothing in game-jam edited for 7 weeks (newest: src/main.rs)",
                "+ nothing written for 7 weeks (newest: release/game-jam)",
                "+ no running process uses it",
            ],
        );
        self.add(
            Kind::BuildOutput,
            src("ml-notebooks/.venv"),
            "ml-notebooks/.venv",
            build("ml-notebooks", ".venv"),
            Review,
            "uv sync or pip install brings it back; project idle 2 months",
            3_800 * MB,
            &["+ nothing in ml-notebooks edited for 2 months (newest: train.ipynb)", "+ no running process uses it"],
        );
        self.add(
            Kind::BuildOutput,
            src("portfolio/node_modules"),
            "portfolio/node_modules",
            build("portfolio", "node_modules"),
            Review,
            "npm install brings it back, from the registry; project idle 5 months",
            1_200 * MB,
            &["+ nothing in portfolio edited for 5 months (newest: pages/index.tsx)", "+ no running process uses it"],
        );
        self.add(
            Kind::BuildOutput,
            src("portfolio/.next"),
            "portfolio/.next",
            build("portfolio", ".next"),
            Safe,
            "next build makes it again; project idle 5 months",
            640 * MB,
            &["+ nothing in portfolio edited for 5 months (newest: pages/index.tsx)", "+ no running process uses it"],
        );
        self.add(
            Kind::BuildOutput,
            src("acme-web/node_modules"),
            "acme-web/node_modules",
            build("acme-web", "node_modules"),
            Kept,
            "in use: project edited 3 h ago",
            1_400 * MB,
            &["- project edited 3 h ago: src/app.tsx"],
        );

        let (tmp, uuid, chrome, tests, xcresult) = (
            ("tmp*", "temp folder"),
            ("<UUID>", "UUID-named temp folder"),
            ("com.google.Chrome.*", "Chrome temp folder"),
            ("TemporaryDirectory.*", "test temp folder"),
            ("ResultBundle_*", "Xcode result bundle"),
        );
        for (name, badge, reason, bytes, newest) in [
            ("tmpx8k2v1qd", Safe, "idle 4 days", 1_100 * MB, "wheels/torch-2.4.0.whl"),
            ("tmp3n0w7d_h", Safe, "idle 6 days", 820 * MB, "frames/0921.png"),
            ("tmpk1f9zz2p", Safe, "idle 3 days", 410 * MB, "export.mov"),
            ("tmpa7c0q4m9", Safe, "idle 2 weeks", 96 * MB, "bundle.js"),
            ("tmp9v5t2l0e", Safe, "idle 3 days", 34 * MB, "report.pdf"),
            ("tmpp0r8s6j3", Kept, "in use: written 12 min ago", 220 * MB, "cache/index"),
        ] {
            self.temp(name, tmp.0, tmp.1, badge, reason, bytes, newest);
        }
        for (name, badge, reason, bytes, newest) in [
            ("6F1C9A2E-3B7D-4E58-9C0A-1D2E3F4A5B6C", Safe, "idle 5 days", 2_300 * MB, "Render/output.mp4"),
            ("0B8E4D21-7A6F-4C39-8E12-9F0A1B2C3D4E", Safe, "idle 2 days", 700 * MB, "Archive.zip"),
            ("A3D5F7B9-1C2E-4F60-8A9B-0C1D2E3F4A5B", Safe, "idle 8 days", 120 * MB, "thumbnails.db"),
            ("C9E1A3B5-7D2F-4061-9A8B-7C6D5E4F3A2B", Kept, "in use: written 3 min ago", 60 * MB, "session.json"),
        ] {
            self.temp(name, uuid.0, uuid.1, badge, reason, bytes, newest);
        }
        for (name, badge, reason, bytes, newest) in [
            ("com.google.Chrome.Xk2PqL", Safe, "idle 3 days", 210 * MB, "Default/Cache/data_3"),
            ("com.google.Chrome.m9TzQa", Safe, "idle 9 days", 64 * MB, "Default/Cache/data_1"),
        ] {
            self.temp(name, chrome.0, chrome.1, badge, reason, bytes, newest);
        }
        self.temp("TemporaryDirectory.Hq3vXe", tests.0, tests.1, Safe, "idle 4 days", 180 * MB, "Fixtures/large.json");
        self.temp("TemporaryDirectory.w8RtYb", tests.0, tests.1, Safe, "idle 2 days", 22 * MB, "db.sqlite");
        self.temp(
            "ResultBundle_2026-10-09_14-12-03.xcresult",
            xcresult.0,
            xcresult.1,
            Safe,
            "idle 5 days",
            380 * MB,
            "Data/data.0~",
        );

        self.cache(
            "com.spotify.client",
            CachePlace::Library,
            Safe,
            "a cache, made again when needed; idle 12 days",
            2_800 * MB,
            &[
                "+ com.spotify.client isn't running",
                "+ nothing written for 12 days (newest: Data/index.dat)",
                "+ no running process uses it",
            ],
        );
        self.cache(
            "com.figma.Desktop",
            CachePlace::Library,
            Safe,
            "a cache, made again when needed; idle 9 days",
            1_300 * MB,
            &[
                "+ com.figma.Desktop isn't running",
                "+ nothing written for 9 days (newest: Cache.db)",
                "+ no running process uses it",
            ],
        );
        self.cache(
            "com.microsoft.teams2",
            CachePlace::Library,
            Safe,
            "a cache, made again when needed; idle 3 months",
            960 * MB,
            &[
                "+ com.microsoft.teams2 isn't running",
                "+ nothing written for 3 months (newest: fsCachedData/7A1F)",
                "+ no running process uses it",
            ],
        );
        self.cache(
            "com.tinyspeck.slackmacgap",
            CachePlace::Library,
            Kept,
            "its app is running: Slack is running",
            1_100 * MB,
            &["- Slack is running"],
        );
        self.cache(
            "clang",
            CachePlace::User,
            Safe,
            "a cache, made again when needed; idle 8 days",
            1_600 * MB,
            &[
                "+ clang isn't running",
                "+ nothing written for 8 days (newest: ModuleCache/3J1QZ/Foundation.pcm)",
                "+ no running process uses it",
            ],
        );

        let derived = |name: &str| format!("{xcode}/DerivedData/{name}");
        self.add(
            Kind::DerivedData,
            derived("Atlas-fqzbgxkeuwhbcsdkqvjrnzpmtyai"),
            "Atlas-fqzbgxkeuwhbcsdkqvjrnzpmtyai",
            Target::DerivedData { workspace: Some(src("atlas-ios/Atlas.xcodeproj").into()) },
            Review,
            "nothing built for 5 weeks; Xcode builds it again",
            9_400 * MB,
            &[
                "· its project still exists: ~/src/atlas-ios/Atlas.xcodeproj",
                "+ nothing written for 5 weeks (newest: Logs/Build/LogStoreManifest.plist)",
                "+ no running process uses it",
            ],
        );
        self.add(
            Kind::DerivedData,
            derived("Prototype-dkqpzmwjxhavtbnelrcsuyogfiq"),
            "Prototype-dkqpzmwjxhavtbnelrcsuyogfiq",
            Target::DerivedData { workspace: Some(src("old/Prototype.xcodeproj").into()) },
            Safe,
            "project gone, idle 3 months",
            4_200 * MB,
            &[
                "+ its project is gone: ~/src/old/Prototype.xcodeproj",
                "+ nothing written for 3 months (newest: Build/Products/Debug/Prototype.app)",
                "+ no running process uses it",
            ],
        );
        self.add(
            Kind::DerivedData,
            derived("AtlasWidgets-hzqmbtrlkwpyxcsdafnevgujoi"),
            "AtlasWidgets-hzqmbtrlkwpyxcsdafnevgujoi",
            Target::DerivedData { workspace: Some(src("atlas-ios-widgets/Atlas.xcodeproj").into()) },
            Kept,
            "in use: written 25 min ago",
            3_100 * MB,
            &["- written 25 min ago: Logs/Build/LogStoreManifest.plist"],
        );

        self.add(
            Kind::XcodeSupport,
            format!("{xcode}/iOS DeviceSupport/iPhone15,2 17.5 (21F90)"),
            "iOS · iPhone15,2 17.5 (21F90)",
            Target::Xcode(XcodeFile::DeviceSupport { newer: Some("iPhone15,2 18.1 (22B83)".into()) }),
            Safe,
            "superseded by iPhone15,2 18.1 (22B83); Xcode copies symbols again from a device that needs them",
            4_800 * MB,
            &["+ nothing written for 4 months (newest: Info.plist)", "+ no running process uses it"],
        );
        self.add(
            Kind::XcodeSupport,
            format!("{xcode}/DocumentationCache"),
            "documentation cache",
            Target::Xcode(XcodeFile::Documentation),
            Safe,
            "unused for 4 months; Xcode downloads it again",
            1_900 * MB,
            &["+ nothing written for 4 months (newest: v266/index.db)", "+ no running process uses it"],
        );
        self.add(
            Kind::XcodeSupport,
            format!("{xcode}/Archives/2026-03-02/Atlas 02-03-2026, 18.40.xcarchive"),
            "archive · Atlas 02-03-2026, 18.40 · 2026-03-02",
            Target::Xcode(XcodeFile::Archive),
            Review,
            "made 7 months ago; crash reports from this build need it to be symbolicated",
            1_100 * MB,
            &["+ nothing written for 7 months (newest: Info.plist)"],
        );

        self.add(
            Kind::CoreMl,
            format!("{HOME}/Library/Caches/com.adobe.lightroomCC/com.apple.e5rt.e5bundlecache/24G90"),
            "com.adobe.lightroomCC · macOS 24G90",
            Target::CoreMl { owner: "com.adobe.lightroomCC".into(), build: "24G90".into(), model: None },
            Safe,
            "compiled for macOS 24G90, not the running 25A354",
            312 * MB,
            &[
                "+ com.adobe.lightroomCC isn't running",
                "+ compiled for macOS build 24G90; this Mac runs 25A354",
                "+ no running process uses it",
            ],
        );

        let sim = |name: &str, runtime: &str, state: &str, available: bool| SimDeviceInfo {
            udid: "5C1E0B7A-9D3F-4A26-8B41-2E7F6C9D0A13".into(),
            name: name.into(),
            runtime: runtime.into(),
            state: state.into(),
            available,
            error: (!available).then(|| "runtime profile not found".into()),
        };
        let devices = format!("{HOME}/Library/Developer/CoreSimulator/Devices");
        for (name, runtime, state, available, badge, reason, bytes) in [
            ("iPhone 15 Pro", "iOS 17.5", "Shutdown", false, Safe, "unavailable: it can't boot any more", 2_600 * MB),
            (
                "iPad Air (5th generation)",
                "iOS 17.5",
                "Shutdown",
                false,
                Safe,
                "unavailable: it can't boot any more",
                1_400 * MB,
            ),
            ("iPhone 16", "iOS 18.1", "Shutdown", true, Review, "a simulator with its apps and data", 3_100 * MB),
            ("iPhone 16 Pro", "iOS 18.1", "Booted", true, Kept, "booted", 5_200 * MB),
        ] {
            let checks: Vec<String> = match (available, badge) {
                (false, _) => {
                    vec!["+ not running (shutdown)".into(), "+ unavailable: runtime profile not found".into()]
                }
                (true, Kept) => vec!["- booted".into()],
                _ => vec!["+ not running (shutdown)".into(), format!("· {runtime} is installed")],
            };
            let checks: Vec<&str> = checks.iter().map(String::as_str).collect();
            let label = format!("{name} · {runtime}");
            self.add(
                Kind::SimDevice,
                format!("{devices}/{name}"),
                &label,
                Target::SimDevice(sim(name, runtime, state, available)),
                badge,
                reason,
                bytes,
                &checks,
            );
        }

        let runtime = |name: &str, build: &str| SimRuntimeInfo {
            identifier: format!("com.apple.CoreSimulator.SimRuntime.{}", name.replace([' ', '.'], "-")),
            name: name.into(),
            build: build.into(),
            size_bytes: None,
            last_used: None,
            deletable: true,
            state: "Ready".into(),
        };
        self.add(
            Kind::SimRuntime,
            "/Library/Developer/CoreSimulator/Volumes/iOS_21F79".into(),
            "iOS 17.5 (21F79)",
            Target::SimRuntime(runtime("iOS 17.5", "21F79")),
            Review,
            "unused for 5 months; Xcode downloads it again",
            7_300 * MB,
            &["+ last used 5 months ago"],
        );
        self.add(
            Kind::SimRuntime,
            "/Library/Developer/CoreSimulator/Volumes/iOS_22B81".into(),
            "iOS 18.1 (22B81)",
            Target::SimRuntime(runtime("iOS 18.1", "22B81")),
            Kept,
            "used 2 h ago",
            8_100 * MB,
            &["- used 2 h ago"],
        );

        self.add(
            Kind::Docker,
            format!("{HOME}/Library/Containers/com.docker.docker/Data/vms/0/data/Docker.raw"),
            "Docker.raw",
            Target::Docker,
            Review,
            "every image, container and volume; Docker makes a new empty disk",
            32 * GB,
            &["+ Docker Desktop isn't running", "+ no running process uses it"],
        );

        for (name, reason, bytes, files) in [
            ("CoreSimulator", "212 log files older than 7 days", 410 * MB, "212 files of 260"),
            ("zoom.us", "3 log files older than 7 days", 18 * MB, "3 files of 4"),
        ] {
            let checks = [format!("+ {files} not written for 7 days"), "+ no running process uses it".into()];
            let checks: Vec<&str> = checks.iter().map(String::as_str).collect();
            self.add(
                Kind::Logs,
                format!("{HOME}/Library/Logs/{name}"),
                name,
                Target::Logs,
                Safe,
                reason,
                bytes,
                &checks,
            );
        }

        use crate::model::CacheTool;
        for (tool, name, path, badge, reason, bytes) in [
            (
                CacheTool::Homebrew,
                "Homebrew",
                "~/Library/Caches/Homebrew",
                Review,
                "Homebrew downloads it again when needed",
                2_400 * MB,
            ),
            (CacheTool::Pip, "pip", "~/Library/Caches/pip", Review, "pip downloads it again when needed", 1_700 * MB),
            (
                CacheTool::Yarn,
                "Yarn",
                "~/Library/Caches/Yarn",
                Kept,
                "always kept: offline installs need it",
                2_200 * MB,
            ),
        ] {
            let checks: Vec<String> = match badge {
                Kept => vec![format!("- {reason}")],
                _ => vec![format!("+ {name} isn't running"), "+ no running process uses it".into()],
            };
            let checks: Vec<&str> = checks.iter().map(String::as_str).collect();
            let label = format!("{name} cache · {path}");
            self.add(
                Kind::PackageCache,
                path.replacen('~', HOME, 1),
                &label,
                Target::PackageCache(tool),
                badge,
                reason,
                bytes,
                &checks,
            );
        }

        for (name, newer, bytes) in [
            ("ms-python.python-2024.2.1-darwin-arm64", "ms-python.python-2024.4.0-darwin-arm64", 210 * MB),
            (
                "rust-lang.rust-analyzer-0.3.1850-darwin-arm64",
                "rust-lang.rust-analyzer-0.3.2029-darwin-arm64",
                120 * MB,
            ),
            ("ms-vscode.cpptools-1.20.5-darwin-arm64", "ms-vscode.cpptools-1.21.6-darwin-arm64", 86 * MB),
        ] {
            let checks = [format!("+ VS Code has {newer} installed instead"), "+ no running process uses it".into()];
            let checks: Vec<&str> = checks.iter().map(String::as_str).collect();
            self.add(
                Kind::Extension,
                format!("{HOME}/.vscode/extensions/{name}"),
                name,
                Target::Extension { editor: "VS Code", app: "com.microsoft.VSCode" },
                Safe,
                "VS Code uses a newer version",
                bytes,
                &checks,
            );
        }

        let ollama = |name: &str| Target::Model(ModelSource::Ollama { name: name.into(), blobs: Vec::new() });
        self.add(
            Kind::Model,
            format!("{HOME}/.ollama/models/manifests/registry.ollama.ai/library/llama3.1/70b"),
            "Ollama · llama3.1:70b",
            ollama("llama3.1:70b"),
            Review,
            "ollama pull llama3.1:70b downloads it again",
            39_600 * MB,
            &["· pulled 4 months ago", "+ no running process has it loaded"],
        );
        self.add(
            Kind::Model,
            format!("{HOME}/.ollama/models/manifests/registry.ollama.ai/library/qwen2.5-coder/7b"),
            "Ollama · qwen2.5-coder:7b",
            ollama("qwen2.5-coder:7b"),
            Kept,
            "loaded by ollama (pid 913)",
            4_700 * MB,
            &["· pulled 3 weeks ago", "- loaded by ollama (pid 913)"],
        );
        self.add(
            Kind::Model,
            format!("{HOME}/.cache/huggingface/hub/models--stabilityai--sdxl-turbo"),
            "Hugging Face · stabilityai/sdxl-turbo",
            Target::Model(ModelSource::Download {
                app: "",
                again: "Hugging Face downloads it again when it's next used",
            }),
            Review,
            "Hugging Face downloads it again when it's next used",
            6_900 * MB,
            &["+ no running process uses it"],
        );

        self.add(
            Kind::Scratch,
            "/Volumes/Work/scratch/render-test".into(),
            "scratch/render-test",
            Target::Scratch { kept_by: None, days: 7.0 },
            Safe,
            "idle 3 weeks",
            12_400 * MB,
            &["+ nothing written for 3 weeks (newest: frames/0420.exr)", "+ no running process uses it"],
        );
        self.add(
            Kind::Scratch,
            "/Volumes/Work/scratch/datasets".into(),
            "scratch/datasets",
            Target::Scratch { kept_by: Some("datasets".into()), days: 7.0 },
            Kept,
            "kept in this scratch root (datasets)",
            48 * GB,
            &["- kept in this scratch root (datasets)"],
        );

        for (name, ago, bytes) in [
            ("Xcode_16.1.xip", "2 months ago", 3_200 * MB),
            ("ubuntu-24.04-desktop-arm64.iso", "5 months ago", 2_900 * MB),
        ] {
            let reason = format!("downloaded {ago}; you can download it again");
            let checks = [format!("+ downloaded or changed {ago}"), "+ no running process uses it".into()];
            let checks: Vec<&str> = checks.iter().map(String::as_str).collect();
            self.add(
                Kind::Downloads,
                format!("{HOME}/Downloads/{name}"),
                name,
                Target::Download,
                Review,
                &reason,
                bytes,
                &checks,
            );
        }

        self.add(
            Kind::Trash,
            format!("{HOME}/.Trash"),
            "Trash",
            Target::Trash,
            Review,
            "emptying the Trash deletes what's in it for good",
            6_300 * MB,
            &["· 1,284 things in it"],
        );
        self.add(
            Kind::Trash,
            "/Volumes/Work/.Trashes/501".into(),
            "Trash on Work",
            Target::Trash,
            Review,
            "emptying the Trash deletes what's in it for good",
            1_900 * MB,
            &["· 37 things in it"],
        );
    }
}

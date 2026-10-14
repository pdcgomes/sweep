//! Rule 2: $TMPDIR entries left by tools and tests, once nothing inside has
//! been written for two days. Names it doesn't recognise stay.

use super::{Found, Sink, gone, idle, idle_for, no_mounts, not_in_use, readable, sized};
use crate::ctx::Ctx;
use crate::glob;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::procs::socket_live;
use crate::walk;
use std::path::{Path, PathBuf};

const FAMILIES: &[(&str, &str)] = &[
    ("remotion-webpack-bundle-*", "Remotion bundle"),
    ("remotion-v4.*", "Remotion bundle"),
    ("react-motion-*", "Remotion bundle"),
    ("TemporaryDirectory.*", "test temp folder"),
    ("com.google.Chrome.*", "Chrome temp folder"),
    ("scoped_dir*", "Chrome temp folder"),
    ("ResultBundle_*", "Xcode result bundle"),
    ("ibtoold-*", "Interface Builder temp folder"),
    ("claude-loopback-*", "Claude Code temp file"),
    ("claude_launcher_*", "Claude Code launcher file"),
    ("codex_launcher_*", "Codex launcher file"),
    ("cc_trace_*", "Claude Code trace"),
    ("cc_otlp_*", "Claude Code telemetry"),
    ("tmp*", "temp folder"),
];

/// The pattern a name matches and what that pattern is: one of the known
/// families, or one from `temp_patterns` in the config.
pub fn classify(name: &str, more: &[String]) -> Option<(&'static str, &'static str)> {
    if is_uuid(name) {
        return Some(("<UUID>", "UUID-named temp folder"));
    }
    if tests_uuid(name) {
        return Some(("*Tests-<UUID>", "test run folder"));
    }
    if let Some(p) = more.iter().find(|p| glob::matches(p, name)) {
        return Some((interned(p), "on temp_patterns in config.toml"));
    }
    FAMILIES.iter().find(|(p, _)| glob::matches(p, name)).copied()
}

/// The config's patterns live as long as sweep does, one copy each.
fn interned(s: &str) -> &'static str {
    static KNOWN: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
        std::sync::LazyLock::new(Default::default);
    let mut known = KNOWN.lock().unwrap();
    if let Some(k) = known.get(s) {
        return k;
    }
    let k: &'static str = Box::leak(s.to_string().into_boxed_str());
    known.insert(k);
    k
}

#[cfg(test)]
fn family(name: &str) -> Option<&'static str> {
    classify(name, &[]).map(|(_, f)| f)
}

/// `<Something>Tests-<uuid>`, the folders XCTest runs leave behind.
fn tests_uuid(name: &str) -> bool {
    if name.len() <= 36 || !name.is_char_boundary(name.len() - 36) {
        return false;
    }
    let (head, uuid) = name.split_at(name.len() - 36);
    is_uuid(uuid) && head.ends_with("Tests-") && head.len() > "Tests-".len()
}

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![ctx.tmpdir.clone()]
}

/// The item an entry in $TMPDIR is, if its name is one sweep recognises.
pub fn entry(ctx: &Ctx, path: &Path) -> Option<Found> {
    if path.parent()? != ctx.tmpdir.as_path() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    let (pattern, family) = classify(&name, &ctx.cfg.temp_patterns)?;
    Some((Kind::Temp, path.to_path_buf(), name, Target::Temp { pattern, family }))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let entries = match crate::fsattr::read_dir_quick(&ctx.tmpdir) {
        Ok(e) => e,
        Err(e) => {
            sink.note(Kind::Temp, format!("couldn't read {}: {e}", ctx.tmpdir.display()));
            return;
        }
    };
    let mut unknown = 0;
    for e in entries {
        match entry(ctx, &ctx.tmpdir.join(&e.name)) {
            Some((kind, path, label, target)) => sink.item(kind, path, label, target),
            None => unknown += 1,
        }
    }
    if unknown > 0 {
        sink.note(
            Kind::Temp,
            format!(
                "left {} in $TMPDIR alone: names sweep doesn't recognise",
                crate::fmt::count(unknown, "entry", "entries")
            ),
        );
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Safe, "");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    idle(ctx, &mut v, path, &s, ctx.cfg.thresholds.temp_days);
    not_in_use(ctx, &mut v, path);
    for sock in &s.sockets {
        let name = super::rel(path, sock);
        match socket_live(sock) {
            Some(true) => {
                v.checks.push(Check::fail(format!("a process is listening on {name}")));
                v.keep(format!("in use: a process is listening on {name}"));
            }
            Some(false) => v.checks.push(Check::pass(format!("nothing listens on {name}"))),
            None => {
                v.checks.push(Check::fail(format!("couldn't check whether anything listens on {name}")));
                v.keep(format!("couldn't check the socket {name}"));
            }
        }
    }
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge == Badge::Safe {
        v.reason = format!("idle {}", idle_for(ctx, &s));
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{Proc, Procs};
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{set_mtime, write, write_random};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    #[test]
    fn families() {
        assert_eq!(family("remotion-webpack-bundle-abc"), Some("Remotion bundle"));
        assert_eq!(family("remotion-v4.0.355"), Some("Remotion bundle"));
        assert_eq!(family("react-motion-x"), Some("Remotion bundle"));
        assert_eq!(family("0A1B2C3D-0000-4000-8000-00000000ABCD"), Some("UUID-named temp folder"));
        assert_eq!(family("WhatsNewTests-0A1B2C3D-0000-4000-8000-00000000ABCD"), Some("test run folder"));
        assert_eq!(family("Tests-0A1B2C3D-0000-4000-8000-00000000ABCD"), None);
        assert_eq!(family("0A1B2C3D-0000-4000-8000-00000000ABCD.ticket"), None);
        assert_eq!(family("ResultBundle_2026-10-12_10-00-00.xcresult"), Some("Xcode result bundle"));
        assert_eq!(family("cc_otlp_0A1B.pb"), Some("Claude Code telemetry"));
        assert_eq!(family("tmpk3j2h1"), Some("temp folder"));
        assert_eq!(family("other_launcher_invocation_x.sessions"), None);
        assert_eq!(family("strip-packs-0A1B2C3D-0000-4000-8000-00000000ABCD"), None);
        assert_eq!(family("app-tests-12"), None);
        assert_eq!(family("bench-1"), None, "only with temp_patterns");
    }

    #[test]
    fn old_entries_go_new_and_busy_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let t = home.join("T");
        let now = SystemTime::now() + Duration::from_secs(10 * 86_400 + 60);
        write_random(&t.join("remotion-webpack-bundle-old/bundle.js"), 200_000);
        write(&t.join("remotion-webpack-bundle-new/bundle.js"), "x");
        set_mtime(&t.join("remotion-webpack-bundle-new/bundle.js"), now - Duration::from_secs(3600));
        // Old files under a freshly touched folder: the folder's date doesn't count.
        write(&t.join("TemporaryDirectory.AbCdEf/sub/old.txt"), "x");
        set_mtime(&t.join("TemporaryDirectory.AbCdEf/sub"), now);
        set_mtime(&t.join("TemporaryDirectory.AbCdEf"), now);
        // A new file deep under old folders does.
        write(&t.join("bench-1/a/b/c/new.txt"), "x");
        set_mtime(&t.join("bench-1/a/b/c/new.txt"), now - Duration::from_secs(600));
        write(&t.join("cc_trace_0A1B.stop.log"), "trace");
        write(&t.join("tmpbusy/data"), "x");
        write(&t.join("mystery-folder/data"), "x");
        std::fs::create_dir_all(t.join("tmpempty/a")).unwrap();

        let mut ctx = crate::ctx::test_ctx(&home, now);
        ctx.cfg.temp_patterns = vec!["bench-*".into()];
        // Commands tend to spell the folder without macOS's /private prefix.
        let spelled = t.display().to_string();
        ctx.procs = Procs::from_list(vec![Proc {
            pid: 7,
            cmd: format!("python3 work.py --dir {}/tmpbusy", spelled.trim_start_matches("/private")),
            exe: None,
            cwd: None,
        }]);
        let (items, notes) = scan(&ctx, discover);
        assert_eq!(badge(&items, "remotion-webpack-bundle-old"), Badge::Safe);
        let old = find(&items, "remotion-webpack-bundle-old").verdict.clone().unwrap();
        assert_eq!(old.reason, "idle 10 days");
        assert_eq!(
            find(&items, "remotion-webpack-bundle-old").target,
            Target::Temp { pattern: "remotion-webpack-bundle-*", family: "Remotion bundle" }
        );
        assert!(old.bytes.unwrap() >= 200_000);
        assert_eq!(badge(&items, "remotion-webpack-bundle-new"), Badge::Kept);
        assert_eq!(badge(&items, "TemporaryDirectory.AbCdEf"), Badge::Safe);
        let bench = find(&items, "bench-1");
        assert_eq!(bench.target, Target::Temp { pattern: "bench-*", family: "on temp_patterns in config.toml" });
        let bench = bench.verdict.clone().unwrap();
        assert_eq!(bench.badge, Badge::Kept);
        assert_eq!(bench.reason, "in use: written 10 min ago");
        assert_eq!(badge(&items, "cc_trace_0A1B.stop.log"), Badge::Safe);
        assert_eq!(find(&items, "tmpbusy").verdict.as_ref().unwrap().reason, "in use by python3 (pid 7)");
        assert_eq!(badge(&items, "tmpempty"), Badge::Safe);
        assert!(items.iter().all(|i| i.label != "mystery-folder"));
        assert!(notes.iter().any(|n| n.contains("1 entry")), "{notes:?}");
    }

    #[test]
    fn a_folder_with_a_live_socket_stays() {
        // Socket paths are limited to 104 bytes, so keep this one short.
        let home = PathBuf::from(format!("/tmp/sweep-sock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let t = home.join("T");
        std::fs::create_dir_all(t.join("com.google.Chrome.live")).unwrap();
        std::fs::create_dir_all(t.join("com.google.Chrome.dead")).unwrap();
        let _l = std::os::unix::net::UnixListener::bind(t.join("com.google.Chrome.live/SingletonSocket")).unwrap();
        drop(std::os::unix::net::UnixListener::bind(t.join("com.google.Chrome.dead/SingletonSocket")).unwrap());
        let now = SystemTime::now() + Duration::from_secs(5 * 86_400);
        let ctx = crate::ctx::test_ctx(&home, now);
        let (items, _) = scan(&ctx, discover);
        let _ = std::fs::remove_dir_all(&home);
        let live = find(&items, "com.google.Chrome.live").verdict.clone().unwrap();
        assert_eq!(live.badge, Badge::Kept);
        assert_eq!(live.reason, "in use: a process is listening on SingletonSocket");
        let dead = find(&items, "com.google.Chrome.dead").verdict.clone().unwrap();
        assert_eq!(dead.badge, Badge::Safe, "{dead:?}");
    }
}

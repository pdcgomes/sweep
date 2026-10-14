//! Rule 15: versions of VS Code's extensions it no longer uses. Updating an
//! extension leaves the old version's folder behind until VS Code gets round
//! to removing it; extensions.json says which versions are the installed ones.

use super::{Sink, gone, no_mounts, not_in_use, readable, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict};
use crate::walk;
use crate::watch::Change;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Editors of VS Code's family: their extensions folder, name and bundle identifier.
const EDITORS: [(&str, &str, &str); 3] = [
    (".vscode/extensions", "VS Code", "com.microsoft.VSCode"),
    (".vscode-insiders/extensions", "VS Code Insiders", "com.microsoft.VSCodeInsiders"),
    (".vscode-oss/extensions", "VSCodium", "com.vscodium"),
];

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    EDITORS.iter().map(|(dir, _, _)| ctx.home.join(dir)).collect()
}

/// A version installed or removed, or the list of installed ones rewritten.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    let Some(dir) = c.path.parent() else { return false };
    roots(ctx).iter().any(|r| r == dir) && (c.entries || c.path.file_name().is_some_and(|n| n == "extensions.json"))
}

/// The folders extensions.json names, or None if it can't be read.
fn installed(dir: &Path) -> Option<HashSet<String>> {
    let text = std::fs::read_to_string(dir.join("extensions.json")).ok()?;
    let list: serde_json::Value = serde_json::from_str(&text).ok()?;
    let names = list.as_array()?.iter().filter_map(|e| {
        let rel = e.get("relativeLocation").and_then(|r| r.as_str()).map(str::to_string);
        rel.or_else(|| {
            let p = e.get("location")?.get("path")?.as_str()?;
            Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned())
        })
    });
    Some(names.collect())
}

/// `openai.chatgpt-26.930.41038-darwin-arm64` → `openai.chatgpt`.
fn id_of(folder: &str) -> &str {
    folder
        .char_indices()
        .find(|&(i, c)| c == '-' && folder[i + 1..].starts_with(|d: char| d.is_ascii_digit()))
        .map_or(folder, |(i, _)| &folder[..i])
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    for (dir, editor, app) in EDITORS {
        let dir = ctx.home.join(dir);
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let Some(used) = installed(&dir) else {
            sink.note(Kind::Extension, format!("couldn't read {}", ctx.tilde(&dir.join("extensions.json"))));
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || used.contains(&name) || !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            // Only what's an older version of something installed: an extension
            // folder extensions.json doesn't know at all may be one put there by hand.
            if !used.iter().any(|u| id_of(u) == id_of(&name)) {
                continue;
            }
            sink.item(Kind::Extension, e.path(), name, Target::Extension { editor, app });
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, editor: &str, app: &str) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let mut v = Verdict::new(Badge::Safe, format!("{editor} uses a newer version"));
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let newer = path.parent().and_then(installed).and_then(|used| used.into_iter().find(|u| id_of(u) == id_of(&name)));
    match newer {
        Some(n) => v.checks.push(Check::pass(format!("{editor} has {n} installed instead"))),
        None => {
            v.checks.push(Check::fail(format!("{editor} lists it as installed")));
            v.keep(format!("{editor} uses it"));
        }
    }
    if let Some(why) = ctx.procs.app_running(app) {
        v.checks.push(Check::info(format!("{why}: a window opened before the update may still use it")));
        if v.badge == Badge::Safe {
            v.badge = Badge::Review;
            v.reason = format!("{editor} is running: reload its windows first");
        }
    }
    not_in_use(ctx, &mut v, path);
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procs::{Proc, Procs};
    use crate::rules::testing::{badge, find, scan};
    use crate::testutil::{write, write_random};
    use std::time::SystemTime;

    #[test]
    fn extension_ids() {
        assert_eq!(id_of("openai.chatgpt-26.930.41038-darwin-arm64"), "openai.chatgpt");
        assert_eq!(id_of("ms-vscode.cpptools-1.34.4-darwin-arm64"), "ms-vscode.cpptools");
        assert_eq!(id_of("esbenp.prettier-vscode-11.0.0"), "esbenp.prettier-vscode");
    }

    #[test]
    fn versions_vs_code_no_longer_uses() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let ext = home.join(".vscode/extensions");
        for v in ["26.917.1", "26.930.2"] {
            write_random(&ext.join(format!("openai.chatgpt-{v}-darwin-arm64/out/x")), 100_000);
        }
        write_random(&ext.join("ms-python.python-2026.1.0/x"), 100_000);
        write_random(&ext.join("hand-made/x"), 100_000);
        write(
            &ext.join("extensions.json"),
            r#"[{"relativeLocation":"openai.chatgpt-26.930.2-darwin-arm64"},
                {"location":{"path":"/x/ms-python.python-2026.1.0"}}]"#,
        );
        let mut ctx = crate::ctx::test_ctx(&home, SystemTime::now());
        let (items, _) = scan(&ctx, discover);
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["openai.chatgpt-26.917.1-darwin-arm64"]);
        let old = find(&items, "openai.chatgpt-26.917.1-darwin-arm64");
        assert_eq!(old.badge(), Some(Badge::Safe));
        assert!(old.bytes().unwrap() >= 100_000);

        // VS Code running: a window may still have the old version loaded.
        let app = home.join("Applications/Visual Studio Code.app");
        let mut info = plist::Dictionary::new();
        info.insert("CFBundleIdentifier".into(), plist::Value::String("com.microsoft.VSCode".into()));
        std::fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
        plist::Value::Dictionary(info).to_file_xml(app.join("Contents/Info.plist")).unwrap();
        let exe = app.join("Contents/MacOS/Electron");
        ctx.procs = Procs::from_list(vec![Proc { pid: 5, cmd: exe.display().to_string(), exe: Some(exe), cwd: None }]);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(badge(&items, "openai.chatgpt-26.917.1-darwin-arm64"), Badge::Review);
    }
}

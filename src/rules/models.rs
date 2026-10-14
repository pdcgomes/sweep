//! Rule 16: AI models downloaded to run here, for review: each comes back
//! with a download, often a big one. Ollama's models, Chrome's on-device
//! model, and what Hugging Face, LM Studio and PyTorch downloaded.

use super::{Sink, gone, no_mounts, not_in_use, readable, sized};
use crate::ctx::Ctx;
use crate::fsattr;
use crate::model::{Badge, Check, Kind, ModelSource, Target, Verdict};
use crate::walk;
use crate::watch::Change;
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn ollama(ctx: &Ctx) -> PathBuf {
    std::env::var_os("OLLAMA_MODELS").map(PathBuf::from).unwrap_or_else(|| ctx.home.join(".ollama/models"))
}

fn chrome_model(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Application Support/Google/Chrome/OptGuideOnDeviceModel")
}

fn hugging_face(ctx: &Ctx) -> PathBuf {
    ctx.home.join(".cache/huggingface/hub")
}

fn lm_studio(ctx: &Ctx) -> PathBuf {
    ctx.home.join(".lmstudio/models")
}

fn torch(ctx: &Ctx) -> PathBuf {
    ctx.home.join(".cache/torch/hub")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![ollama(ctx).join("manifests"), chrome_model(ctx), hugging_face(ctx), lm_studio(ctx), torch(ctx)]
}

/// A model pulled or removed.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    c.entries && roots(ctx).iter().any(|r| c.path.starts_with(r) && c.path != *r)
}

/// Each Ollama model's manifest, its name, and the digests of its blobs.
fn manifests(root: &Path) -> Vec<(PathBuf, String, Vec<String>)> {
    let mut out = Vec::new();
    let mut stack = vec![(root.join("manifests"), Vec::<String>::new())];
    while let Some((dir, parts)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let mut p = parts.clone();
            p.push(name);
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((e.path(), p));
            } else if p.len() == 4 {
                // registry / namespace / model / tag
                let digests = blob_digests(&e.path());
                let model = match (p[0].as_str(), p[1].as_str()) {
                    ("registry.ollama.ai", "library") => format!("{}:{}", p[2], p[3]),
                    ("registry.ollama.ai", ns) => format!("{ns}/{}:{}", p[2], p[3]),
                    (reg, ns) => format!("{reg}/{ns}/{}:{}", p[2], p[3]),
                };
                out.push((e.path(), model, digests));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

fn blob_digests(manifest: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let config = v.get("config").into_iter();
    let layers = v.get("layers").and_then(|l| l.as_array()).into_iter().flatten();
    config.chain(layers).filter_map(|l| l.get("digest")?.as_str().map(str::to_string)).collect()
}

fn blob(root: &Path, digest: &str) -> PathBuf {
    root.join("blobs").join(digest.replace(':', "-"))
}

/// The folders of a cache with one folder per model, as items.
fn per_folder(sink: &dyn Sink, dir: &Path, label: impl Fn(&str) -> Option<String>, src: ModelSource) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(l) = label(&name).filter(|_| e.file_type().is_ok_and(|t| t.is_dir())) {
            sink.item(Kind::Model, e.path(), l, Target::Model(src.clone()));
        }
    }
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    for (path, name, blobs) in manifests(&ollama(ctx)) {
        let label = format!("Ollama · {name}");
        sink.item(Kind::Model, path, label, Target::Model(ModelSource::Ollama { name, blobs }));
    }
    let chrome = ModelSource::Download {
        app: "com.google.Chrome",
        again: "Chrome downloads it again when an on-device AI feature needs it",
    };
    if chrome_model(ctx).is_dir() {
        sink.item(Kind::Model, chrome_model(ctx), "Chrome · on-device model".into(), Target::Model(chrome));
    }
    let hf = ModelSource::Download { app: "", again: "Hugging Face downloads it again when it's next used" };
    per_folder(
        sink,
        &hugging_face(ctx),
        |n| n.strip_prefix("models--").map(|m| format!("Hugging Face · {}", m.replacen("--", "/", 1))),
        hf,
    );
    let lms = ModelSource::Download { app: "ai.elementlabs.lmstudio", again: "LM Studio downloads it again" };
    if let Ok(publishers) = std::fs::read_dir(lm_studio(ctx)) {
        for p in publishers.flatten() {
            let publisher = p.file_name().to_string_lossy().into_owned();
            per_folder(sink, &p.path(), |m| Some(format!("LM Studio · {publisher}/{m}")), lms.clone());
        }
    }
    if torch(ctx).is_dir() {
        let src = ModelSource::Download { app: "", again: "PyTorch downloads them again when they're next used" };
        sink.item(Kind::Model, torch(ctx), "PyTorch hub downloads".into(), Target::Model(src));
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, src: &ModelSource) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    match src {
        ModelSource::Ollama { name, blobs } => evaluate_ollama(ctx, path, name, blobs),
        ModelSource::Download { app, again } => {
            let mut v = Verdict::new(Badge::Review, *again);
            let s = walk::measure(path, &ctx.walk_opts());
            sized(&mut v, &s);
            if let Some(why) = (!app.is_empty()).then(|| ctx.procs.app_running(app)).flatten() {
                v.checks.push(Check::fail(why.clone()));
                v.keep(why);
            }
            not_in_use(ctx, &mut v, path);
            no_mounts(&mut v, &s);
            readable(&mut v, &s);
            v
        }
    }
}

/// What deleting an Ollama model frees: the blobs no other model uses.
fn evaluate_ollama(ctx: &Ctx, manifest: &Path, name: &str, blobs: &[String]) -> Verdict {
    let root = ollama(ctx);
    let mut v = Verdict::new(Badge::Review, format!("ollama pull {name} downloads it again"));
    let shared: HashSet<String> =
        manifests(&root).into_iter().filter(|(p, _, _)| p != manifest).flat_map(|(_, _, d)| d).collect();
    let (mut freed, mut total) = (0, 0);
    for d in blobs {
        let Ok(a) = fsattr::attrs(&blob(&root, d)) else { continue };
        total += a.freed();
        if !shared.contains(d) {
            freed += a.freed();
        }
    }
    v.bytes = Some(freed);
    v.du = Some(total);
    v.last_write = fsattr::attrs(manifest).ok().map(|a| a.last_write());
    if let Some(t) = v.last_write {
        v.checks.push(Check::info(format!("pulled {}", ctx.ago(t))));
    }
    if freed < total {
        v.checks.push(Check::info(format!(
            "{} of it is shared with other models and stays",
            crate::fmt::bytes(total - freed)
        )));
    }
    for d in blobs {
        let b = blob(&root, d);
        if let Some(p) = ctx.procs.using(&b) {
            let who = p.describe();
            v.checks.push(Check::fail(format!("loaded by {who}")));
            v.keep(format!("loaded by {who}"));
        }
    }
    if v.badge != Badge::Kept {
        v.checks.push(Check::pass("no running process has it loaded"));
    }
    v
}

/// Removes an Ollama model as `ollama rm` does: its manifest, and the blobs
/// no remaining model uses.
pub fn delete(ctx: &Ctx, manifest: &Path) -> Result<String> {
    let root = ollama(ctx);
    let own = blob_digests(manifest);
    std::fs::remove_file(manifest)?;
    let still: HashSet<String> = manifests(&root).into_iter().flat_map(|(_, _, d)| d).collect();
    let mut removed = 0;
    for d in own.iter().filter(|d| !still.contains(*d)) {
        if std::fs::remove_file(blob(&root, d)).is_ok() {
            removed += 1;
        }
    }
    // Empty model and namespace folders go too.
    for dir in manifest.ancestors().skip(1).take(3) {
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
    }
    Ok(format!("{} removed", crate::fmt::count(removed, "blob", "blobs")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::testing::{find, scan};
    use crate::testutil::{write, write_random};
    use std::time::SystemTime;

    fn manifest(root: &Path, model: &str, tag: &str, digests: &[&str]) {
        let layers: Vec<String> = digests.iter().map(|d| format!(r#"{{"digest":"{d}","size":1}}"#)).collect();
        let path = root.join("manifests/registry.ollama.ai/library").join(model).join(tag);
        write(&path, &format!(r#"{{"config":{{"digest":"sha256:c-{model}"}},"layers":[{}]}}"#, layers.join(",")));
        for d in digests.iter().copied().chain([format!("sha256:c-{model}").as_str()]) {
            write_random(&blob(root, d), 100_000);
        }
    }

    #[test]
    fn ollama_models_free_only_what_they_alone_use() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let root = home.join(".ollama/models");
        manifest(&root, "llama3.2", "latest", &["sha256:aaa", "sha256:shared"]);
        manifest(&root, "mistral", "7b", &["sha256:bbb", "sha256:shared"]);
        write_random(&home.join(".cache/huggingface/hub/models--openai--whisper-small/blobs/x"), 50_000);
        let ctx = crate::ctx::test_ctx(&home, SystemTime::now());
        let (items, _) = scan(&ctx, discover);
        let llama = find(&items, "Ollama · llama3.2:latest");
        let v = llama.verdict.clone().unwrap();
        assert_eq!((v.badge, v.reason.as_str()), (Badge::Review, "ollama pull llama3.2:latest downloads it again"));
        let one = fsattr::attrs(&blob(&root, "sha256:aaa")).unwrap().freed();
        assert_eq!(v.bytes, Some(2 * one), "its own layer and config, not the shared one");
        assert_eq!(find(&items, "Hugging Face · openai/whisper-small").badge(), Some(Badge::Review));

        delete(&ctx, &llama.path).unwrap();
        assert!(!blob(&root, "sha256:aaa").exists());
        assert!(blob(&root, "sha256:shared").exists(), "mistral still uses it");
        assert!(!root.join("manifests/registry.ollama.ai/library/llama3.2").exists());
        let (items, _) = scan(&ctx, discover);
        assert_eq!(items.iter().filter(|i| i.label.starts_with("Ollama")).count(), 1);
    }
}

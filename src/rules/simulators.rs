//! Rule 5: simulators. Devices from `xcrun simctl list devices -j`
//! (unavailable ones are Safe, the rest Review) and runtimes from
//! `xcrun simctl runtime list -j` (Review once unused for three months).

use super::{Sink, TOOL_LOCK, gone, not_in_use, readable, run_tool_timeout, sized};
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, SimDeviceInfo, SimRuntimeInfo, Target, Verdict};
use crate::walk;
use crate::watch::Change;
use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

const TIMEOUT: Duration = Duration::from_secs(120);

fn simctl(args: &[&str]) -> Command {
    let mut c = Command::new("/usr/bin/xcrun");
    c.arg("simctl").args(args);
    c
}

fn devices_dir(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Developer/CoreSimulator/Devices")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![devices_dir(ctx)]
}

/// Simulators come and go in Devices, and each one's state is written to the
/// device.plist at its top.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    let d = devices_dir(ctx);
    let Some(dir) = c.path.parent() else { return false };
    (c.entries && dir == d) || (c.path.file_name().is_some_and(|n| n == "device.plist") && dir.parent() == Some(&d))
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    if !Path::new("/usr/bin/xcrun").exists() {
        return;
    }
    match run_tool_timeout(&mut simctl(&["list", "devices", "-j"]), TIMEOUT) {
        Ok(json) => {
            for (path, label, info) in parse_devices(ctx, &json) {
                sink.item(Kind::SimDevice, path, label, Target::SimDevice(info));
            }
        }
        Err(e) => sink.note(Kind::SimDevice, format!("couldn't list simulators: {e}")),
    }
    match run_tool_timeout(&mut simctl(&["runtime", "list", "-j"]), TIMEOUT) {
        Ok(json) => {
            for (path, label, info) in parse_runtimes(&json) {
                sink.item(Kind::SimRuntime, path, label, Target::SimRuntime(info));
            }
        }
        Err(e) => sink.note(Kind::SimRuntime, format!("couldn't list simulator runtimes: {e}")),
    }
}

/// `com.apple.CoreSimulator.SimRuntime.iOS-26-3` → `iOS 26.3`.
fn runtime_name(id: &str) -> String {
    let short = id.rsplit('.').next().unwrap_or(id);
    match short.split_once('-') {
        Some((platform, version)) => format!("{platform} {}", version.replace('-', ".")),
        None => short.to_string(),
    }
}

pub fn parse_devices(ctx: &Ctx, json: &str) -> Vec<(PathBuf, String, SimDeviceInfo)> {
    let mut out = Vec::new();
    let Ok(v) = serde_json::from_str::<Value>(json) else { return out };
    let Some(runtimes) = v.get("devices").and_then(Value::as_object) else { return out };
    for (runtime_id, devices) in runtimes {
        let runtime = runtime_name(runtime_id);
        for d in devices.as_array().into_iter().flatten() {
            let s = |k: &str| d.get(k).and_then(Value::as_str).map(String::from);
            let Some(udid) = s("udid") else { continue };
            let name = s("name").unwrap_or_else(|| udid.clone());
            let path = s("dataPath")
                .and_then(|p| Path::new(&p).parent().map(Path::to_path_buf))
                .unwrap_or_else(|| devices_dir(ctx).join(&udid));
            let info = SimDeviceInfo {
                udid,
                name: name.clone(),
                runtime: runtime.clone(),
                state: s("state").unwrap_or_default(),
                available: d.get("isAvailable").and_then(Value::as_bool).unwrap_or(true),
                error: s("availabilityError"),
            };
            out.push((path, format!("{name} · {runtime}"), info));
        }
    }
    out
}

pub fn parse_runtimes(json: &str) -> Vec<(PathBuf, String, SimRuntimeInfo)> {
    let mut out = Vec::new();
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(json) else { return out };
    for (key, r) in map {
        let s = |k: &str| r.get(k).and_then(Value::as_str).map(String::from);
        let identifier = s("identifier").unwrap_or(key);
        let platform = s("runtimeIdentifier").map(|id| runtime_name(&id)).unwrap_or_default();
        let platform = platform.split(' ').next().unwrap_or("").to_string();
        let version = s("version").unwrap_or_default();
        let build = s("build").unwrap_or_default();
        let name = format!("{platform} {version}").trim().to_string();
        let last_used =
            s("lastUsedAt").and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()).map(SystemTime::from);
        let path = s("path").or_else(|| s("mountPath")).map(PathBuf::from).unwrap_or_default();
        let info = SimRuntimeInfo {
            identifier,
            name: name.clone(),
            build: build.clone(),
            size_bytes: r.get("sizeBytes").and_then(Value::as_u64),
            last_used,
            deletable: r.get("deletable").and_then(Value::as_bool).unwrap_or(true),
            state: s("state").unwrap_or_default(),
        };
        out.push((path, format!("{name} ({build})"), info));
    }
    out
}

pub fn evaluate_device(ctx: &Ctx, path: &Path, d: &SimDeviceInfo) -> Verdict {
    let mut v = Verdict::new(Badge::Review, "");
    if gone(path).is_none() {
        let s = walk::measure(path, &ctx.walk_opts());
        sized(&mut v, &s);
        readable(&mut v, &s);
    } else {
        v.bytes = Some(0);
    }
    if d.state.eq_ignore_ascii_case("booted") {
        v.checks.push(Check::fail("booted"));
        v.keep("booted");
    } else {
        v.checks.push(Check::pass(format!("not running ({})", d.state.to_lowercase())));
    }
    not_in_use(ctx, &mut v, path);
    if d.available {
        v.checks.push(Check::info(format!("{} is installed", d.runtime)));
        if v.badge != Badge::Kept {
            v.badge = Badge::Review;
            v.reason = "a simulator with its apps and data".into();
        }
    } else {
        let why = d.error.clone().unwrap_or_else(|| "its runtime is missing".into());
        v.checks.push(Check::pass(format!("unavailable: {why}")));
        if v.badge != Badge::Kept {
            v.badge = Badge::Safe;
            v.reason = "unavailable: it can't boot any more".into();
        }
    }
    v
}

pub fn evaluate_runtime(ctx: &Ctx, r: &SimRuntimeInfo) -> Verdict {
    let mut v = Verdict::new(Badge::Review, "");
    v.bytes = r.size_bytes;
    v.last_write = r.last_used;
    if !r.deletable {
        v.checks.push(Check::fail("simctl says it can't be deleted"));
        v.keep("simctl says it can't be deleted");
        return v;
    }
    let days = ctx.cfg.thresholds.runtime_unused_days;
    match r.last_used {
        Some(t) if ctx.age_days(t) < days => {
            let ago = ctx.ago(t);
            v.checks.push(Check::fail(format!("used {ago}")));
            v.keep(format!("used {ago}"));
        }
        Some(t) => {
            v.checks.push(Check::pass(format!("last used {}", ctx.ago(t))));
            v.reason = format!(
                "unused for {}; Xcode downloads it again",
                crate::fmt::age(ctx.now.duration_since(t).unwrap_or_default())
            );
        }
        None => {
            v.checks.push(Check::pass("never used"));
            v.reason = "never used; Xcode downloads it again".into();
        }
    }
    v
}

pub fn delete_device(d: &SimDeviceInfo) -> Result<String> {
    let _g = TOOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    run_tool_timeout(&mut simctl(&["delete", &d.udid]), TIMEOUT)?;
    Ok(String::new())
}

pub fn delete_runtime(r: &SimRuntimeInfo) -> Result<String> {
    let _g = TOOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    run_tool_timeout(&mut simctl(&["runtime", "delete", &r.identifier]), TIMEOUT)?;
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Item;
    use crate::rules::evaluate;

    const DEVICES: &str = r#"{
      "devices" : {
        "com.apple.CoreSimulator.SimRuntime.iOS-17-0" : [
          { "udid" : "AAAA", "isAvailable" : false, "availabilityError" : "runtime profile not found",
            "state" : "Shutdown", "name" : "iPhone 15", "dataPath" : "/nonexistent/Devices/AAAA/data" }
        ],
        "com.apple.CoreSimulator.SimRuntime.iOS-26-5" : [
          { "udid" : "BBBB", "isAvailable" : true, "state" : "Shutdown", "name" : "iPhone 17 Pro" },
          { "udid" : "CCCC", "isAvailable" : true, "state" : "Booted", "name" : "iPad Air" }
        ]
      }
    }"#;

    const RUNTIMES: &str = r#"{
      "6A5C66CF-9397-4198-BC7E-72D62E55ED85" : {
        "build" : "23D8133", "deletable" : true, "identifier" : "6A5C66CF-9397-4198-BC7E-72D62E55ED85",
        "lastUsedAt" : "2026-10-08T12:23:11Z", "runtimeIdentifier" : "com.apple.CoreSimulator.SimRuntime.iOS-26-3",
        "sizeBytes" : 8393503393, "state" : "Ready", "version" : "26.3.1", "path" : "/x/094-26194-058.dmg"
      },
      "C41D6466-1916-4BA0-A420-C0364B4AACB7" : {
        "build" : "23A343", "deletable" : true, "identifier" : "C41D6466-1916-4BA0-A420-C0364B4AACB7",
        "lastUsedAt" : "2026-05-07T15:39:44Z", "runtimeIdentifier" : "com.apple.CoreSimulator.SimRuntime.iOS-26-0",
        "sizeBytes" : 8014773567, "state" : "Ready", "version" : "26.0"
      },
      "D0000000-0000-0000-0000-000000000000" : {
        "build" : "21A1", "deletable" : false, "identifier" : "D0000000-0000-0000-0000-000000000000",
        "runtimeIdentifier" : "com.apple.CoreSimulator.SimRuntime.watchOS-10-0", "sizeBytes" : 1, "version" : "10.0"
      }
    }"#;

    fn check(ctx: &Ctx, path: PathBuf, label: String, kind: Kind, target: Target) -> Item {
        let mut item = Item { id: 0, kind, path, label, target, verdict: None };
        item.verdict = Some(evaluate(ctx, &item));
        item
    }

    #[test]
    fn devices() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::ctx::test_ctx(dir.path(), SystemTime::now());
        let devices = parse_devices(&ctx, DEVICES);
        assert_eq!(devices.len(), 3);
        let items: Vec<Item> =
            devices.into_iter().map(|(p, l, d)| check(&ctx, p, l, Kind::SimDevice, Target::SimDevice(d))).collect();
        let by = |l: &str| items.iter().find(|i| i.label == l).unwrap().verdict.clone().unwrap();
        assert_eq!(by("iPhone 15 · iOS 17.0").badge, Badge::Safe);
        assert_eq!(items[0].path, PathBuf::from("/nonexistent/Devices/AAAA"));
        assert_eq!(by("iPhone 17 Pro · iOS 26.5").badge, Badge::Review);
        assert_eq!(by("iPad Air · iOS 26.5").reason, "booted");
    }

    #[test]
    fn runtimes() {
        let dir = tempfile::tempdir().unwrap();
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T16:00:00Z").unwrap().into();
        let ctx = crate::ctx::test_ctx(dir.path(), now);
        let items: Vec<Item> = parse_runtimes(RUNTIMES)
            .into_iter()
            .map(|(p, l, r)| check(&ctx, p, l, Kind::SimRuntime, Target::SimRuntime(r)))
            .collect();
        let by = |l: &str| items.iter().find(|i| i.label == l).unwrap().verdict.clone().unwrap();
        let recent = by("iOS 26.3.1 (23D8133)");
        assert_eq!(recent.badge, Badge::Kept);
        assert_eq!(recent.reason, "used 5 days ago");
        assert_eq!(recent.bytes, Some(8393503393));
        let old = by("iOS 26.0 (23A343)");
        assert_eq!(old.badge, Badge::Review);
        assert_eq!(old.reason, "unused for 5 months; Xcode downloads it again");
        assert_eq!(by("watchOS 10.0 (21A1)").badge, Badge::Kept);
    }
}

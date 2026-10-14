//! Rule 14: what Xcode keeps beside DerivedData. Symbols it copied from a
//! device (copied again when the device next connects), its downloaded
//! documentation, built products, and the devices behind SwiftUI previews
//! and tests go once idle; archives, which crash reports and resubmitting
//! need, are for review.

use super::{Sink, gone, idle, idle_for, no_mounts, not_in_use, readable, sized};
use crate::config::Thresholds;
use crate::ctx::Ctx;
use crate::model::{Badge, Check, Kind, Target, Verdict, XcodeFile};
use crate::walk;
use crate::watch::Change;
use std::path::{Path, PathBuf};

const XCODE: &str = "com.apple.dt.Xcode";
const PLATFORMS: [&str; 5] = ["iOS", "watchOS", "tvOS", "visionOS", "xrOS"];

fn xcode_dir(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Developer/Xcode")
}

fn test_devices(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/Developer/XCTestDevices")
}

pub fn roots(ctx: &Ctx) -> Vec<PathBuf> {
    vec![xcode_dir(ctx), test_devices(ctx)]
}

/// A device connected for the first time, or an archive made.
pub fn lists(ctx: &Ctx, c: &Change) -> bool {
    let Some(dir) = c.path.parent() else { return false };
    c.entries
        && (dir.file_name().is_some_and(|n| n.to_string_lossy().ends_with(" DeviceSupport"))
            || dir.parent() == Some(xcode_dir(ctx).join("Archives").as_path()))
}

/// The days each kind of file has to sit unwritten; archives aren't judged by age.
pub fn idle_days(t: &Thresholds, f: &XcodeFile) -> Option<f64> {
    Some(match f {
        XcodeFile::DeviceSupport { newer: Some(_) } => 2.0,
        XcodeFile::DeviceSupport { newer: None } => t.device_support_days,
        XcodeFile::Documentation => 30.0,
        XcodeFile::Products | XcodeFile::Previews => 7.0,
        XcodeFile::TestDevices => 2.0,
        XcodeFile::Archive => return None,
    })
}

/// While Xcode runs it may be reading its documentation, previews and products.
pub fn kept_without_measuring(ctx: &Ctx, f: &XcodeFile) -> Option<String> {
    match f {
        XcodeFile::Documentation | XcodeFile::Previews | XcodeFile::Products => {
            ctx.procs.app_running(XCODE).map(|_| "Xcode is running".to_string())
        }
        _ => None,
    }
}

/// `iPhone18,1 27.0.1 (24A446)` → the device and its OS version, as numbers
/// to compare; older Xcodes left out the device.
fn device_and_version(name: &str) -> (String, Vec<u32>) {
    let head = name.split(" (").next().unwrap_or(name);
    let (device, version) = match head.rsplit_once(' ') {
        Some((d, v)) => (d.to_string(), v),
        None => (String::new(), head),
    };
    (device, version.split('.').map(|p| p.parse().unwrap_or(0)).collect())
}

pub fn discover(ctx: &Ctx, sink: &dyn Sink) {
    let x = xcode_dir(ctx);
    for platform in PLATFORMS {
        let dir = x.join(format!("{platform} DeviceSupport"));
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let entries: Vec<(PathBuf, String)> = rd
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .map(|e| (e.path(), e.file_name().to_string_lossy().into_owned()))
            .collect();
        for (path, name) in &entries {
            let (device, version) = device_and_version(name);
            let newer = entries
                .iter()
                .filter(|(_, other)| {
                    let (d, v) = device_and_version(other);
                    d == device && v > version
                })
                .max_by_key(|(_, other)| device_and_version(other).1)
                .map(|(_, other)| other.clone());
            let label = format!("{platform} · {name}");
            sink.item(Kind::XcodeSupport, path.clone(), label, Target::Xcode(XcodeFile::DeviceSupport { newer }));
        }
    }
    let single = [
        (x.join("DocumentationCache"), "documentation cache", XcodeFile::Documentation),
        (x.join("DocumentationIndex"), "documentation index", XcodeFile::Documentation),
        (x.join("Products"), "built products", XcodeFile::Products),
        (x.join("UserData/Previews"), "SwiftUI preview devices", XcodeFile::Previews),
        (test_devices(ctx), "test devices", XcodeFile::TestDevices),
    ];
    for (path, label, f) in single {
        if path.is_dir() {
            sink.item(Kind::XcodeSupport, path, label.into(), Target::Xcode(f));
        }
    }
    let Ok(days) = std::fs::read_dir(x.join("Archives")) else { return };
    for day in days.flatten() {
        let Ok(archives) = std::fs::read_dir(day.path()) else { continue };
        for a in archives.flatten() {
            let name = a.file_name().to_string_lossy().into_owned();
            if let Some(app) = name.strip_suffix(".xcarchive") {
                let label = format!("archive · {app} · {}", day.file_name().to_string_lossy());
                sink.item(Kind::XcodeSupport, a.path(), label, Target::Xcode(XcodeFile::Archive));
            }
        }
    }
}

pub fn evaluate(ctx: &Ctx, path: &Path, f: &XcodeFile) -> Verdict {
    if let Some(v) = gone(path) {
        return v;
    }
    let base = if *f == XcodeFile::Archive { Badge::Review } else { Badge::Safe };
    let mut v = Verdict::new(base, "");
    let s = walk::measure(path, &ctx.walk_opts());
    sized(&mut v, &s);
    if let Some(why) = kept_without_measuring(ctx, f) {
        v.checks.push(Check::fail(why.clone()));
        v.keep(why);
    }
    if let Some(days) = idle_days(&ctx.cfg.thresholds, f) {
        idle(ctx, &mut v, path, &s, days);
    }
    not_in_use(ctx, &mut v, path);
    no_mounts(&mut v, &s);
    readable(&mut v, &s);
    if v.badge != Badge::Kept {
        let idle = idle_for(ctx, &s);
        v.reason = match f {
            XcodeFile::DeviceSupport { newer: Some(n) } => {
                format!("superseded by {n}; Xcode copies symbols again from a device that needs them")
            }
            XcodeFile::DeviceSupport { newer: None } => {
                format!("device not connected for {idle}; Xcode copies it again when it is")
            }
            XcodeFile::Documentation => format!("unused for {idle}; Xcode downloads it again"),
            XcodeFile::Products | XcodeFile::Previews | XcodeFile::TestDevices => {
                format!("idle {idle}; Xcode makes it again")
            }
            XcodeFile::Archive => {
                format!("made {idle} ago; crash reports from this build need it to be symbolicated")
            }
        };
        // An empty folder of test devices isn't worth a row.
        if s.freed < 1_000_000 && *f != XcodeFile::Archive {
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
    use crate::testutil::{set_mtime, write_random};
    use std::time::{Duration, SystemTime};

    #[test]
    fn versions_and_build_order() {
        assert_eq!(device_and_version("iPhone18,1 27.0.1 (24A446)"), ("iPhone18,1".into(), vec![27, 0, 1]));
        assert_eq!(device_and_version("17.2 (21C62)"), (String::new(), vec![17, 2]));
        assert!(device_and_version("iPhone18,1 27.0.10 (x)").1 > device_and_version("iPhone18,1 27.0.9 (x)").1);
    }

    #[test]
    fn device_symbols_documentation_and_archives() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let x = home.join("Library/Developer/Xcode");
        let ds = x.join("iOS DeviceSupport");
        write_random(&ds.join("iPhone18,1 26.4 (23E1)/Symbols/a"), 100_000);
        write_random(&ds.join("iPhone18,1 27.0.1 (24A446)/Symbols/a"), 100_000);
        write_random(&ds.join("iPad16,3 26.4 (23E1)/Symbols/a"), 100_000);
        write_random(&x.join("DocumentationCache/v1/d"), 100_000);
        write_random(&x.join("Archives/2026-04-17/Example 17-04-2026, 10.12.xcarchive/Info.plist"), 1_000);
        write_random(&home.join("Library/Developer/XCTestDevices/ABC/data/x"), 100_000);
        let now = SystemTime::now() + Duration::from_secs(40 * 86_400);
        set_mtime(&ds.join("iPhone18,1 27.0.1 (24A446)/Symbols/a"), now - Duration::from_secs(86_400));
        let mut ctx = crate::ctx::test_ctx(&home, now);
        let (items, _) = scan(&ctx, discover);
        let old = find(&items, "iOS · iPhone18,1 26.4 (23E1)").verdict.clone().unwrap();
        assert_eq!(old.badge, Badge::Safe);
        assert!(old.reason.starts_with("superseded by iPhone18,1 27.0.1 (24A446)"), "{}", old.reason);
        assert_eq!(badge(&items, "iOS · iPhone18,1 27.0.1 (24A446)"), Badge::Kept, "connected yesterday");
        let ipad = find(&items, "iOS · iPad16,3 26.4 (23E1)").verdict.clone().unwrap();
        assert_eq!(ipad.badge, Badge::Kept, "the newest for its device, connected 40 days ago: {}", ipad.reason);
        assert_eq!(badge(&items, "documentation cache"), Badge::Safe);
        assert_eq!(badge(&items, "test devices"), Badge::Safe);
        let archive = find(&items, "archive · Example 17-04-2026, 10.12 · 2026-04-17").verdict.clone().unwrap();
        assert_eq!(archive.badge, Badge::Review);

        ctx.procs = Procs::from_list(vec![Proc {
            pid: 3,
            cmd: "/Applications/Xcode.app/Contents/MacOS/Xcode".into(),
            exe: None,
            cwd: None,
        }]);
        let (items, _) = scan(&ctx, discover);
        assert_eq!(find(&items, "documentation cache").verdict.as_ref().unwrap().reason, "Xcode is running");
    }
}

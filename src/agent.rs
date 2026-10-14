//! The weekly launchd agent: it scans and posts a notification with what it
//! found. It never deletes anything.

use crate::ctx::Ctx;
use crate::disks::{Disk, Severity};
use crate::fmt;
use crate::model::Item;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const LABEL: &str = "local.sweep.weekly";

fn plist_path(ctx: &Ctx) -> PathBuf {
    ctx.home.join("Library/LaunchAgents").join(format!("{LABEL}.plist"))
}

fn domain() -> String {
    format!("gui/{}", unsafe { libc::getuid() })
}

fn definition(exe: &std::path::Path, logs: &std::path::Path, weekday: u8, hour: u8) -> plist::Value {
    let mut interval = plist::Dictionary::new();
    interval.insert("Weekday".into(), (weekday as i64).into());
    interval.insert("Hour".into(), (hour as i64).into());
    interval.insert("Minute".into(), 0i64.into());
    let mut env = plist::Dictionary::new();
    env.insert("PATH".into(), "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".into());
    let mut d = plist::Dictionary::new();
    d.insert("Label".into(), LABEL.into());
    d.insert("ProgramArguments".into(), plist::Value::Array(vec![exe.display().to_string().into(), "notify".into()]));
    d.insert("StartCalendarInterval".into(), plist::Value::Dictionary(interval));
    d.insert("EnvironmentVariables".into(), plist::Value::Dictionary(env));
    d.insert("StandardOutPath".into(), logs.join("agent.log").display().to_string().into());
    d.insert("StandardErrorPath".into(), logs.join("agent.log").display().to_string().into());
    d.insert("ProcessType".into(), "Background".into());
    d.insert("LowPriorityIO".into(), true.into());
    d.insert("Nice".into(), 10i64.into());
    plist::Value::Dictionary(d)
}

pub fn install(ctx: &Ctx, weekday: u8, hour: u8) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let logs = ctx.home.join("Library/Logs/sweep");
    std::fs::create_dir_all(&logs)?;
    let path = plist_path(ctx);
    std::fs::create_dir_all(path.parent().unwrap())?;
    definition(&exe, &logs, weekday, hour).to_file_xml(&path).with_context(|| format!("writing {}", path.display()))?;

    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).output();
    let out = Command::new("/bin/launchctl").args(["bootstrap", &domain()]).arg(&path).output()?;
    if !out.status.success() {
        anyhow::bail!(
            "wrote {} but launchctl couldn't load it: {}\nRun this in Terminal: launchctl bootstrap {} {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim(),
            domain(),
            path.display()
        );
    }
    let day = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"][weekday as usize % 7];
    println!("Installed {LABEL}: every {day} at {hour:02}:00 sweep scans and posts a notification. It never deletes.");
    println!("Agent: {}", path.display());
    Ok(())
}

pub fn uninstall(ctx: &Ctx) -> Result<()> {
    let _ = Command::new("/bin/launchctl").args(["bootout", &format!("{}/{LABEL}", domain())]).output();
    let path = plist_path(ctx);
    if path.exists() {
        std::fs::remove_file(&path)?;
        println!("Removed {}", path.display());
    } else {
        println!("No agent installed.");
    }
    Ok(())
}

pub fn status(ctx: &Ctx) {
    let path = plist_path(ctx);
    let loaded = Command::new("/bin/launchctl")
        .args(["print", &format!("{}/{LABEL}", domain())])
        .output()
        .is_ok_and(|o| o.status.success());
    println!("{}: {}", path.display(), if path.exists() { "installed" } else { "not installed" });
    println!("launchd: {}", if loaded { "loaded" } else { "not loaded" });
}

/// The startup disk's free space, any other disk that's running out, then
/// what sweep found.
pub fn message(items: &[Item], disks: &[Disk]) -> String {
    let mut m = String::new();
    for d in disks.iter().filter(|d| d.startup || d.severity() != Severity::Ok) {
        let state = match d.severity() {
            Severity::Ok => String::new(),
            s => format!(" is {}", s.label()),
        };
        m += &format!("{}{state}: {} free. ", d.title(), fmt::bytes(d.free()));
    }
    let t = crate::report::totals(items);
    let found = t.safe_bytes + t.review_bytes;
    if found == 0 && t.unknown_size == 0 {
        return m + "Nothing to clean up.";
    }
    m += &format!(
        "Found {}: {} safe to delete, {} to review.",
        fmt::bytes(found),
        fmt::bytes(t.safe_bytes),
        fmt::bytes(t.review_bytes)
    );
    if t.unknown_size > 0 {
        m += &format!(" Plus {} of unknown size.", fmt::count(t.unknown_size, "snapshot", "snapshots"));
    }
    m + " Run sweep to choose."
}

pub fn notify(text: &str) -> Result<()> {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"sweep\"", esc(text));
    let out = Command::new("/usr/bin/osascript").args(["-e", &script]).output()?;
    if !out.status.success() {
        anyhow::bail!("osascript: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_agent_runs_notify_weekly_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.plist");
        definition(Path::new("/Users/u/.cargo/bin/sweep"), Path::new("/Users/u/Library/Logs/sweep"), 1, 10)
            .to_file_xml(&file)
            .unwrap();
        let lint = Command::new("/usr/bin/plutil").arg("-lint").arg(&file).output().unwrap();
        assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        let v = plist::Value::from_file(&file).unwrap();
        let d = v.as_dictionary().unwrap();
        let args: Vec<&str> =
            d["ProgramArguments"].as_array().unwrap().iter().map(|a| a.as_string().unwrap()).collect();
        assert_eq!(args, ["/Users/u/.cargo/bin/sweep", "notify"]);
        let when = d["StartCalendarInterval"].as_dictionary().unwrap();
        assert_eq!(when["Weekday"].as_signed_integer(), Some(1));
        assert_eq!(when["Hour"].as_signed_integer(), Some(10));
        assert_eq!(d["Label"].as_string(), Some(LABEL));
    }

    #[test]
    fn the_message_totals_what_can_go() {
        use crate::disks::Store;
        use crate::model::{Badge, Item, Kind, Target, Verdict};
        let item = |badge, bytes| {
            let mut v = Verdict::new(badge, "");
            v.bytes = Some(bytes);
            Item {
                id: 0,
                kind: Kind::Temp,
                path: "/x".into(),
                label: "x".into(),
                target: Target::Temp { pattern: "tmp*", family: "" },
                verdict: Some(v),
            }
        };
        let items =
            [item(Badge::Safe, 2_000_000_000), item(Badge::Review, 500_000_000), item(Badge::Kept, 90_000_000_000)];
        assert_eq!(message(&items, &[]), "Found 2.5 GB: 2.0 GB safe to delete, 500 MB to review. Run sweep to choose.");
        assert_eq!(message(&[], &[]), "Nothing to clean up.");

        let disk = |startup: bool, free: u64| Disk {
            id: "disk0".into(),
            model: "SSD".into(),
            internal: startup,
            startup,
            size: 1_000_000_000_000,
            volumes: vec![if startup { "Macintosh HD" } else { "Backup" }.into()],
            stores: vec![Store { id: "disk3".into(), free, probe: None }],
        };
        let disks = [disk(true, 12_000_000_000), disk(false, 600_000_000_000)];
        assert_eq!(
            message(&items, &disks),
            "Internal · Macintosh HD is CRITICAL: 12 GB free. Found 2.5 GB: 2.0 GB safe to delete, 500 MB to review. Run sweep to choose."
        );
    }
}

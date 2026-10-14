//! Physical disks, not volumes: how full each one is, and which disk a path
//! lives on. APFS volumes share their container's free space, so a disk's free
//! space is what its containers have left.

use anyhow::{Context, Result};
use plist::{Dictionary, Value};
use std::collections::HashMap;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    /// BSD name of the whole disk, `disk0`.
    pub id: String,
    /// The drive's own name, `APPLE SSD AP1024R`.
    pub model: String,
    pub internal: bool,
    /// Holds the volume macOS booted from.
    pub startup: bool,
    pub size: u64,
    /// The volume names people know it by.
    pub volumes: Vec<String>,
    pub stores: Vec<Store>,
}

/// An APFS container, or a plain partition, on a disk.
#[derive(Clone, Debug, PartialEq)]
pub struct Store {
    /// `disk3` for a container, `disk6s1` for a partition.
    pub id: String,
    pub free: u64,
    /// A mounted volume to read fresh free space from.
    pub probe: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Ok,
    Low,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Ok => "OK",
            Severity::Low => "LOW",
            Severity::Critical => "CRITICAL",
        }
    }
}

impl Disk {
    pub fn free(&self) -> u64 {
        self.stores.iter().map(|s| s.free).sum::<u64>().min(self.size)
    }

    pub fn used(&self) -> u64 {
        self.size - self.free()
    }

    pub fn severity(&self) -> Severity {
        severity(self.free(), self.size)
    }

    /// "Internal · Macintosh HD" or "External · SSD, src".
    pub fn title(&self) -> String {
        let place = if self.internal { "Internal" } else { "External" };
        if self.volumes.is_empty() {
            format!("{place} · {}", self.model)
        } else {
            format!("{place} · {}", self.volumes.join(", "))
        }
    }
}

/// A disk's bar in cells: used space sweep can't free, then what it can
/// (Safe, then Review), then free space.
/// Anything reclaimable from a gigabyte up gets at least one cell, so it
/// shows on a terabyte disk.
pub fn bar_cells(d: &Disk, r: &Reclaim, width: usize) -> [usize; 4] {
    const GB: u64 = 1_000_000_000;
    let size = d.size.max(1) as f64;
    let cells = |b: u64| {
        let c = ((b as f64 / size) * width as f64).round() as usize;
        if c == 0 && b >= GB { 1 } else { c }
    };
    let used = cells(d.used()).min(width);
    let safe = cells(r.safe).min(used);
    let review = cells(r.review).min(used - safe);
    [used - safe - review, safe, review, width - used]
}

pub const BAR_GLYPHS: [&str; 4] = ["█", "▓", "▒", "░"];

/// "63% used · 365 GB free of 1.0 TB".
pub fn usage(d: &Disk) -> String {
    let pct = if d.size == 0 { 0 } else { (d.used() as f64 * 100.0 / d.size as f64).round() as u64 };
    format!("{pct}% used · {} free of {}", crate::fmt::bytes(d.free()), crate::fmt::bytes(d.size))
}

/// "sweep frees 2.2 GB · 3.6 GB to review · marked 1.9 GB → 367 GB free", or
/// the longest shorter wording that fits in `room` columns, keeping what's
/// marked longest.
pub fn prospects(d: &Disk, r: &Reclaim, room: usize) -> String {
    use crate::fmt::bytes;
    let frees = bytes(r.safe);
    let review = if r.review > 0 { format!(" · {} to review", bytes(r.review)) } else { String::new() };
    let forms = if r.marked > 0 {
        let after = bytes(d.free() + r.marked);
        let marked = format!("marked {} → {after} free", bytes(r.marked));
        vec![
            format!("sweep frees {frees}{review} · {marked}"),
            format!("frees {frees} · {marked}"),
            marked,
            format!("→ {after} free"),
        ]
    } else {
        vec![format!("sweep frees {frees}{review}"), format!("frees {frees}{review}"), format!("frees {frees}")]
    };
    forms.into_iter().find(|s| s.chars().count() <= room).unwrap_or_default()
}

/// Critical below 5% or 20 GB free, low below 15% or 50 GB: macOS needs room
/// for swap, snapshots and updates long before a disk is actually full.
pub fn severity(free: u64, size: u64) -> Severity {
    const GB: u64 = 1_000_000_000;
    let pct = if size == 0 { 100.0 } else { free as f64 * 100.0 / size as f64 };
    if pct < 5.0 || free < 20 * GB {
        Severity::Critical
    } else if pct < 15.0 || free < 50 * GB {
        Severity::Low
    } else {
        Severity::Ok
    }
}

/// Every physical disk, from `diskutil`. Takes a couple of seconds.
pub fn survey() -> Result<Vec<Disk>> {
    let list = diskutil(&["list", "-plist", "physical"])?;
    let apfs = diskutil(&["apfs", "list", "-plist"])?;
    let mut infos = HashMap::new();
    for id in whole_disks(&list) {
        if let Ok(info) = diskutil(&["info", "-plist", &id]) {
            infos.insert(id, info);
        }
    }
    Ok(assemble(&list, &apfs, &infos, &mounts()))
}

fn diskutil(args: &[&str]) -> Result<Dictionary> {
    let out = Command::new("/usr/sbin/diskutil").args(args).output().context("running diskutil")?;
    if !out.status.success() {
        anyhow::bail!("diskutil {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    let v = Value::from_reader(std::io::Cursor::new(out.stdout)).context("reading diskutil's output")?;
    v.into_dictionary().context("diskutil didn't return a dictionary")
}

fn whole_disks(list: &Dictionary) -> Vec<String> {
    list.get("WholeDisks")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_string).map(String::from).collect())
        .unwrap_or_default()
}

/// A mounted file system: its BSD name and where it's mounted.
#[derive(Clone, Debug, PartialEq)]
pub struct Mount {
    pub dev: String,
    pub path: PathBuf,
}

/// Every mounted local file system with a /dev entry.
pub fn mounts() -> Vec<Mount> {
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if n <= 0 {
        return Vec::new();
    }
    let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; n as usize + 8];
    let bytes = (buf.len() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    let n = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    buf.truncate(n.max(0) as usize);
    buf.iter()
        .filter_map(|st| {
            let from = unsafe { CStr::from_ptr(st.f_mntfromname.as_ptr()) }.to_string_lossy().into_owned();
            let on = unsafe { CStr::from_ptr(st.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
            Some(Mount { dev: from.strip_prefix("/dev/")?.to_string(), path: PathBuf::from(on) })
        })
        .collect()
}

/// `disk3s5s1` → `disk3`: the container (or whole disk) a device belongs to.
pub fn parent_disk(dev: &str) -> &str {
    let digits = dev.strip_prefix("disk").map(|rest| rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len()));
    match digits {
        Some(n) => &dev[..4 + n],
        None => dev,
    }
}

fn assemble(list: &Dictionary, apfs: &Dictionary, infos: &HashMap<String, Dictionary>, mounts: &[Mount]) -> Vec<Disk> {
    let s = |d: &Dictionary, k: &str| d.get(k).and_then(Value::as_string).map(String::from);
    let n = |d: &Dictionary, k: &str| d.get(k).and_then(Value::as_unsigned_integer);
    let containers: Vec<&Dictionary> = apfs
        .get("Containers")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_dictionary).collect())
        .unwrap_or_default();
    let boot = mounts.iter().find(|m| m.path == Path::new("/")).map(|m| parent_disk(&m.dev).to_string());
    // Any mounted volume of a container reports the container's free space.
    let probe_for = |id: &str| mounts.iter().find(|m| m.dev == id || parent_disk(&m.dev) == id).map(|m| m.path.clone());

    let mut disks = Vec::new();
    let whole = whole_disks(list);
    let entries = list.get("AllDisksAndPartitions").and_then(Value::as_array).cloned().unwrap_or_default();
    for entry in entries.iter().filter_map(Value::as_dictionary) {
        let Some(id) = s(entry, "DeviceIdentifier") else { continue };
        if !whole.contains(&id) {
            continue;
        }
        let info = infos.get(&id);
        let model = info.and_then(|i| s(i, "MediaName")).unwrap_or_else(|| id.clone());
        let internal = info.and_then(|i| i.get("Internal")).and_then(Value::as_boolean).unwrap_or(false);
        let size = n(entry, "Size").unwrap_or(0);
        let mut stores = Vec::new();
        let mut volumes = Vec::new();
        let mut startup = false;
        let parts = entry.get("Partitions").and_then(Value::as_array).cloned().unwrap_or_default();
        for p in parts.iter().filter_map(Value::as_dictionary) {
            let Some(pid) = s(p, "DeviceIdentifier") else { continue };
            let container = containers.iter().find(|c| {
                c.get("PhysicalStores").and_then(Value::as_array).is_some_and(|ps| {
                    ps.iter()
                        .filter_map(Value::as_dictionary)
                        .any(|d| s(d, "DeviceIdentifier").as_deref() == Some(&pid))
                })
            });
            match container {
                Some(c) => {
                    let cid = s(c, "ContainerReference").unwrap_or_default();
                    startup |= boot.as_deref() == Some(cid.as_str());
                    for v in c
                        .get("Volumes")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_dictionary)
                    {
                        let roles: Vec<String> = v
                            .get("Roles")
                            .and_then(Value::as_array)
                            .map(|r| r.iter().filter_map(Value::as_string).map(String::from).collect())
                            .unwrap_or_default();
                        let name = s(v, "Name").unwrap_or_default();
                        let shown = roles.is_empty() || roles.iter().any(|r| r == "System");
                        if shown && !name.is_empty() && !name.contains(":/") && !volumes.contains(&name) {
                            volumes.push(name);
                        }
                    }
                    stores.push(Store { free: n(c, "CapacityFree").unwrap_or(0), probe: probe_for(&cid), id: cid });
                }
                None => {
                    let probe = probe_for(&pid);
                    if probe.is_some()
                        && let Some(name) = s(p, "VolumeName")
                        && !name.is_empty()
                        && !volumes.contains(&name)
                    {
                        volumes.push(name);
                    }
                    let free = probe.as_deref().and_then(statfs_free).unwrap_or(0);
                    stores.push(Store { id: pid, free, probe });
                }
            }
        }
        disks.push(Disk { id, model, internal, startup, size, volumes, stores });
    }
    // The startup disk first, then internal ones, then the rest by name.
    disks.sort_by_key(|d| (!d.startup, !d.internal, d.id.clone()));
    disks
}

fn statfs_free(path: &Path) -> Option<u64> {
    crate::fsattr::volume(path).map(|v| v.free)
}

/// Reads fresh free space for every store that has a mounted volume.
pub fn refresh(disks: &mut [Disk]) {
    for d in disks {
        for s in &mut d.stores {
            if let Some(free) = s.probe.as_deref().and_then(statfs_free) {
                s.free = free;
            }
        }
    }
}

/// Finds which disk a path lives on.
#[derive(Default)]
pub struct Locator {
    by_store: HashMap<String, usize>,
    cache: HashMap<PathBuf, Option<usize>>,
    items: HashMap<usize, Option<usize>>,
    /// Paths placed by prefix rather than by asking the file system, most
    /// specific first: for made-up items.
    pins: Vec<(PathBuf, usize)>,
}

/// What sweep could free on one disk, and how much of that is marked.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize)]
pub struct Reclaim {
    pub safe: u64,
    pub review: u64,
    pub marked: u64,
}

impl Locator {
    pub fn new(disks: &[Disk]) -> Locator {
        let mut by_store = HashMap::new();
        for (i, d) in disks.iter().enumerate() {
            by_store.insert(d.id.clone(), i);
            for s in &d.stores {
                by_store.insert(s.id.clone(), i);
            }
        }
        Locator { by_store, ..Locator::default() }
    }

    pub fn pinned(pins: Vec<(PathBuf, usize)>) -> Locator {
        Locator { pins, ..Locator::default() }
    }

    pub fn item(&mut self, item: &crate::model::Item) -> Option<usize> {
        if let Some(hit) = self.items.get(&item.id) {
            return *hit;
        }
        let found = self.disk_of(&item.path);
        self.items.insert(item.id, found);
        found
    }

    /// Safe, Review and marked bytes per disk.
    pub fn reclaim<'a>(
        &mut self,
        disks: usize,
        items: impl IntoIterator<Item = &'a crate::model::Item>,
        marked: impl Fn(usize) -> bool,
    ) -> Vec<Reclaim> {
        use crate::model::Badge;
        let mut out = vec![Reclaim::default(); disks];
        for i in items {
            let Some(v) = i.verdict.as_ref().filter(|v| !v.hidden) else { continue };
            if !matches!(v.badge, Badge::Safe | Badge::Review) {
                continue;
            }
            let Some(d) = self.item(i).filter(|&d| d < disks) else { continue };
            let bytes = v.bytes.unwrap_or(0);
            match v.badge {
                Badge::Safe => out[d].safe += bytes,
                _ => out[d].review += bytes,
            }
            if marked(i.id) {
                out[d].marked += bytes;
            }
        }
        out
    }

    /// The index of the disk holding `path`. Siblings share their parent's
    /// answer, so thousands of temp folders cost one lookup.
    pub fn disk_of(&mut self, path: &Path) -> Option<usize> {
        if !self.pins.is_empty() {
            return self.pins.iter().find(|(p, _)| path.starts_with(p)).map(|(_, d)| *d);
        }
        let key = path.parent().unwrap_or(path).to_path_buf();
        if let Some(hit) = self.cache.get(&key) {
            return *hit;
        }
        let probe = key.ancestors().find(|a| a.exists()).unwrap_or(Path::new("/"));
        let found = device_of(probe)
            .and_then(|dev| self.by_store.get(&dev).or_else(|| self.by_store.get(parent_disk(&dev))).copied());
        self.cache.insert(key, found);
        found
    }
}

/// The BSD name of the device holding `path`, `disk3s1`.
pub fn device_of(path: &Path) -> Option<String> {
    let c = crate::fsattr::cpath(path).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let from = unsafe { CStr::from_ptr(st.f_mntfromname.as_ptr()) }.to_string_lossy().into_owned();
    from.strip_prefix("/dev/").map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict(xml: &str) -> Dictionary {
        Value::from_reader_xml(xml.as_bytes()).unwrap().into_dictionary().unwrap()
    }

    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
  <key>WholeDisks</key><array><string>disk0</string><string>disk6</string></array>
  <key>AllDisksAndPartitions</key><array>
    <dict><key>DeviceIdentifier</key><string>disk0</string><key>Size</key><integer>1000555581440</integer>
      <key>Partitions</key><array>
        <dict><key>DeviceIdentifier</key><string>disk0s1</string><key>Size</key><integer>524288000</integer></dict>
        <dict><key>DeviceIdentifier</key><string>disk0s2</string><key>Size</key><integer>994662584320</integer></dict>
        <dict><key>DeviceIdentifier</key><string>disk0s3</string><key>Size</key><integer>5368664064</integer></dict>
      </array></dict>
    <dict><key>DeviceIdentifier</key><string>disk6</string><key>Size</key><integer>1024209543168</integer>
      <key>Partitions</key><array>
        <dict><key>DeviceIdentifier</key><string>disk6s1</string><key>Size</key><integer>209715200</integer><key>VolumeName</key><string>EFI</string></dict>
        <dict><key>DeviceIdentifier</key><string>disk6s2</string><key>Size</key><integer>1023999787008</integer></dict>
      </array></dict>
  </array>
</dict></plist>"#;

    const APFS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict><key>Containers</key><array>
  <dict><key>ContainerReference</key><string>disk1</string><key>CapacityFree</key><integer>506138624</integer>
    <key>PhysicalStores</key><array><dict><key>DeviceIdentifier</key><string>disk0s1</string></dict></array>
    <key>Volumes</key><array><dict><key>Name</key><string>iSCPreboot</string><key>Roles</key><array><string>Preboot</string></array></dict></array></dict>
  <dict><key>ContainerReference</key><string>disk3</string><key>CapacityFree</key><integer>365395578880</integer>
    <key>PhysicalStores</key><array><dict><key>DeviceIdentifier</key><string>disk0s2</string></dict></array>
    <key>Volumes</key><array>
      <dict><key>Name</key><string>Macintosh HD - Data</string><key>Roles</key><array><string>Data</string></array></dict>
      <dict><key>Name</key><string>Macintosh HD</string><key>Roles</key><array><string>System</string></array></dict>
      <dict><key>Name</key><string>fuse:/Users/u/mnt/remote</string><key>Roles</key><array/></dict>
    </array></dict>
  <dict><key>ContainerReference</key><string>disk5</string><key>CapacityFree</key><integer>466067456</integer>
    <key>PhysicalStores</key><array><dict><key>DeviceIdentifier</key><string>disk4s1</string></dict></array>
    <key>Volumes</key><array><dict><key>Name</key><string>iOS 26.5 Simulator</string><key>Roles</key><array/></dict></array></dict>
  <dict><key>ContainerReference</key><string>disk7</string><key>CapacityFree</key><integer>374776868864</integer>
    <key>PhysicalStores</key><array><dict><key>DeviceIdentifier</key><string>disk6s2</string></dict></array>
    <key>Volumes</key><array>
      <dict><key>Name</key><string>SSD</string><key>Roles</key><array/></dict>
      <dict><key>Name</key><string>src</string><key>Roles</key><array/></dict>
    </array></dict>
</array></dict></plist>"#;

    fn info(model: &str, internal: bool) -> Dictionary {
        let mut d = Dictionary::new();
        d.insert("MediaName".into(), model.into());
        d.insert("Internal".into(), internal.into());
        d
    }

    #[test]
    fn physical_disks_with_their_containers_and_no_disk_images() {
        let infos = HashMap::from([
            ("disk0".to_string(), info("APPLE SSD AP1024R", true)),
            ("disk6".to_string(), info("Predator SSD GM7 1TB", false)),
        ]);
        let mounts = vec![
            Mount { dev: "disk3s5s1".into(), path: "/".into() },
            Mount { dev: "disk3s1".into(), path: "/System/Volumes/Data".into() },
            Mount { dev: "disk7s1".into(), path: "/Volumes/SSD".into() },
            Mount { dev: "disk5s1".into(), path: "/Library/Developer/CoreSimulator/Volumes/iOS_23F77".into() },
        ];
        let disks = assemble(&dict(LIST), &dict(APFS), &infos, &mounts);
        assert_eq!(disks.len(), 2, "the simulator image's container isn't on a physical disk");
        let internal = &disks[0];
        assert_eq!(internal.id, "disk0");
        assert!(internal.startup && internal.internal);
        assert_eq!(internal.title(), "Internal · Macintosh HD");
        assert_eq!(internal.stores.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["disk1", "disk3", "disk0s3"]);
        assert_eq!(internal.stores[1].probe.as_deref(), Some(Path::new("/")));
        assert_eq!(internal.free(), 506138624 + 365395578880);
        let external = &disks[1];
        assert_eq!(external.title(), "External · SSD, src");
        assert_eq!(external.stores[1].probe.as_deref(), Some(Path::new("/Volumes/SSD")));
        assert!(!external.startup);
    }

    #[test]
    fn severity_by_share_and_absolute_space() {
        const GB: u64 = 1_000_000_000;
        assert_eq!(severity(365 * GB, 1000 * GB), Severity::Ok);
        assert_eq!(severity(120 * GB, 1000 * GB), Severity::Low);
        assert_eq!(severity(40 * GB, 1000 * GB), Severity::Critical);
        assert_eq!(severity(45 * GB, 256 * GB), Severity::Low);
        assert_eq!(severity(15 * GB, 256 * GB), Severity::Critical);
    }

    #[test]
    fn prospects_shorten_to_fit_and_keep_whats_marked() {
        const GB: u64 = 1_000_000_000;
        let store = Store { id: "disk3".into(), free: 38 * GB, probe: None };
        let d = Disk {
            id: "disk0".into(),
            model: "SSD".into(),
            internal: true,
            startup: true,
            size: 1000 * GB,
            volumes: vec![],
            stores: vec![store],
        };
        let r = Reclaim { safe: 49 * GB, review: 123 * GB, marked: 49 * GB };
        assert_eq!(prospects(&d, &r, usize::MAX), "sweep frees 49 GB · 123 GB to review · marked 49 GB → 87 GB free");
        assert_eq!(prospects(&d, &r, 40), "frees 49 GB · marked 49 GB → 87 GB free");
        assert_eq!(prospects(&d, &r, 30), "marked 49 GB → 87 GB free");
        assert_eq!(prospects(&d, &r, 5), "");
        let r = Reclaim { marked: 0, ..r };
        assert_eq!(prospects(&d, &r, 20), "frees 49 GB");
    }

    #[test]
    fn device_names_map_to_their_container() {
        assert_eq!(parent_disk("disk3s5s1"), "disk3");
        assert_eq!(parent_disk("disk7s2"), "disk7");
        assert_eq!(parent_disk("disk12"), "disk12");
    }

    #[test]
    fn this_macs_disks() {
        let Ok(disks) = survey() else { return };
        assert!(disks.iter().any(|d| d.startup), "{disks:?}");
        let mut loc = Locator::new(&disks);
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let at = loc.disk_of(&home.join("Library/Caches")).expect("the home folder is on a disk");
        assert!(disks[at].startup || !disks[at].internal);
        assert!(disks.iter().all(|d| d.free() <= d.size));
    }
}

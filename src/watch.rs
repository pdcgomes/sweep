//! Which folders changed, from FSEvents. macOS keeps a record of every change
//! on a volume as it happens; a stream asks for the ones under some roots and
//! gets them a second or so later, a batch at a time, without walking anything.

use anyhow::{Result, bail};
use std::ffi::{CStr, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

/// A file or folder that changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub dir: bool,
    /// It was created, removed or renamed: the folder it's in lists different entries.
    pub entries: bool,
    /// Changes were coalesced or dropped: anything below may have changed too.
    pub subtree: bool,
    /// A watched root itself was moved, deleted, mounted or unmounted.
    pub root: bool,
}

type CFRef = *const c_void;
type Stream = *mut c_void;
type Queue = *mut c_void;
type Callback = extern "C" fn(Stream, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[repr(C)]
struct ArrayCallBacks {
    version: isize,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
    equal: *const c_void,
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        alloc: CFRef,
        callback: Callback,
        context: *const StreamContext,
        paths: CFRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> Stream;
    fn FSEventStreamSetDispatchQueue(stream: Stream, queue: Queue);
    fn FSEventStreamStart(stream: Stream) -> u8;
    fn FSEventStreamStop(stream: Stream);
    fn FSEventStreamInvalidate(stream: Stream);
    fn FSEventStreamRelease(stream: Stream);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFTypeArrayCallBacks: ArrayCallBacks;
    fn CFStringCreateWithBytes(alloc: CFRef, bytes: *const u8, len: isize, encoding: u32, external: u8) -> CFRef;
    fn CFArrayCreate(alloc: CFRef, values: *const CFRef, count: isize, callbacks: *const ArrayCallBacks) -> CFRef;
    fn CFRelease(cf: CFRef);
}

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> Queue;
    fn dispatch_sync_f(queue: Queue, context: *mut c_void, work: extern "C" fn(*mut c_void));
    fn dispatch_release(object: Queue);
}

const UTF8: u32 = 0x0800_0100;
const SINCE_NOW: u64 = u64::MAX;
const WATCH_ROOT: u32 = 0x04;
/// The process's own writes (its deletions, its log) don't come back to it.
const IGNORE_SELF: u32 = 0x08;
/// Each change names the file, so only the item it's in is looked at.
const FILE_EVENTS: u32 = 0x10;
const MUST_SCAN_SUBDIRS: u32 = 0x01;
const USER_DROPPED: u32 = 0x02;
const KERNEL_DROPPED: u32 = 0x04;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
const CREATED: u32 = 0x100;
const REMOVED: u32 = 0x200;
const RENAMED: u32 = 0x800;
const IS_DIR: u32 = 0x2_0000;

struct Sink {
    /// FSEvents reports real paths (/private/var/…); each root's own spelling is put back.
    aliases: Vec<(PathBuf, PathBuf)>,
    send: Box<dyn Fn(Vec<Change>) + Send + Sync>,
}

impl Sink {
    fn shown(&self, real: PathBuf) -> PathBuf {
        for (r, shown) in &self.aliases {
            if let Ok(rest) = real.strip_prefix(r) {
                return if rest.as_os_str().is_empty() { shown.clone() } else { shown.join(rest) };
            }
        }
        real
    }
}

extern "C" fn on_events(_: Stream, info: *mut c_void, n: usize, paths: *mut c_void, flags: *const u32, _: *const u64) {
    // SAFETY: `info` is the Sink that Watcher owns, freed only after the
    // stream is invalidated and this queue has drained; FSEvents passes `n`
    // C strings and `n` flags.
    let sink = unsafe { &*(info as *const Sink) };
    let paths = paths as *const *const c_char;
    let mut changes = Vec::with_capacity(n);
    for i in 0..n {
        let (raw, f) = unsafe { (CStr::from_ptr(*paths.add(i)), *flags.add(i)) };
        if f & HISTORY_DONE != 0 {
            continue;
        }
        let mut bytes = raw.to_bytes();
        while bytes.len() > 1 && bytes.ends_with(b"/") {
            bytes = &bytes[..bytes.len() - 1];
        }
        changes.push(Change {
            path: sink.shown(PathBuf::from(OsStr::from_bytes(bytes))),
            dir: f & IS_DIR != 0,
            entries: f & (CREATED | REMOVED | RENAMED) != 0,
            subtree: f & (MUST_SCAN_SUBDIRS | USER_DROPPED | KERNEL_DROPPED) != 0,
            root: f & (ROOT_CHANGED | MOUNT | UNMOUNT) != 0,
        });
    }
    if !changes.is_empty() {
        (sink.send)(changes);
    }
}

extern "C" fn nothing(_: *mut c_void) {}

/// A running stream; dropping it stops the watching.
pub struct Watcher {
    stream: Stream,
    queue: Queue,
    sink: *mut Sink,
}

impl Watcher {
    /// Watches the roots that exist (FSEvents can't watch a folder before it
    /// exists) and calls `send` with each batch of changes, `latency` after the
    /// first change in it.
    pub fn start(
        roots: &[PathBuf],
        latency: Duration,
        send: impl Fn(Vec<Change>) + Send + Sync + 'static,
    ) -> Result<Watcher> {
        let mut aliases = Vec::new();
        let mut real = Vec::new();
        for r in roots {
            let Ok(c) = r.canonicalize() else { continue };
            if c != *r {
                aliases.push((c.clone(), r.clone()));
            }
            real.push(c);
        }
        if real.is_empty() {
            bail!("none of the folders to watch exist");
        }
        unsafe {
            let strings: Vec<CFRef> = real
                .iter()
                .map(|p| {
                    let b = p.as_os_str().as_bytes();
                    CFStringCreateWithBytes(std::ptr::null(), b.as_ptr(), b.len() as isize, UTF8, 0)
                })
                .collect();
            let array = CFArrayCreate(
                std::ptr::null(),
                strings.as_ptr(),
                strings.len() as isize,
                &raw const kCFTypeArrayCallBacks,
            );
            for s in strings {
                CFRelease(s);
            }
            let sink = Box::into_raw(Box::new(Sink { aliases, send: Box::new(send) }));
            let context = StreamContext {
                version: 0,
                info: sink as *mut c_void,
                retain: std::ptr::null(),
                release: std::ptr::null(),
                copy_description: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                on_events,
                &context,
                array,
                SINCE_NOW,
                latency.as_secs_f64(),
                WATCH_ROOT | IGNORE_SELF | FILE_EVENTS,
            );
            CFRelease(array);
            if stream.is_null() {
                drop(Box::from_raw(sink));
                bail!("FSEvents wouldn't make a stream");
            }
            let queue = dispatch_queue_create(c"sweep.watch".as_ptr(), std::ptr::null());
            FSEventStreamSetDispatchQueue(stream, queue);
            if FSEventStreamStart(stream) == 0 {
                FSEventStreamInvalidate(stream);
                FSEventStreamRelease(stream);
                dispatch_release(queue);
                drop(Box::from_raw(sink));
                bail!("FSEvents wouldn't start watching");
            }
            Ok(Watcher { stream, queue, sink })
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            // A batch already being handed over finishes before its sink goes.
            dispatch_sync_f(self.queue, std::ptr::null_mut(), nothing);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.sink));
        }
    }
}

/// The roots that exist, leaving out any inside another.
pub fn existing(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.retain(|r| r.is_dir());
    roots.sort();
    roots.dedup();
    let all = roots.clone();
    roots.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
    roots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::Instant;

    fn wait_for(rx: &Receiver<Vec<Change>>, limit: Duration, want: impl Fn(&Change) -> bool) -> Option<Change> {
        let end = Instant::now() + limit;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(batch) => {
                    if let Some(c) = batch.into_iter().find(|c| want(c)) {
                        return Some(c);
                    }
                }
                Err(_) => break,
            }
        }
        None
    }

    #[test]
    fn reports_files_changed_by_others_in_the_roots_own_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let (tx, rx) = channel();
        let w = Watcher::start(&[root.clone(), dir.path().join("not-there")], Duration::from_millis(100), move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));

        // sweep's own writes don't wake it up.
        std::fs::write(root.join("a/b/mine"), "x").unwrap();
        assert_eq!(wait_for(&rx, Duration::from_millis(1500), |c| c.path.starts_with(&root)), None);

        let file = root.join("a/b/theirs");
        let st = Command::new("/bin/sh").arg("-c").arg(format!("echo x > '{}'", file.display())).status().unwrap();
        assert!(st.success());
        let c = wait_for(&rx, Duration::from_secs(10), |c| c.path == file).expect("the new file");
        assert!(c.entries && !c.dir && !c.subtree, "{c:?}");
        drop(w);
    }

    #[test]
    fn roots_inside_others_and_missing_ones_go() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir_all(a.join("inner")).unwrap();
        let got = existing(vec![a.join("inner"), a.clone(), dir.path().join("missing"), a.clone()]);
        assert_eq!(got, [a]);
    }
}

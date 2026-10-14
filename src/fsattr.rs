//! File metadata straight from `getattrlist(2)` and `getattrlistbulk(2)`.
//!
//! The figure that matters is `ATTR_CMNEXT_PRIVATESIZE`: the bytes of a file
//! that no clone or snapshot shares, i.e. what deleting it frees right away.
//! `du` adds up allocated blocks instead, which counts every APFS clone in full.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjKind {
    File,
    Dir,
    Symlink,
    Socket,
    Other,
}

#[derive(Clone, Debug)]
pub struct Attrs {
    pub kind: ObjKind,
    pub dev: i32,
    pub mtime: SystemTime,
    pub ctime: SystemTime,
    pub fileid: u64,
    /// Only meaningful for non-directories.
    pub nlink: u32,
    /// Allocated bytes: the figure `du` adds up.
    pub alloc: u64,
    /// Bytes deleting this object would free. `None` when the volume doesn't
    /// report it (anything but APFS), and callers fall back to `alloc`.
    pub private: Option<u64>,
}

impl Attrs {
    /// The later of the content and status change times. ctime moves on any
    /// write, rename or link change and can't be set back, so a freshly
    /// unpacked tree with old mtimes still counts as written now.
    pub fn last_write(&self) -> SystemTime {
        self.mtime.max(self.ctime)
    }

    pub fn freed(&self) -> u64 {
        self.private.unwrap_or(self.alloc)
    }
}

pub struct Entry {
    pub name: OsString,
    pub attrs: Attrs,
}

const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
const VSOCK: u32 = 6;

const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_CHGTIME
    | libc::ATTR_CMN_FILEID;
const FILE: u32 = libc::ATTR_FILE_LINKCOUNT | libc::ATTR_FILE_ALLOCSIZE;
const CMNEXT: u32 = libc::ATTR_CMNEXT_PRIVATESIZE;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;

fn attr_list(bulk: bool) -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: COMMON | if bulk { libc::ATTR_CMN_NAME | ATTR_CMN_ERROR } else { 0 },
        volattr: 0,
        dirattr: 0,
        fileattr: FILE,
        forkattr: CMNEXT,
    }
}

/// Like `read_dir` without sizes: about twice as fast, for questions that
/// only need types and dates.
pub fn read_dir_quick(dir: &Path) -> io::Result<Vec<Entry>> {
    read_dir_with(dir, libc::attrlist { fileattr: 0, forkattr: 0, ..attr_list(true) }, 0)
}

pub fn cpath(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// Attributes of one path, without following a final symlink.
pub fn attrs(path: &Path) -> io::Result<Attrs> {
    let c = cpath(path)?;
    let mut list = attr_list(false);
    let mut buf = [0u8; 256];
    let rc = unsafe {
        libc::getattrlist(
            c.as_ptr(),
            &mut list as *mut _ as *mut libc::c_void,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::FSOPT_NOFOLLOW | libc::FSOPT_ATTR_CMN_EXTENDED,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let len = (read_u32(&buf, 0) as usize).min(buf.len());
    parse(&buf[..len]).map(|(a, _, _)| a)
}

/// Every entry of a directory with its attributes. `getattrlistbulk` returns
/// many entries per system call, which matters on trees of a million files.
pub fn read_dir(dir: &Path) -> io::Result<Vec<Entry>> {
    read_dir_with(dir, attr_list(true), libc::FSOPT_ATTR_CMN_EXTENDED as u64)
}

fn read_dir_with(dir: &Path, mut list: libc::attrlist, options: u64) -> io::Result<Vec<Entry>> {
    let c = cpath(dir)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::new();
    loop {
        let n = unsafe {
            libc::getattrlistbulk(
                fd.as_raw_fd(),
                &mut list as *mut _ as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                options,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if n == 0 {
            break;
        }
        let mut off = 0usize;
        for _ in 0..n {
            if off + 4 > buf.len() {
                return Err(bad_record());
            }
            let len = read_u32(&buf, off) as usize;
            if len < 4 || off + len > buf.len() {
                return Err(bad_record());
            }
            let (attrs, name, error) = parse(&buf[off..off + len])?;
            off += len;
            if let (Some(name), 0) = (name, error) {
                out.push(Entry { name, attrs });
            }
        }
    }
    Ok(out)
}

fn bad_record() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "malformed attribute record")
}

/// Parses one record: a length word, the returned-attributes set, then each
/// returned attribute in a fixed order (common, file, extended common).
fn parse(rec: &[u8]) -> io::Result<(Attrs, Option<OsString>, u32)> {
    let mut off = 4;
    if rec.len() < off + 20 {
        return Err(bad_record());
    }
    let ret_common = read_u32(rec, off);
    let ret_file = read_u32(rec, off + 12);
    let ret_ext = read_u32(rec, off + 16);
    off += 20;
    let need = |off: usize, n: usize| if off + n <= rec.len() { Ok(()) } else { Err(bad_record()) };

    let mut error = 0;
    if ret_common & ATTR_CMN_ERROR != 0 {
        need(off, 4)?;
        error = read_u32(rec, off);
        off += 4;
    }
    let mut name = None;
    if ret_common & libc::ATTR_CMN_NAME != 0 {
        need(off, 8)?;
        let data_off = read_u32(rec, off) as i32 as isize;
        let data_len = read_u32(rec, off + 4) as usize;
        let start = off as isize + data_off;
        if start < 0 || start as usize + data_len > rec.len() {
            return Err(bad_record());
        }
        let start = start as usize;
        // The length counts the trailing NUL.
        let bytes = &rec[start..start + data_len.saturating_sub(1)];
        name = Some(OsStr::from_bytes(bytes).to_os_string());
        off += 8;
    }
    let mut dev = 0;
    if ret_common & libc::ATTR_CMN_DEVID != 0 {
        need(off, 4)?;
        dev = read_u32(rec, off) as i32;
        off += 4;
    }
    let mut kind = ObjKind::Other;
    if ret_common & libc::ATTR_CMN_OBJTYPE != 0 {
        need(off, 4)?;
        kind = match read_u32(rec, off) {
            VREG => ObjKind::File,
            VDIR => ObjKind::Dir,
            VLNK => ObjKind::Symlink,
            VSOCK => ObjKind::Socket,
            _ => ObjKind::Other,
        };
        off += 4;
    }
    let mut mtime = UNIX_EPOCH;
    if ret_common & libc::ATTR_CMN_MODTIME != 0 {
        need(off, 16)?;
        mtime = timespec(rec, off);
        off += 16;
    }
    let mut ctime = UNIX_EPOCH;
    if ret_common & libc::ATTR_CMN_CHGTIME != 0 {
        need(off, 16)?;
        ctime = timespec(rec, off);
        off += 16;
    }
    let mut fileid = 0;
    if ret_common & libc::ATTR_CMN_FILEID != 0 {
        need(off, 8)?;
        fileid = read_u64(rec, off);
        off += 8;
    }
    let mut nlink = 1;
    if ret_file & libc::ATTR_FILE_LINKCOUNT != 0 {
        need(off, 4)?;
        nlink = read_u32(rec, off);
        off += 4;
    }
    let mut alloc = 0;
    if ret_file & libc::ATTR_FILE_ALLOCSIZE != 0 {
        need(off, 8)?;
        alloc = read_u64(rec, off);
        off += 8;
    }
    let mut private = None;
    if ret_ext & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
        need(off, 8)?;
        private = Some(read_u64(rec, off));
    }
    Ok((Attrs { kind, dev, mtime, ctime, fileid, nlink, alloc, private }, name, error))
}

fn read_u32(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(b[off..off + 4].try_into().unwrap())
}

fn read_u64(b: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(b[off..off + 8].try_into().unwrap())
}

fn timespec(b: &[u8], off: usize) -> SystemTime {
    let secs = read_u64(b, off) as i64;
    let nanos = (read_u64(b, off + 8) as i64).clamp(0, 999_999_999) as u32;
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nanos)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
    }
}

pub struct Volume {
    pub mount: String,
    pub free: u64,
    /// APFS volumes in one container share their free space and report the
    /// same size, so this key groups them.
    pub key: (u64, u64),
}

/// The volume holding `path`, for the free-space line in the log.
pub fn volume(path: &Path) -> Option<Volume> {
    let c = cpath(path).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    let mount = unsafe { std::ffi::CStr::from_ptr(st.f_mntonname.as_ptr()) }.to_string_lossy().into_owned();
    let bsize = st.f_bsize as u64;
    Some(Volume { mount, free: st.f_bavail * bsize, key: (st.f_blocks * bsize, bsize) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write_random;
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    #[test]
    fn single_and_bulk_agree_with_lstat() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.bin");
        write_random(&f, 300_000);
        std::os::unix::fs::symlink("a.bin", dir.path().join("link")).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();

        let a = attrs(&f).unwrap();
        let m = std::fs::symlink_metadata(&f).unwrap();
        assert_eq!(a.kind, ObjKind::File);
        assert_eq!(a.fileid, m.ino());
        assert_eq!(a.alloc, m.blocks() * 512);
        assert_eq!(a.nlink as u64, m.nlink());
        assert_eq!(a.mtime.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64, m.mtime());
        assert_eq!(a.ctime.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64, m.ctime());
        assert_eq!(a.private, Some(a.alloc), "a fresh file owns all its blocks");

        let mut entries = read_dir(dir.path()).unwrap();
        entries.sort_by(|x, y| x.name.cmp(&y.name));
        let names: Vec<_> = entries.iter().map(|e| e.name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["a.bin", "link", "sub"]);
        assert_eq!(entries[0].attrs.private, a.private);
        assert_eq!(entries[0].attrs.fileid, a.fileid);
        assert_eq!(entries[1].attrs.kind, ObjKind::Symlink);
        assert_eq!(entries[2].attrs.kind, ObjKind::Dir);
    }

    #[test]
    fn a_cp_c_clone_frees_nothing_while_du_counts_it_in_full() {
        let dir = tempfile::tempdir().unwrap();
        let orig = dir.path().join("fixture.bin");
        write_random(&orig, 4 << 20);
        let clone = dir.path().join("fixture-copy.bin");
        assert!(Command::new("cp").arg("-c").arg(&orig).arg(&clone).status().unwrap().success());

        let c = attrs(&clone).unwrap();
        let du = std::fs::symlink_metadata(&clone).unwrap().blocks() * 512;
        assert!(du >= 4 << 20, "du counts the clone in full: {du}");
        assert_eq!(c.alloc, du);
        assert_eq!(c.private, Some(0), "every block of the clone is shared");

        // Rewriting part of the clone gives it private blocks of its own.
        let mut f = std::fs::OpenOptions::new().write(true).open(&clone).unwrap();
        std::io::Write::write_all(&mut f, &vec![7u8; 1 << 20]).unwrap();
        f.sync_all().unwrap();
        drop(f);
        assert_eq!(attrs(&clone).unwrap().private, Some(1 << 20));
    }

    #[test]
    fn sockets_are_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let _l = std::os::unix::net::UnixListener::bind(dir.path().join("s")).unwrap();
        let e = read_dir(dir.path()).unwrap();
        assert_eq!(e[0].attrs.kind, ObjKind::Socket);
    }
}

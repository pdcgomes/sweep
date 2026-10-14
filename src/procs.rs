//! A snapshot of running processes: their command lines, working folders and
//! the apps they belong to. "In use" checks consult it.

use std::collections::{HashMap, HashSet};
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: i32,
    pub cmd: String,
    pub exe: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
}

impl Proc {
    pub fn name(&self) -> String {
        let exe = self.exe.as_ref().and_then(|e| e.file_name()).map(|n| n.to_string_lossy().into_owned());
        exe.unwrap_or_else(|| {
            self.cmd.split_whitespace().next().unwrap_or("?").rsplit('/').next().unwrap_or("?").to_string()
        })
    }

    pub fn describe(&self) -> String {
        format!("{} (pid {})", self.name(), self.pid)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Procs {
    pub list: Vec<Proc>,
    /// Lower-cased bundle identifiers of running apps.
    bundles: HashSet<String>,
    /// Lower-cased executable names.
    names: HashSet<String>,
    /// Bundle identifier → app name, for messages.
    app_names: HashMap<String, String>,
    /// Folder → the processes mentioning it at all. Thousands of siblings in
    /// $TMPDIR then check a handful of command lines instead of every one.
    mentioning: Arc<Mutex<HashMap<PathBuf, Arc<Vec<usize>>>>>,
}

impl Procs {
    pub fn snapshot() -> Procs {
        let me = std::process::id() as i32;
        let mut cmds = ps("command=");
        let comms = ps("comm=");
        let mut list = Vec::new();
        for (pid, cmd) in cmds.drain() {
            if pid == me {
                continue;
            }
            let exe = pid_path(pid).or_else(|| comms.get(&pid).filter(|c| c.starts_with('/')).map(PathBuf::from));
            list.push(Proc { pid, cmd, exe, cwd: pid_cwd(pid) });
        }
        list.sort_by_key(|p| p.pid);
        Procs::from_list(list)
    }

    pub fn from_list(list: Vec<Proc>) -> Procs {
        let mut p = Procs { list, ..Procs::default() };
        let mut seen_apps = HashSet::new();
        for proc in &p.list {
            p.names.insert(proc.name().to_lowercase());
            if let Some(exe) = &proc.exe {
                for app in app_bundles(exe) {
                    if seen_apps.insert(app.clone())
                        && let Some(id) = bundle_id(&app)
                    {
                        let name = app.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        p.app_names.insert(id.to_lowercase(), name);
                        p.bundles.insert(id.to_lowercase());
                    }
                }
            }
        }
        p
    }

    /// A process whose command line names `path` (or something inside it), or
    /// whose working folder is inside it.
    pub fn using(&self, path: &Path) -> Option<&Proc> {
        let variants = path_variants(path);
        let uses = |p: &&Proc| {
            variants.iter().any(|v| names_path(&p.cmd, v))
                || p.cwd.as_ref().is_some_and(|cwd| variants.iter().any(|v| cwd.starts_with(v)))
        };
        match path.parent().filter(|p| p.parent().is_some()) {
            Some(parent) => self.mentioning(parent).iter().map(|&i| &self.list[i]).find(uses),
            None => self.list.iter().find(uses),
        }
    }

    fn mentioning(&self, dir: &Path) -> Arc<Vec<usize>> {
        if let Some(v) = self.mentioning.lock().unwrap().get(dir) {
            return v.clone();
        }
        let variants = path_variants(dir);
        let v: Vec<usize> = self
            .list
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                variants.iter().any(|d| p.cmd.contains(d.as_str()) || p.cwd.as_ref().is_some_and(|c| c.starts_with(d)))
            })
            .map(|(i, _)| i)
            .collect();
        let v = Arc::new(v);
        self.mentioning.lock().unwrap().insert(dir.to_path_buf(), v.clone());
        v
    }

    /// Whether the app owning a cache folder named `owner` (a bundle
    /// identifier like `com.apple.remindd`, or a tool's name) is running.
    pub fn app_running(&self, owner: &str) -> Option<String> {
        let o = owner.to_lowercase();
        if let Some(name) = self.app_names.get(&o) {
            return Some(format!("{name} is running"));
        }
        if self.bundles.contains(&o) || self.names.contains(&o) {
            return Some(format!("{owner} is running"));
        }
        if let Some(last) = o.rsplit('.').next()
            && o.contains('.')
            && last.len() > 3
            && self.names.contains(last)
        {
            return Some(format!("{last} is running"));
        }
        None
    }

    pub fn find(&self, pred: impl Fn(&Proc) -> bool) -> Option<&Proc> {
        self.list.iter().find(|p| pred(p))
    }
}

fn ps(field: &str) -> HashMap<i32, String> {
    let out = Command::new("/bin/ps").args(["-axww", "-o", "pid=", "-o", field]).output();
    let mut map = HashMap::new();
    if let Ok(out) = out {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(' ').unwrap_or((line, ""));
            if let Ok(pid) = pid.parse() {
                map.insert(pid, rest.trim_start().to_string());
            }
        }
    }
    map
}

fn pid_path(pid: i32) -> Option<PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(PathBuf::from(String::from_utf8_lossy(&buf).into_owned()))
}

fn pid_cwd(pid: i32) -> Option<PathBuf> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, &mut info as *mut _ as *mut libc::c_void, size)
    };
    if n != size {
        return None;
    }
    let raw = unsafe { CStr::from_ptr(info.pvi_cdir.vip_path.as_ptr() as *const libc::c_char) };
    let s = raw.to_string_lossy();
    if s.is_empty() || s == "/" { None } else { Some(PathBuf::from(s.into_owned())) }
}

/// Every `.app` bundle an executable lives in, innermost first.
fn app_bundles(exe: &Path) -> Vec<PathBuf> {
    exe.ancestors().filter(|a| a.extension().is_some_and(|e| e == "app")).map(Path::to_path_buf).collect()
}

/// An app's bundle identifier, read from its Info.plist once per run.
fn bundle_id(app: &Path) -> Option<String> {
    static KNOWN: std::sync::LazyLock<Mutex<HashMap<PathBuf, Option<String>>>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Some(id) = KNOWN.lock().unwrap().get(app) {
        return id.clone();
    }
    let read = || -> Option<String> {
        let v = plist::Value::from_file(app.join("Contents/Info.plist")).ok()?;
        v.as_dictionary()?.get("CFBundleIdentifier")?.as_string().map(str::to_string)
    };
    let id = read();
    KNOWN.lock().unwrap().insert(app.to_path_buf(), id.clone());
    id
}

/// The spellings a command line might use for a path: as given, and with or
/// without the `/private` prefix macOS hides behind `/var`, `/tmp` and `/etc`.
pub fn path_variants(path: &Path) -> Vec<String> {
    let mut v = vec![path.to_string_lossy().trim_end_matches('/').to_string()];
    let s = v[0].clone();
    if let Some(rest) = s.strip_prefix("/private") {
        if ["/var/", "/tmp/", "/etc/"].iter().any(|p| rest.starts_with(p)) {
            v.push(rest.to_string());
        }
    } else if ["/var/", "/tmp/", "/etc/"].iter().any(|p| s.starts_with(p)) {
        v.push(format!("/private{s}"));
    }
    if let Ok(real) = std::fs::canonicalize(path) {
        let r = real.to_string_lossy().to_string();
        if !v.contains(&r) {
            v.push(r);
        }
    }
    v
}

/// Whether `cmd` mentions `path` itself or something inside it, rather than a
/// longer name that merely starts the same way.
pub fn names_path(cmd: &str, path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let mut from = 0;
    while let Some(i) = cmd[from..].find(path) {
        let end = from + i + path.len();
        match cmd[end..].chars().next() {
            None => return true,
            Some(c) if !(c.is_alphanumeric() || "._-+@~".contains(c)) => return true,
            _ => {}
        }
        from = from + i + 1;
    }
    false
}

/// Whether some process is listening on the Unix socket at `path`:
/// `Some(true)` if a connection is accepted, `Some(false)` if nothing is
/// there to answer, `None` if it can't be told.
pub fn socket_live(path: &Path) -> Option<bool> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() {
        return None;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    addr.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
    unsafe {
        let fd = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return None;
        }
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        let rc = libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr.sun_len as libc::socklen_t);
        let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        libc::close(fd);
        if rc == 0 {
            return Some(true);
        }
        match err {
            libc::EINPROGRESS | libc::EAGAIN => Some(true),
            libc::ECONNREFUSED | libc::ENOENT | libc::ENOTSOCK => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_path_respects_name_boundaries() {
        let p = "/var/folders/x/T/tmp1";
        assert!(names_path("node render /var/folders/x/T/tmp1/index.html", p));
        assert!(names_path("tool --out=/var/folders/x/T/tmp1", p));
        assert!(names_path("tool '/var/folders/x/T/tmp1' -v", p));
        assert!(!names_path("tool /var/folders/x/T/tmp12/x", p));
        assert!(!names_path("tool /var/folders/x/T/tmp1.lock", p));
        assert!(names_path("a /var/folders/x/T/tmp12 b /var/folders/x/T/tmp1", p));
    }

    #[test]
    fn private_prefix_variants() {
        let v = path_variants(Path::new("/var/folders/ab/T/x"));
        assert!(v.contains(&"/private/var/folders/ab/T/x".to_string()));
        let v = path_variants(Path::new("/private/tmp/y"));
        assert!(v.contains(&"/tmp/y".to_string()));
    }

    #[test]
    fn a_process_naming_a_folder_marks_it_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("remotion-webpack-bundle-1");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("index.html"), "x").unwrap();
        let mut child = Command::new("/usr/bin/tail").arg("-f").arg(target.join("index.html")).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let procs = Procs::snapshot();
        let other = dir.path().join("remotion-webpack-bundle-10");
        let user = procs.using(&target).map(|p| p.pid);
        let other_user = procs.using(&other).map(|p| p.pid);
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(user, Some(child.id() as i32));
        assert_eq!(other_user, None);
    }

    #[test]
    fn a_working_folder_inside_marks_it_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let mut child = Command::new("/bin/sleep").arg("30").current_dir(wt.join("src")).spawn().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let procs = Procs::snapshot();
        let found = procs.list.iter().find(|p| p.pid == child.id() as i32).cloned();
        let user = procs.using(&wt).map(|p| p.pid);
        child.kill().unwrap();
        child.wait().unwrap();
        let found = found.expect("child in the process list");
        if found.cwd.is_some() {
            assert_eq!(user, Some(child.id() as i32));
        }
    }

    #[test]
    fn running_apps_by_bundle_id_and_name() {
        let procs = Procs::from_list(vec![
            Proc { pid: 1, cmd: "/usr/libexec/remindd".into(), exe: Some("/usr/libexec/remindd".into()), cwd: None },
            Proc { pid: 2, cmd: "bench --x".into(), exe: Some("/Users/u/bin/bench".into()), cwd: None },
            Proc {
                pid: 3,
                cmd: "/System/Applications/Calendar.app/Contents/MacOS/Calendar".into(),
                exe: Some("/System/Applications/Calendar.app/Contents/MacOS/Calendar".into()),
                cwd: None,
            },
        ]);
        assert!(procs.app_running("com.apple.remindd").is_some());
        assert!(procs.app_running("bench").is_some());
        assert!(procs.app_running("com.apple.iCal").is_some());
        assert!(procs.app_running("com.example.app").is_none());
        assert!(procs.app_running("com.adobe.lightroomCC").is_none());
    }

    #[test]
    fn socket_probe_tells_live_from_dead() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        let listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        let dead = dir.path().join("dead");
        drop(std::os::unix::net::UnixListener::bind(&dead).unwrap());
        assert_eq!(socket_live(&live), Some(true));
        assert_eq!(socket_live(&dead), Some(false));
        drop(listener);
    }
}

//! Everything a rule consults: where things live, the clock, the running
//! processes and what is always kept.

#[cfg(test)]
use crate::config::Keep;
use crate::config::{self, Config};
use crate::glob;
use crate::procs::Procs;
use std::ffi::CStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub struct Ctx {
    pub home: PathBuf,
    pub tmpdir: PathBuf,
    pub src_root: PathBuf,
    pub now: SystemTime,
    pub cfg: Config,
    pub procs: Procs,
    pub os_build: String,
    pub guard: Guard,
    /// Measure private sizes while checking. Off for the re-check just
    /// before deleting, which only needs dates; the scan's sizes stand.
    pub sizes: bool,
}

const DAY: f64 = 86_400.0;

impl Ctx {
    pub fn real(cfg: Config) -> Ctx {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
        let tmpdir = user_temp_dir().unwrap_or_else(std::env::temp_dir);
        Ctx::new(home, tmpdir, SystemTime::now(), cfg, Procs::snapshot(), os_build())
    }

    pub fn new(home: PathBuf, tmpdir: PathBuf, now: SystemTime, cfg: Config, procs: Procs, os_build: String) -> Ctx {
        let src_root = config::expand(&home, &cfg.src_root);
        let guard = Guard::new(&home, &cfg);
        Ctx { home, tmpdir, src_root, now, cfg, procs, os_build, guard, sizes: true }
    }

    pub fn walk_opts(&self) -> crate::walk::Opts<'static> {
        crate::walk::Opts { no_sizes: !self.sizes, ..Default::default() }
    }

    /// The same settings at another moment, with the given process list.
    pub fn at(&self, now: SystemTime, procs: Procs) -> Ctx {
        Ctx {
            home: self.home.clone(),
            tmpdir: self.tmpdir.clone(),
            src_root: self.src_root.clone(),
            now,
            cfg: self.cfg.clone(),
            procs,
            os_build: self.os_build.clone(),
            guard: Guard::new(&self.home, &self.cfg),
            sizes: self.sizes,
        }
    }

    /// The same settings with a fresh clock and process list, for the checks
    /// repeated just before deleting.
    pub fn refreshed(&self) -> Ctx {
        let procs = if self.procs.list.is_empty() { Procs::default() } else { Procs::snapshot() };
        let now = self.now + SystemTime::now().duration_since(self.now).unwrap_or_default();
        Ctx::new(self.home.clone(), self.tmpdir.clone(), now, self.cfg.clone(), procs, self.os_build.clone())
    }

    pub fn age_days(&self, t: SystemTime) -> f64 {
        match self.now.duration_since(t) {
            Ok(d) => d.as_secs_f64() / DAY,
            Err(_) => 0.0,
        }
    }

    pub fn ago(&self, t: SystemTime) -> String {
        crate::fmt::ago(self.now.duration_since(t).unwrap_or(Duration::ZERO))
    }

    pub fn cutoff(&self, days: f64) -> SystemTime {
        self.now - Duration::from_secs_f64(days.max(0.0) * DAY)
    }

    /// `~/x` for paths in the home folder.
    pub fn tilde(&self, p: &Path) -> String {
        match p.strip_prefix(&self.home) {
            Ok(rest) if !self.home.as_os_str().is_empty() && self.home != Path::new("/") => {
                format!("~/{}", rest.display())
            }
            _ => p.display().to_string(),
        }
    }
}

/// The per-user temporary folder `$TMPDIR` normally points to, straight from
/// the system, so a shell that overrides `TMPDIR` doesn't hide it.
pub fn user_temp_dir() -> Option<PathBuf> {
    let mut buf = vec![0u8; 1024];
    let n = unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if n == 0 || n > buf.len() {
        return std::env::var_os("TMPDIR").map(PathBuf::from);
    }
    let s = CStr::from_bytes_until_nul(&buf).ok()?.to_string_lossy().into_owned();
    Some(PathBuf::from(s))
}

pub fn os_build() -> String {
    let name = c"kern.osversion";
    let mut buf = vec![0u8; 64];
    let mut len = buf.len();
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr(), buf.as_mut_ptr() as *mut libc::c_void, &mut len, std::ptr::null_mut(), 0)
    };
    if rc != 0 {
        return String::new();
    }
    CStr::from_bytes_until_nul(&buf).map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Paths no rule may touch, whatever else they pass: the keep list. Like
/// macOS's file systems, it ignores case.
pub struct Guard {
    keep: Vec<Kept>,
}

struct Kept {
    path: PathBuf,
    lower: String,
    why: String,
}

impl Guard {
    pub fn new(home: &Path, cfg: &Config) -> Guard {
        let keep = cfg
            .keep
            .iter()
            .map(|k| {
                let why =
                    k.why().map_or_else(|| "on the keep list in config.toml".into(), |w| format!("always kept: {w}"));
                let path = config::expand(home, k.path().trim_end_matches('/'));
                Kept { lower: path.to_string_lossy().to_lowercase(), path, why }
            })
            .collect();
        Guard { keep }
    }

    /// Why `path` must stay, if it must.
    pub fn check(&self, path: &Path) -> Option<String> {
        let lower = PathBuf::from(path.to_string_lossy().to_lowercase());
        for k in &self.keep {
            if k.lower.contains(['*', '?']) {
                if lower.ancestors().any(|a| glob::matches(&k.lower, &a.to_string_lossy())) {
                    return Some(k.why.clone());
                }
            } else {
                let p = Path::new(&k.lower);
                if lower.starts_with(p) {
                    return Some(k.why.clone());
                }
                if p.starts_with(&lower) {
                    return Some(format!("contains {}, which is {}", k.path.display(), k.why));
                }
            }
        }
        None
    }
}

#[cfg(test)]
pub fn test_ctx(home: &Path, now: SystemTime) -> Ctx {
    let cfg = Config { scratch: Vec::new(), ..Config::default() };
    Ctx::new(home.to_path_buf(), home.join("T"), now, cfg, Procs::default(), "25G83".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keep_list_says_why() {
        let why = |path: &str, why: &str| Keep::Why { path: path.into(), why: why.into() };
        let cfg = Config {
            keep: vec![
                Keep::Path("~/src/app/build/wt-release".into()),
                why("~/Library/Application Support/Editor", "the editor's data"),
                why("~/Library/*/*chatapp*", "the chat app's data"),
            ],
            ..Config::default()
        };
        let g = Guard::new(Path::new("/Users/u"), &cfg);
        let check = |p: &str| g.check(Path::new(p));
        assert_eq!(check("/Users/u/src/app/build/wt-release").as_deref(), Some("on the keep list in config.toml"));
        assert_eq!(
            check("/Users/u/Library/Application Support/Editor/x").as_deref(),
            Some("always kept: the editor's data")
        );
        assert_eq!(
            check("/Users/u/Library/Application Support").as_deref(),
            Some("contains /Users/u/Library/Application Support/Editor, which is always kept: the editor's data")
        );
        assert!(check("/Users/u/Library/Caches/net.chatapp.Mac/blobs").is_some());
        assert!(check("/Users/u/Library/Logs/ChatApp").is_some(), "case doesn't matter");
        assert!(check("/Users/u/Library/application support/editor").is_some(), "nor here");
        assert!(check("/Users/u/src/app/build/wt-release-2").is_none());
        assert!(check("/Users/u/Library/Caches/pip").is_none());
    }

    #[test]
    fn keep_patterns() {
        let cfg = Config { keep: vec![Keep::Path("~/src/*/build/wt-keep-*".into())], ..Config::default() };
        let g = Guard::new(Path::new("/Users/u"), &cfg);
        assert!(g.check(Path::new("/Users/u/src/a/build/wt-keep-1")).is_some());
        assert!(g.check(Path::new("/Users/u/src/a/build/wt-keep-1/inner")).is_some());
        assert!(g.check(Path::new("/Users/u/src/a/build/wt-other")).is_none());
    }
}

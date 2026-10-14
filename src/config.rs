//! Per-Mac settings in ~/.config/sweep/config.toml.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Where to look for git repositories and projects.
    pub src_root: String,
    /// Paths (or patterns with `*`) never deleted, by any rule.
    pub keep: Vec<Keep>,
    /// Names (or patterns) of more $TMPDIR entries that tools leave behind.
    pub temp_patterns: Vec<String>,
    pub thresholds: Thresholds,
    pub scratch: Vec<Scratch>,
}

/// A path to keep, as a string or with the reason the interface shows.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Keep {
    Path(String),
    Why { path: String, why: String },
}

impl Keep {
    pub fn path(&self) -> &str {
        match self {
            Keep::Path(p) | Keep::Why { path: p, .. } => p,
        }
    }

    pub fn why(&self) -> Option<&str> {
        match self {
            Keep::Path(_) => None,
            Keep::Why { why, .. } => Some(why),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct Thresholds {
    pub worktree_edit_days: f64,
    pub worktree_build_days: f64,
    pub build_output_days: f64,
    pub temp_days: f64,
    pub caches_days: f64,
    pub derived_data_days: f64,
    pub derived_data_idle_days: f64,
    pub device_support_days: f64,
    pub coreml_days: f64,
    pub runtime_unused_days: f64,
    pub logs_days: f64,
    pub scratch_days: f64,
    pub downloads_days: f64,
    pub downloads_min_mb: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
#[derive(Default)]
pub struct Scratch {
    pub path: String,
    /// Top-level names (or patterns) to keep in this root.
    pub keep: Vec<String>,
    /// Overrides `thresholds.scratch_days` for this root.
    pub days: Option<f64>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            src_root: "~/src".into(),
            keep: Vec::new(),
            temp_patterns: Vec::new(),
            thresholds: Thresholds::default(),
            scratch: Vec::new(),
        }
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            worktree_edit_days: 2.0,
            worktree_build_days: 2.0,
            build_output_days: 14.0,
            temp_days: 2.0,
            caches_days: 7.0,
            derived_data_days: 2.0,
            derived_data_idle_days: 30.0,
            device_support_days: 90.0,
            coreml_days: 30.0,
            runtime_unused_days: 90.0,
            logs_days: 7.0,
            scratch_days: 7.0,
            downloads_days: 30.0,
            downloads_min_mb: 500,
        }
    }
}

pub const TEMPLATE: &str = r#"# sweep settings for this Mac. Every key is optional; a missing key takes the
# value shown here.

# Git worktrees of the repositories under this folder are candidates, and so
# is what builds leave in its projects. Each main checkout always stays.
src_root = "~/src"

# Never deleted by any rule: paths, or patterns where * matches within a
# name. An entry can say why, and the interface shows it.
keep = [
  # "~/src/my-app/build/release",
  # { path = "~/Library/Caches/Yarn", why = "offline installs need it" },
]

# More $TMPDIR entries your tools leave behind, by name or pattern.
temp_patterns = [
  # "my-tests-*",
]

[thresholds]
worktree_edit_days = 2     # no file outside build/ edited for this long
worktree_build_days = 2    # nothing written under build/ for this long
build_output_days = 14     # projects under src_root nobody has edited, for their build output
temp_days = 2              # $TMPDIR entries
caches_days = 7            # apps' caches
derived_data_days = 2      # Xcode DerivedData of deleted projects
derived_data_idle_days = 30  # ...and of projects that still exist, for review
device_support_days = 90   # symbols Xcode copied from a device not connected since
coreml_days = 30           # compiled Core ML models for the running macOS
runtime_unused_days = 90   # simulator runtimes
logs_days = 7              # files in ~/Library/Logs
scratch_days = 7           # top-level folders of the scratch roots
downloads_days = 30        # files in ~/Downloads...
downloads_min_mb = 500     # ...at least this big

# Scratch roots: their top-level folders go once idle, except the ones kept.
# [[scratch]]
# path = "/Volumes/Scratch/tmp"
# keep = ["fixtures", "models-*"]
# days = 7   # overrides thresholds.scratch_days for this root
"#;

pub fn path(home: &Path) -> PathBuf {
    home.join(".config/sweep/config.toml")
}

/// Loads the config, writing the commented default first if there is none.
pub fn load_or_create(home: &Path) -> Result<(Config, PathBuf, bool)> {
    let p = path(home);
    let mut created = false;
    if !p.exists() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        created = std::fs::write(&p, TEMPLATE).is_ok();
    }
    let cfg = match std::fs::read_to_string(&p) {
        Ok(text) => parse(&text).with_context(|| format!("reading {}", p.display()))?,
        Err(_) => Config::default(),
    };
    Ok((cfg, p, created))
}

pub fn parse(text: &str) -> Result<Config> {
    Ok(toml::from_str(text)?)
}

/// Expands a leading `~`.
pub fn expand(home: &Path, s: &str) -> PathBuf {
    if s == "~" {
        home.to_path_buf()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_template_is_the_default() {
        assert_eq!(parse(TEMPLATE).unwrap(), Config::default());
    }

    #[test]
    fn missing_keys_take_defaults() {
        let c = parse("[thresholds]\ntemp_days = 5\n").unwrap();
        assert_eq!(c.thresholds.temp_days, 5.0);
        assert_eq!(c.thresholds.logs_days, 7.0);
        assert_eq!(c.src_root, "~/src");
    }

    #[test]
    fn scratch_roots_parse() {
        let c = parse("[[scratch]]\npath = \"/tmp/x\"\ndays = 3\n").unwrap();
        assert_eq!(c.scratch.len(), 1);
        assert_eq!(c.scratch[0].days, Some(3.0));
        assert!(c.scratch[0].keep.is_empty());
    }

    #[test]
    fn keep_entries_with_and_without_a_reason() {
        let c = parse("keep = [\"~/a\", { path = \"~/.npm\", why = \"offline installs\" }]\n").unwrap();
        assert_eq!(
            c.keep,
            [Keep::Path("~/a".into()), Keep::Why { path: "~/.npm".into(), why: "offline installs".into() }]
        );
        assert_eq!((c.keep[1].path(), c.keep[1].why()), ("~/.npm", Some("offline installs")));
    }
}

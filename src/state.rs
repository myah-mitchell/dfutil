//! Persistent records of what dfapp installed and deployed, and a lock so two
//! runs (say, the scheduled update and an open TUI) never write at once.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How a tool version was installed.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Prebuilt,
    Cargo,
    Npm,
    Uv,
    Script,
}

impl std::fmt::Display for Method {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Method::Prebuilt => "prebuilt",
            Method::Cargo => "cargo",
            Method::Npm => "npm",
            Method::Uv => "uv",
            Method::Script => "script",
        })
    }
}

/// One installed version of a tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct InstalledVersion {
    /// Version string (release tag, package version, or what the version
    /// command printed).
    pub version: String,
    /// Directory holding this version's files, when dfapp owns one.
    pub install_dir: Option<PathBuf>,
    /// Links dfapp created for this version (bin links and `links` entries).
    pub links: Vec<PathBuf>,
}

/// Everything dfapp knows about one installed tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolRecord {
    pub current: InstalledVersion,
    /// The version replaced by the last install, kept for `dfapp rollback`.
    pub previous: Option<InstalledVersion>,
    pub method: Method,
    /// Source the version came from (`owner/name`, npm or PyPI package, crate).
    pub source: Option<String>,
    /// Recipe fingerprint at install time; see `ToolDef::fingerprint`.
    pub fingerprint: String,
    /// SHA-256 of the downloaded asset, for prebuilt installs.
    pub sha256: Option<String>,
    /// Whether that hash was checked against a published checksum.
    pub verified: bool,
    pub installed_at: u64,
}

/// Tool records for one scope.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ToolState {
    pub tools: BTreeMap<String, ToolRecord>,
}

/// A rendered template behind a deployed link.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct RenderRecord {
    /// The `.tmpl` file in the source repo.
    pub template: PathBuf,
    /// Where the rendered output lives (the link's target).
    pub output: PathBuf,
    /// SHA-256 of the output as dfapp last wrote it; a different hash on disk
    /// means it was edited in place and has something to capture.
    pub sha256: String,
}

/// One symlink dfapp created in `$HOME`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct LinkRecord {
    /// What the link points at: a repo file or a rendered output.
    pub source: PathBuf,
    pub package: String,
    /// Source repo the file came from (`main` or a secondary source's name).
    pub root: String,
    pub rendered: Option<RenderRecord>,
}

/// Dotfile links for one user, keyed by link path.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DeployState {
    pub links: BTreeMap<PathBuf, LinkRecord>,
}

/// Reads a JSON state file, returning the default when it doesn't exist yet.
pub fn load<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Writes a JSON state file atomically.
pub fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    crate::util::write_atomic(path, text.as_bytes())
}

/// An exclusive lock held for the duration of a run that changes state.
/// Released (the file removed) on drop.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Takes the lock in `state_dir`, failing if another live dfapp process
    /// holds it. A lock left by a process that no longer exists is reclaimed.
    pub fn acquire(state_dir: &Path) -> Result<Lock> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let path = state_dir.join("dfapp.lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    write!(file, "{}", std::process::id())?;
                    return Ok(Lock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid: Option<u32> = holder.trim().parse().ok();
                    let alive = pid.is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists());
                    if alive && pid != Some(std::process::id()) {
                        bail!(
                            "another dfapp run (pid {}) is in progress; wait for it or remove {}",
                            pid.unwrap_or_default(),
                            path.display()
                        );
                    }
                    std::fs::remove_file(&path).ok();
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        bail!("couldn't take the lock at {}", path.display())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_state_loads_as_default_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/tools.json");
        let mut state: ToolState = load(&path).unwrap();
        assert!(state.tools.is_empty());
        state.tools.insert(
            "rg".into(),
            ToolRecord {
                current: InstalledVersion {
                    version: "14.1.0".into(),
                    install_dir: Some("/x".into()),
                    links: vec!["/bin/rg".into()],
                },
                previous: None,
                method: Method::Prebuilt,
                source: Some("BurntSushi/ripgrep".into()),
                fingerprint: "abc".into(),
                sha256: None,
                verified: false,
                installed_at: 1,
            },
        );
        save(&path, &state).unwrap();
        let loaded: ToolState = load(&path).unwrap();
        assert_eq!(loaded.tools["rg"], state.tools["rg"]);
    }

    #[test]
    fn lock_is_exclusive_and_reclaims_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("dfapp.lock"), "999999999").unwrap();
        let lock = Lock::acquire(dir.path()).unwrap();
        assert!(dir.path().join("dfapp.lock").exists());
        drop(lock);
        assert!(!dir.path().join("dfapp.lock").exists());
        // pid 1 is always alive, so its lock is respected.
        std::fs::write(dir.path().join("dfapp.lock"), "1").unwrap();
        assert!(Lock::acquire(dir.path()).is_err());
    }
}

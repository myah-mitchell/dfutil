//! Persistent records of what dfutil installed and deployed, and a lock so two
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
    /// Directory holding this version's files, when dfutil owns one.
    pub install_dir: Option<PathBuf>,
    /// Links dfutil created for this version (bin links and `links` entries).
    pub links: Vec<PathBuf>,
}

/// Everything dfutil knows about one installed tool.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolRecord {
    pub current: InstalledVersion,
    /// The version replaced by the last install, kept for `dfutil rollback`.
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
    /// Set by a rollback to the version rolled back from, so updates skip it
    /// until a different version is available or a reinstall is asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_version: Option<String>,
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
    /// SHA-256 of the output as dfutil last wrote it; a different hash on disk
    /// means it was edited in place and has something to capture.
    pub sha256: String,
}

/// One symlink dfutil created in `$HOME`.
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
///
/// It's an advisory `flock` on `dfutil.lock`, so the kernel releases it when
/// the holding process exits, however it exits. The file holds the holder's
/// pid for the error message only. Acquiring a lock this process already
/// holds succeeds, so nested operations (sync, then deploy) don't deadlock.
pub struct Lock {
    path: PathBuf,
    /// The locked file; `None` for a nested acquire.
    file: Option<std::fs::File>,
}

fn held_locks() -> &'static std::sync::Mutex<BTreeMap<PathBuf, usize>> {
    static HELD: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, usize>>> =
        std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}

impl Lock {
    /// Takes the lock in `state_dir`, failing if another dfutil process holds it.
    pub fn acquire(state_dir: &Path) -> Result<Lock> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let path = state_dir.join("dfutil.lock");
        let mut held = held_locks()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(count) = held.get_mut(&path) {
            *count += 1;
            return Ok(Lock { path, file: None });
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                bail!(
                    "another dfutil run (pid {}) is in progress; wait for it to finish",
                    holder.trim()
                );
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).with_context(|| format!("locking {}", path.display()));
            }
        }
        use std::io::{Seek, Write};
        file.set_len(0)?;
        file.rewind()?;
        write!(file, "{}", std::process::id())?;
        held.insert(path.clone(), 1);
        Ok(Lock {
            path,
            file: Some(file),
        })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let mut held = held_locks()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(count) = held.get_mut(&self.path) {
            *count -= 1;
            if *count == 0 {
                held.remove(&self.path);
            }
        }
        // Unlock explicitly rather than relying on close: a child forked by
        // another thread can hold a copy of the descriptor until it execs, and
        // the lock lasts while any copy is open. The file itself stays;
        // deleting it would let a waiting process lock a different inode.
        if let Some(file) = &self.file {
            file.unlock().ok();
        }
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
                skip_version: None,
            },
        );
        save(&path, &state).unwrap();
        let loaded: ToolState = load(&path).unwrap();
        assert_eq!(loaded.tools["rg"], state.tools["rg"]);
    }

    #[test]
    fn lock_is_exclusive_nests_and_ignores_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfutil.lock");
        // A leftover file from a crashed run doesn't block anything.
        std::fs::write(&path, "999999999").unwrap();
        let outer = Lock::acquire(dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            std::process::id().to_string()
        );
        // Another open file description, as another process would have, can't lock it.
        let other = std::fs::File::open(&path).unwrap();
        assert!(matches!(
            other.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        // This process can take it again, and the lock lasts until both drop.
        let inner = Lock::acquire(dir.path()).unwrap();
        drop(inner);
        assert!(other.try_lock().is_err());
        drop(outer);
        other.try_lock().unwrap();
    }
}

//! Filesystem locations dfapp reads and writes, for user and system scope.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Where tools are installed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Under `~/.local`, for the current user only.
    #[default]
    User,
    /// Under `/usr/local`, for every user. Needs write access there.
    System,
}

impl std::fmt::Display for Scope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Scope::User => "user",
            Scope::System => "system",
        })
    }
}

/// All paths for one host and scope. Dotfile deployment, rendered templates,
/// init scripts, caches, and host config are always per user; only the tool
/// install locations and the tool state file move with the scope.
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub scope: Scope,
    /// `~/.local` or `/usr/local`.
    pub prefix: PathBuf,
    /// `<prefix>/bin`, where tool binaries are linked.
    pub bin_dir: PathBuf,
    /// Versioned tool install directories.
    pub tools_dir: PathBuf,
    /// Tool install state for this scope.
    pub tool_state: PathBuf,
    /// Dotfile link and render state (always per user).
    pub deploy_state: PathBuf,
    /// Rendered template output that dotfile links point at.
    pub rendered_dir: PathBuf,
    /// Generated shell init scripts.
    pub init_dir: PathBuf,
    /// HTTP response cache.
    pub cache_dir: PathBuf,
    /// Directory holding `host.toml`.
    pub config_dir: PathBuf,
    /// Checkouts of secondary source repos.
    pub sources_dir: PathBuf,
    /// Lock file and logs.
    pub state_dir: PathBuf,
}

impl Paths {
    /// Builds the paths from `$HOME` and the XDG base-directory variables.
    pub fn from_env(scope: Scope) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let xdg = |variable: &str, fallback: &str| {
            std::env::var_os(variable)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
        };
        Self::build(
            home.clone(),
            scope,
            xdg("XDG_DATA_HOME", ".local/share"),
            xdg("XDG_STATE_HOME", ".local/state"),
            xdg("XDG_CACHE_HOME", ".cache"),
            xdg("XDG_CONFIG_HOME", ".config"),
        )
    }

    /// Builds the paths for a given home directory, ignoring XDG variables.
    /// Used by tests and by anything that operates on a scratch home.
    pub fn for_home(home: &Path, scope: Scope) -> Self {
        Self::build(
            home.to_path_buf(),
            scope,
            home.join(".local/share"),
            home.join(".local/state"),
            home.join(".cache"),
            home.join(".config"),
        )
    }

    fn build(
        home: PathBuf,
        scope: Scope,
        data: PathBuf,
        state: PathBuf,
        cache: PathBuf,
        config: PathBuf,
    ) -> Self {
        let state_dir = state.join("dfapp");
        let (prefix, tools_dir, tool_state) = match scope {
            Scope::User => (
                home.join(".local"),
                data.join("dfapp/tools"),
                state_dir.join("tools.json"),
            ),
            Scope::System => (
                PathBuf::from("/usr/local"),
                PathBuf::from("/usr/local/lib/dfapp/tools"),
                PathBuf::from("/var/lib/dfapp/tools.json"),
            ),
        };
        Self {
            bin_dir: prefix.join("bin"),
            prefix,
            tools_dir,
            tool_state,
            deploy_state: state_dir.join("deploy.json"),
            rendered_dir: data.join("dfapp/rendered"),
            init_dir: data.join("dfapp/init"),
            cache_dir: cache.join("dfapp"),
            config_dir: config.join("dfapp"),
            sources_dir: data.join("dfapp/sources"),
            state_dir,
            home,
            scope,
        }
    }

    /// Default location of the host config file.
    pub fn host_config(&self) -> PathBuf {
        self.config_dir.join("host.toml")
    }

    /// Checks that the scope's install locations are writable, so a system
    /// install run without permission fails up front with a clear message
    /// instead of partway through.
    pub fn check_writable(&self) -> anyhow::Result<()> {
        for directory in [&self.bin_dir, &self.tools_dir] {
            let existing = directory
                .ancestors()
                .find(|ancestor| ancestor.exists())
                .unwrap_or(Path::new("/"));
            let probe = tempfile::Builder::new()
                .prefix(".dfapp-write-check")
                .tempfile_in(existing);
            if probe.is_err() {
                anyhow::bail!(
                    "{} isn't writable by this user; {} scope needs write access there \
                     (run dfapp as a user that has it, or use user scope)",
                    existing.display(),
                    self.scope
                );
            }
        }
        if let Some(parent) = self.tool_state.parent() {
            let existing = parent
                .ancestors()
                .find(|ancestor| ancestor.exists())
                .unwrap_or(Path::new("/"));
            if tempfile::tempfile_in(existing).is_err() {
                anyhow::bail!(
                    "{} isn't writable by this user, so the {} scope's state file can't be saved",
                    existing.display(),
                    self.scope
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_scope_stays_under_home() {
        let paths = Paths::for_home(Path::new("/home/u"), Scope::User);
        assert_eq!(paths.bin_dir, Path::new("/home/u/.local/bin"));
        assert!(paths.tools_dir.starts_with("/home/u"));
        assert!(paths.tool_state.starts_with("/home/u"));
    }

    #[test]
    fn system_scope_moves_only_tool_locations() {
        let paths = Paths::for_home(Path::new("/home/u"), Scope::System);
        assert_eq!(paths.bin_dir, Path::new("/usr/local/bin"));
        assert_eq!(paths.tool_state, Path::new("/var/lib/dfapp/tools.json"));
        assert!(paths.deploy_state.starts_with("/home/u"));
        assert!(paths.rendered_dir.starts_with("/home/u"));
    }
}

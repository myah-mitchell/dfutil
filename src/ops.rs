//! High-level operations shared by the CLI and the TUI. Each takes the
//! state lock for its duration, so a scheduled update and an open TUI never
//! write at once.

use crate::config::{HostConfig, Workspace};
use crate::deploy;
use crate::gitops::{self, SyncReport, SyncResult};
use crate::plan::{self, Mode, Options, Plan};
use crate::remote::Remote;
use crate::selection::Selection;
use crate::state::{self, DeployState, Lock, ToolState};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Concurrent version lookups.
const LOOKUP_THREADS: usize = 8;

/// A loaded host: workspace, selection, and an HTTP client.
pub struct Session {
    pub host_path: PathBuf,
    pub home_override: Option<PathBuf>,
    pub workspace: Workspace,
    pub selection: Selection,
    pub remote: Remote,
}

/// Progress callback.
pub type Log<'a> = &'a (dyn Fn(String) + Sync);

impl Session {
    pub fn load(host_path: &Path, home_override: Option<&Path>, offline: bool) -> Result<Self> {
        let workspace = Workspace::load(host_path, home_override)?;
        let selection = Selection::compute(&workspace)?;
        let ttl = Duration::from_secs(workspace.config.settings.cache_ttl.unwrap_or(3600));
        let remote = Remote::new(&workspace.paths.cache_dir, ttl, offline);
        Ok(Self {
            host_path: host_path.to_path_buf(),
            home_override: home_override.map(Path::to_path_buf),
            workspace,
            selection,
            remote,
        })
    }

    /// Re-reads every config file (after a sync or an edit).
    pub fn reload(&mut self) -> Result<()> {
        self.workspace = Workspace::load(&self.host_path, self.home_override.as_deref())?;
        self.selection = Selection::compute(&self.workspace)?;
        Ok(())
    }

    fn lock(&self) -> Result<Vec<Lock>> {
        let paths = &self.workspace.paths;
        let mut locks = vec![Lock::acquire(&paths.state_dir)?];
        if let Some(directory) = paths.tool_state.parent()
            && directory != paths.state_dir
        {
            locks.push(Lock::acquire(directory)?);
        }
        Ok(locks)
    }

    pub fn tool_state(&self) -> Result<ToolState> {
        state::load(&self.workspace.paths.tool_state)
    }

    pub fn deploy_state(&self) -> Result<DeployState> {
        state::load(&self.workspace.paths.deploy_state)
    }

    /// Plans tool changes. With `everything`, also looks up versions of
    /// tools that aren't selected, for listing.
    pub fn plan(&self, options: &Options, everything: bool) -> Result<Plan> {
        let state = self.tool_state()?;
        let mut names =
            plan::names_to_resolve(&self.workspace, &self.selection, &state, everything);
        if let Some(only) = &options.only {
            for name in only {
                if !self.workspace.config.tools.contains_key(name) {
                    bail!("unknown tool `{name}`");
                }
            }
            if !everything {
                names.retain(|name| only.contains(name));
            }
        }
        let resolved = plan::resolve_all(&self.workspace, &self.remote, &names, LOOKUP_THREADS);
        Ok(plan::build(
            &self.workspace,
            &self.selection,
            &state,
            &resolved,
            options,
        ))
    }

    /// Installs, upgrades, and removes tools per `options`, then deploys
    /// dotfiles unless `options.only` restricts the run to some tools.
    pub fn apply(&self, options: &Options, log: Log) -> Result<ApplyReport> {
        let _locks = self.lock()?;
        let plan = self.plan(options, false)?;
        self.apply_plan(&plan, options.only.is_none(), log)
    }

    /// Applies an already computed plan (the TUI shows it first).
    pub fn apply_plan(&self, plan: &Plan, with_deploy: bool, log: Log) -> Result<ApplyReport> {
        if plan.has_changes() {
            self.workspace.paths.check_writable()?;
        }
        let state = self.tool_state()?;
        let (_, tools) = plan::apply(&self.workspace, plan, &self.remote, state, log)?;
        let errors: Vec<String> = plan
            .errors()
            .map(|row| format!("{}: {}", row.name, row.error.clone().unwrap_or_default()))
            .collect();
        let dotfiles = if with_deploy {
            self.deploy_locked(&deploy::Options::default())?
        } else {
            Vec::new()
        };
        Ok(ApplyReport {
            tools,
            resolve_errors: errors,
            dotfiles,
            sync: Vec::new(),
            notes: self.workspace.notes.clone(),
        })
    }

    pub fn deploy(&self, options: &deploy::Options) -> Result<Vec<deploy::Item>> {
        let _locks = if options.dry_run {
            Vec::new()
        } else {
            self.lock()?
        };
        self.deploy_locked(options)
    }

    fn deploy_locked(&self, options: &deploy::Options) -> Result<Vec<deploy::Item>> {
        let mut state = self.deploy_state()?;
        let items = deploy::deploy(&self.workspace, &self.selection, &mut state, options)?;
        if !options.dry_run {
            state::save(&self.workspace.paths.deploy_state, &state)?;
        }
        Ok(items)
    }

    /// Maps in-place edits of rendered files back onto their templates.
    pub fn capture(&self, write: bool) -> Result<Vec<deploy::Edited>> {
        let _locks = if write { self.lock()? } else { Vec::new() };
        let mut state = self.deploy_state()?;
        let edited = deploy::capture_all(&self.workspace, &mut state, write)?;
        if write {
            state::save(&self.workspace.paths.deploy_state, &state)?;
        }
        Ok(edited)
    }

    /// Clones missing sources, captures template edits, pulls every repo
    /// (stashing and reapplying local changes), reloads the config, and
    /// redeploys dotfiles.
    pub fn sync(&mut self, log: Log) -> Result<ApplyReport> {
        let _locks = self.lock()?;
        self.sync_locked(log)
    }

    fn sync_locked(&mut self, log: Log) -> Result<ApplyReport> {
        gitops::clone_missing(&self.workspace, &log)?;
        self.reload()?;
        let mut state = self.deploy_state()?;
        let captured = deploy::capture_all(&self.workspace, &mut state, true)?;
        state::save(&self.workspace.paths.deploy_state, &state)?;
        for edited in &captured {
            match &edited.problem {
                None => log(format!(
                    "captured edits to {} into {}",
                    edited.target.display(),
                    edited.template.display()
                )),
                Some(problem) => log(format!("{}: {problem}", edited.target.display())),
            }
        }
        let sync = gitops::sync_all(&self.workspace);
        self.reload()?;
        let dotfiles = self.deploy_locked(&deploy::Options::default())?;
        Ok(ApplyReport {
            tools: Vec::new(),
            resolve_errors: Vec::new(),
            dotfiles,
            sync,
            notes: self.workspace.notes.clone(),
        })
    }

    /// The scheduled job: sync, then install whatever changed.
    pub fn update(&mut self, log: Log) -> Result<ApplyReport> {
        let _locks = self.lock()?;
        let mut report = self.sync_locked(log)?;
        let plan = self.plan(&Options::default(), false)?;
        let state = self.tool_state()?;
        if plan.has_changes() {
            self.workspace.paths.check_writable()?;
        }
        let (_, tools) = plan::apply(&self.workspace, &plan, &self.remote, state, log)?;
        report.tools = tools;
        report.resolve_errors = plan
            .errors()
            .map(|row| format!("{}: {}", row.name, row.error.clone().unwrap_or_default()))
            .collect();
        Ok(report)
    }

    /// One-off install: adds tools to this host's `tools` (or drops them
    /// from `exclude_tools`) and installs just those.
    pub fn install(&mut self, names: &[String], log: Log) -> Result<ApplyReport> {
        for name in names {
            if !self.workspace.config.tools.contains_key(name) {
                bail!("unknown tool `{name}`; add it with `dfapp tool add` first");
            }
        }
        for name in names {
            crate::config_edit::host_list(&self.host_path, "exclude_tools", name, false)?;
        }
        self.reload()?;
        // Only list tools no preset already selects.
        for name in names {
            if !self.selection.tools.contains(name) {
                crate::config_edit::host_list(&self.host_path, "tools", name, true)?;
            }
        }
        self.reload()?;
        let only: BTreeSet<String> = names.iter().cloned().collect();
        self.apply(
            &Options {
                mode: Mode::Changed,
                only: Some(only),
            },
            log,
        )
    }

    /// Removes tools from this host: drops them from `tools`, and excludes
    /// them when a preset still selects them.
    pub fn uninstall(&mut self, names: &[String], log: Log) -> Result<ApplyReport> {
        for name in names {
            crate::config_edit::host_list(&self.host_path, "tools", name, false)?;
        }
        self.reload()?;
        for name in names {
            if self.selection.tools.contains(name) {
                crate::config_edit::host_list(&self.host_path, "exclude_tools", name, true)?;
            }
        }
        self.reload()?;
        for name in names {
            if self.selection.tools.contains(name) {
                bail!(
                    "{name} is still required by another selected tool ({})",
                    self.selection.reasons[name].join(", ")
                );
            }
        }
        let only: BTreeSet<String> = names.iter().cloned().collect();
        self.apply(
            &Options {
                mode: Mode::Changed,
                only: Some(only),
            },
            log,
        )
    }

    /// Switches a tool back to the version it had before its last install.
    pub fn rollback(&self, name: &str) -> Result<String> {
        let _locks = self.lock()?;
        let mut state = self.tool_state()?;
        let record = state
            .tools
            .get(name)
            .with_context(|| format!("{name} isn't installed"))?;
        let rolled = crate::install::rollback(&self.workspace.paths, name, record)?;
        let version = rolled.current.version.clone();
        state.tools.insert(name.to_string(), rolled);
        state::save(&self.workspace.paths.tool_state, &state)?;
        Ok(version)
    }

    /// Switches the palette and re-renders templates.
    pub fn set_palette(&mut self, name: &str) -> Result<Vec<deploy::Item>> {
        if !self.workspace.palettes.contains_key(name) {
            bail!(
                "no palette named `{name}`; available: {}",
                self.workspace
                    .palettes
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        crate::config_edit::host_set(&self.host_path, "palette", Some(name))?;
        self.reload()?;
        self.deploy(&deploy::Options::default())
    }

    /// Path of the host config used for scheduling and hooks.
    pub fn host_path(&self) -> &Path {
        &self.host_path
    }
}

pub fn describe_sync(report: &SyncReport) -> String {
    let stash = if report.stashed {
        " (local changes stashed and reapplied)"
    } else {
        ""
    };
    match &report.result {
        SyncResult::UpToDate => format!("up to date{stash}"),
        SyncResult::Updated { from, to } => format!("updated {from}..{to}{stash}"),
        SyncResult::RolledBack { reason } => format!("not updated, repo left as it was: {reason}"),
        SyncResult::Failed { reason } => format!("failed: {reason}"),
    }
}

/// Everything an apply, sync, or update did, for printing or `--json`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyReport {
    pub tools: Vec<plan::Outcome>,
    pub resolve_errors: Vec<String>,
    pub dotfiles: Vec<deploy::Item>,
    pub sync: Vec<SyncReport>,
    pub notes: Vec<String>,
}

impl ApplyReport {
    pub fn changed(&self) -> bool {
        self.tools.iter().any(|outcome| outcome.ok)
            || self.dotfiles.iter().any(|item| {
                !matches!(
                    item.change,
                    deploy::Change::Unchanged | deploy::Change::Skipped | deploy::Change::Error
                )
            })
            || self
                .sync
                .iter()
                .any(|report| matches!(report.result, SyncResult::Updated { .. }))
    }

    pub fn failed(&self) -> bool {
        self.tools.iter().any(|outcome| !outcome.ok)
            || !self.resolve_errors.is_empty()
            || self
                .dotfiles
                .iter()
                .any(|item| item.change == deploy::Change::Error)
            || self.sync.iter().any(|report| {
                matches!(
                    report.result,
                    SyncResult::RolledBack { .. } | SyncResult::Failed { .. }
                )
            })
    }
}

/// Sets up a new host: writes `host.toml` pointing at the content repo,
/// cloning it first when given a URL.
pub fn init(
    host_path: &Path,
    home: &Path,
    repo: &str,
    clone_to: Option<&Path>,
    presets: Vec<String>,
    scope: Option<crate::paths::Scope>,
    log: Log,
) -> Result<PathBuf> {
    let looks_remote = repo.contains("://")
        || repo.starts_with("git@")
        || (repo.contains(':') && !repo.starts_with('/') && !repo.starts_with('~'));
    let checkout = if looks_remote {
        let destination = clone_to
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("dotfiles"));
        if destination.exists() {
            bail!(
                "{} already exists; pass --clone-to or point --repo at it",
                destination.display()
            );
        }
        log(format!("cloning {repo} into {}", destination.display()));
        crate::util::run_checked(
            std::process::Command::new("git")
                .env("GIT_TERMINAL_PROMPT", "0")
                .args(["clone", "--quiet", "--", repo])
                .arg(&destination),
        )?;
        destination
    } else {
        crate::util::expand_tilde(repo, home)
    };
    let config_file = checkout.join(crate::config::REPO_CONFIG_FILE);
    let config = crate::config::RepoConfig::load(&config_file)?;
    let presets = if presets.is_empty() {
        config.settings.presets.clone()
    } else {
        presets
    };
    for preset in &presets {
        if !config.presets.contains_key(preset) {
            bail!("{}: no preset named `{preset}`", config_file.display());
        }
    }
    let repo_text = match checkout.strip_prefix(home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => checkout.display().to_string(),
    };
    let missing: Vec<String> = config
        .vars
        .iter()
        .filter(|(_, definition)| definition.default_value().is_none())
        .map(|(name, _)| name.clone())
        .collect();
    let host = HostConfig {
        repo: repo_text,
        presets,
        scope,
        ..HostConfig::default()
    };
    if let Some(parent) = host_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::config_edit::create_host(host_path, &host)?;
    if !missing.is_empty() {
        log(format!(
            "set these template variables before deploying: {} (`dfapp var set <name> <value>`)",
            missing.join(", ")
        ));
    }
    Ok(checkout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO: &str = r#"
[tools.hello]
kind = "script"
install = "mkdir -p \"$DFAPP_TOOL_DIR\" && printf '#!/bin/sh\necho hi\n' > \"$DFAPP_TOOL_DIR/hello\" && chmod +x \"$DFAPP_TOOL_DIR/hello\""
version = "echo 1.0.0"
bins = ["hello=hello"]

[tools.extra]
kind = "script"
install = "true"
version = "echo 2.0.0"

[presets.base]
tools = ["hello"]
"#;

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("dfapp.toml"), REPO).unwrap();
        let host = home.path().join(".config/dfapp/host.toml");
        let quiet = |_: String| {};
        init(
            &host,
            home.path(),
            "~/dotfiles",
            None,
            vec!["base".into()],
            None,
            &quiet,
        )
        .unwrap();
        (home, host)
    }

    fn host_lists(host: &Path) -> (Vec<String>, Vec<String>) {
        let config = HostConfig::load(host).unwrap();
        (config.tools, config.exclude_tools)
    }

    #[test]
    fn install_and_uninstall_edit_the_host_config_minimally() {
        let (home, host) = setup();
        let quiet = |_: String| {};
        let mut session = Session::load(&host, Some(home.path()), true).unwrap();
        let report = session.apply(&Options::default(), &quiet).unwrap();
        assert!(!report.failed(), "{report:?}");
        assert!(home.path().join(".local/bin/hello").exists());

        // A preset tool is excluded, not just dropped from `tools`.
        let report = session.uninstall(&["hello".into()], &quiet).unwrap();
        assert!(!report.failed(), "{report:?}");
        assert!(!home.path().join(".local/bin/hello").exists());
        assert_eq!(host_lists(&host), (vec![], vec!["hello".to_string()]));

        // Reinstalling a preset tool only lifts the exclusion.
        session.install(&["hello".into()], &quiet).unwrap();
        assert_eq!(host_lists(&host), (vec![], vec![]));
        assert!(home.path().join(".local/bin/hello").exists());

        // A tool outside every preset goes into `tools`, and back out.
        session.install(&["extra".into()], &quiet).unwrap();
        assert_eq!(host_lists(&host), (vec!["extra".to_string()], vec![]));
        assert!(session.tool_state().unwrap().tools.contains_key("extra"));
        session.uninstall(&["extra".into()], &quiet).unwrap();
        assert_eq!(host_lists(&host), (vec![], vec![]));
        assert!(!session.tool_state().unwrap().tools.contains_key("extra"));

        assert!(session.install(&["nope".into()], &quiet).is_err());
    }

    #[test]
    fn init_refuses_unknown_presets_and_existing_host_files() {
        let (home, host) = setup();
        let quiet = |_: String| {};
        assert!(init(&host, home.path(), "~/dotfiles", None, vec![], None, &quiet).is_err());
        let other = home.path().join("other.toml");
        assert!(
            init(
                &other,
                home.path(),
                "~/dotfiles",
                None,
                vec!["missing".into()],
                None,
                &quiet
            )
            .is_err()
        );
    }
}

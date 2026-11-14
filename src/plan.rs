//! Comparing what a host should have with what it has, and carrying out the
//! difference.
//!
//! Planning is split from resolving so it can be tested without a network:
//! [`resolve_all`] looks up versions (in parallel), and [`build`] is a pure
//! function of the workspace, selection, state, and those lookups.

use crate::config::{ToolDef, Workspace};
use crate::install::{self, InstallContext, Resolved};
use crate::remote::Remote;
use crate::selection::Selection;
use crate::state::{Method, ToolRecord, ToolState};
use anyhow::{Result, anyhow};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// What an apply run reinstalls on top of what changed.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Mode {
    /// Install missing tools and those whose version or recipe changed.
    #[default]
    Changed,
    /// Also reinstall these tools (every selected tool when `None`).
    Reinstall(Option<BTreeSet<String>>),
}

/// Planning options.
#[derive(Clone, Debug, Default)]
pub struct Options {
    pub mode: Mode,
    /// Restrict the run to these tools: nothing else is installed or removed.
    pub only: Option<BTreeSet<String>>,
}

/// What will happen to a tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Install,
    Upgrade,
    Reinstall,
    Uninstall,
    Keep,
    /// Not selected and not installed: listed for information only.
    Available,
    /// The tool couldn't be resolved; see `error`.
    Error,
}

impl Action {
    pub fn changes_something(self) -> bool {
        matches!(
            self,
            Action::Install | Action::Upgrade | Action::Reinstall | Action::Uninstall
        )
    }
}

/// One line of `dfutil status` / `plan`, and one row of the TUI tools table.
#[derive(Clone, Debug, Serialize)]
pub struct ToolRow {
    pub name: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub selected: bool,
    /// Why it's selected (preset names, `host`, `required by x`).
    pub reasons: Vec<String>,
    pub pin: String,
    pub installed: Option<String>,
    pub installed_method: Option<Method>,
    pub available: Option<String>,
    pub method: Option<Method>,
    pub action: Action,
    /// Why the action was chosen.
    pub why: String,
    pub notes: Vec<String>,
    pub error: Option<String>,
    pub verified: bool,
    pub has_previous: bool,
    /// Kept back on purpose: the available version is one the user rolled
    /// back from.
    pub held: bool,
    #[serde(skip)]
    pub resolved: Option<Resolved>,
}

/// A computed plan: one row per known or installed tool, in name order.
#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub rows: Vec<ToolRow>,
}

impl Plan {
    pub fn changes(&self) -> impl Iterator<Item = &ToolRow> {
        self.rows
            .iter()
            .filter(|row| row.action.changes_something())
    }

    pub fn has_changes(&self) -> bool {
        self.changes().next().is_some()
    }

    pub fn errors(&self) -> impl Iterator<Item = &ToolRow> {
        self.rows.iter().filter(|row| row.action == Action::Error)
    }
}

/// Resolves versions for `names` with up to `threads` lookups at once.
/// Failures are returned per tool rather than failing the whole lookup.
pub fn resolve_all(
    workspace: &Workspace,
    remote: &Remote,
    names: &BTreeSet<String>,
    threads: usize,
) -> BTreeMap<String, Result<Resolved, String>> {
    let build = workspace.build_mode();
    let queue = Mutex::new(names.iter().cloned().collect::<Vec<_>>());
    let results = Mutex::new(BTreeMap::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.max(1) {
            scope.spawn(|| {
                loop {
                    let Some(name) = queue.lock().unwrap().pop() else {
                        break;
                    };
                    let outcome = match workspace.config.tools.get(&name) {
                        Some(tool) => {
                            install::resolve(remote, &workspace.platform, &name, tool, build)
                                .map_err(|error| format!("{error:#}"))
                        }
                        None => Err("no longer defined in the config".to_string()),
                    };
                    results.lock().unwrap().insert(name, outcome);
                }
            });
        }
    });
    results.into_inner().unwrap()
}

/// Tools whose versions a plan needs: selected or installed ones, plus every
/// defined tool when `everything` is set (for listing available versions).
pub fn names_to_resolve(
    workspace: &Workspace,
    selection: &Selection,
    state: &ToolState,
    everything: bool,
) -> BTreeSet<String> {
    let mut names: BTreeSet<String> = selection.tools.iter().cloned().collect();
    names.extend(
        state
            .tools
            .keys()
            .filter(|name| workspace.config.tools.contains_key(*name))
            .cloned(),
    );
    if everything {
        names.extend(
            workspace
                .config
                .tools
                .iter()
                .filter(|(_, tool)| tool.when.matches(&workspace.platform))
                .map(|(name, _)| name.clone()),
        );
    }
    names
}

/// Builds the plan. Pure: no network, no file system.
pub fn build(
    workspace: &Workspace,
    selection: &Selection,
    state: &ToolState,
    resolved: &BTreeMap<String, Result<Resolved, String>>,
    options: &Options,
) -> Plan {
    let mut names: BTreeSet<&String> = workspace.config.tools.keys().collect();
    names.extend(state.tools.keys());
    let in_scope = |name: &str| options.only.as_ref().is_none_or(|only| only.contains(name));

    let rows = names
        .into_iter()
        .map(|name| {
            let tool = workspace.config.tools.get(name);
            let record = state.tools.get(name);
            let selected = selection.tools.contains(name);
            let lookup = resolved.get(name);
            let mut row = ToolRow {
                name: name.clone(),
                description: tool.and_then(|tool| tool.description.clone()),
                category: tool.and_then(|tool| tool.category.clone()),
                selected,
                reasons: selection.reasons.get(name).cloned().unwrap_or_default(),
                pin: tool
                    .and_then(|tool| tool.pin().ok())
                    .map(|pin| pin.to_string())
                    .unwrap_or_else(|| "latest".into()),
                installed: record.map(|record| record.current.version.clone()),
                installed_method: record.map(|record| record.method),
                available: None,
                method: None,
                action: Action::Keep,
                why: String::new(),
                notes: Vec::new(),
                error: None,
                verified: record.is_some_and(|record| record.verified),
                has_previous: record.is_some_and(|record| record.previous.is_some()),
                held: false,
                resolved: None,
            };
            match lookup {
                Some(Ok(resolved)) => {
                    row.available = resolved.version.clone();
                    row.method = Some(resolved.method);
                    row.notes = resolved.notes.clone();
                    row.resolved = Some(resolved.clone());
                }
                Some(Err(error)) => row.error = Some(error.clone()),
                None => {}
            }
            let (action, why) = decide(name, tool, record, selected, &row, options, in_scope(name));
            row.held = action == Action::Keep
                && record.is_some_and(|record| {
                    record.skip_version.is_some() && record.skip_version == row.available
                });
            row.action = action;
            row.why = why;
            row
        })
        .collect();
    Plan { rows }
}

fn decide(
    name: &str,
    tool: Option<&ToolDef>,
    record: Option<&ToolRecord>,
    selected: bool,
    row: &ToolRow,
    options: &Options,
    in_scope: bool,
) -> (Action, String) {
    let keep_or_available = |why: &str| {
        if record.is_some() {
            (Action::Keep, why.to_string())
        } else {
            (Action::Available, why.to_string())
        }
    };
    if !in_scope {
        return keep_or_available("outside --only");
    }
    let Some(record) = record else {
        if !selected {
            return (Action::Available, "not selected".into());
        }
        return match &row.resolved {
            Some(_) => (Action::Install, "not installed".into()),
            None => (
                Action::Error,
                row.error.clone().unwrap_or_else(|| "not resolved".into()),
            ),
        };
    };
    if !selected {
        // `--only` names tools to work on; it never removes what it didn't name
        // unless the name is deselected.
        return (Action::Uninstall, "no longer selected".into());
    }
    let Some(resolved) = &row.resolved else {
        return match &row.error {
            Some(error) => (Action::Error, error.clone()),
            None => (Action::Keep, "not checked".into()),
        };
    };
    let forced = match &options.mode {
        Mode::Changed => false,
        Mode::Reinstall(None) => true,
        Mode::Reinstall(Some(names)) => names.contains(name),
    };
    let fingerprint_changed = tool.is_some_and(|tool| tool.fingerprint() != record.fingerprint);
    let method_changed = resolved.method != record.method;
    let version_changed = resolved.version.as_ref().is_some_and(|version| {
        *version != record.current.version && !script_version_matches(record, version)
    });
    let held = !forced && record.skip_version.is_some() && record.skip_version == resolved.version;
    if held {
        (
            Action::Keep,
            format!(
                "rolled back from {}; reinstall to take it again",
                resolved.version.clone().unwrap_or_default()
            ),
        )
    } else if version_changed {
        (
            Action::Upgrade,
            format!(
                "{} -> {}",
                record.current.version,
                resolved.version.clone().unwrap_or_default()
            ),
        )
    } else if method_changed {
        (
            Action::Reinstall,
            format!("method {} -> {}", record.method, resolved.method),
        )
    } else if fingerprint_changed {
        (Action::Reinstall, "recipe changed".into())
    } else if forced {
        (Action::Reinstall, "reinstall requested".into())
    } else {
        (Action::Keep, "up to date".into())
    }
}

/// Script tools record what their version command printed (`1.2.3`), while
/// the release tag may be `v1.2.3`; treat those as equal.
fn script_version_matches(record: &ToolRecord, tag: &str) -> bool {
    record.method == Method::Script
        && crate::version::Version::parse(tag).is_some()
        && crate::version::Version::parse(tag)
            == crate::version::Version::parse(&record.current.version)
}

/// Outcome of applying one tool.
#[derive(Clone, Debug, Serialize)]
pub struct Outcome {
    pub name: String,
    pub action: Action,
    pub ok: bool,
    pub message: String,
}

/// Carries out a plan's changes, saving state after every tool so an
/// interrupted run keeps what it finished. Tools install in dependency
/// waves; prebuilt downloads in a wave run up to four at a time, everything
/// else one at a time.
pub fn apply(
    workspace: &Workspace,
    plan: &Plan,
    remote: &Remote,
    state: ToolState,
    log: &(dyn Fn(String) + Sync),
) -> Result<(ToolState, Vec<Outcome>)> {
    let paths = &workspace.paths;
    let state = Mutex::new(state);
    let outcomes = Mutex::new(Vec::new());
    let repo_root = workspace.main_root().path.clone();
    let context = InstallContext {
        paths,
        remote,
        platform: &workspace.platform,
        repo_root: &repo_root,
        log,
    };
    let save = |state: &ToolState| crate::state::save(&paths.tool_state, state);

    for row in plan
        .rows
        .iter()
        .filter(|row| row.action == Action::Uninstall)
    {
        let record = state.lock().unwrap().tools.get(&row.name).cloned();
        let Some(record) = record else { continue };
        let tool = workspace.config.tools.get(&row.name);
        log(format!("{}: uninstalling", row.name));
        let result = install::uninstall(paths, &row.name, tool, &record, &repo_root);
        install::init::remove(paths, &row.name);
        let mut guard = state.lock().unwrap();
        if result.is_ok() {
            guard.tools.remove(&row.name);
            save(&guard)?;
        }
        outcomes
            .lock()
            .unwrap()
            .push(outcome(row, result.map(|_| "removed".to_string())));
    }

    let to_install: Vec<&ToolRow> = plan
        .rows
        .iter()
        .filter(|row| {
            matches!(
                row.action,
                Action::Install | Action::Upgrade | Action::Reinstall
            )
        })
        .collect();
    let failed: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    for wave in waves(workspace, &to_install) {
        let (parallel, serial): (Vec<&ToolRow>, Vec<&ToolRow>) = wave
            .into_iter()
            .partition(|row| row.method == Some(Method::Prebuilt));
        let install_row = |row: &ToolRow| {
            let tool = &workspace.config.tools[&row.name];
            let blocked: Vec<String> = tool
                .requires
                .iter()
                .filter(|required| failed.lock().unwrap().contains(*required))
                .cloned()
                .collect();
            let result = if !blocked.is_empty() {
                Err(anyhow!(
                    "skipped: required tool {} failed",
                    blocked.join(", ")
                ))
            } else {
                let resolved = row
                    .resolved
                    .as_ref()
                    .expect("planned installs are resolved");
                let existing = state.lock().unwrap().tools.get(&row.name).cloned();
                install::install(&context, &row.name, tool, resolved, existing.as_ref()).and_then(
                    |mut record| {
                        let message = format!("{} ({})", record.current.version, record.method);
                        // Record what's on disk first, so a failing post step
                        // can't leave the state naming a different version.
                        {
                            let mut guard = state.lock().unwrap();
                            guard.tools.insert(row.name.clone(), record.clone());
                            save(&guard)?;
                        }
                        let finish = install::post_install(&context, &row.name, tool, resolved)
                            .and_then(|()| {
                                install::init::remove(paths, &row.name);
                                install::init::generate(
                                    paths,
                                    &row.name,
                                    tool,
                                    &shells_for(workspace),
                                )
                                .map(|_| ())
                            });
                        if let Err(error) = finish {
                            // An empty fingerprint reads as "recipe changed",
                            // so the next apply finishes the job.
                            record.fingerprint = String::new();
                            let mut guard = state.lock().unwrap();
                            guard.tools.insert(row.name.clone(), record);
                            save(&guard)?;
                            return Err(error);
                        }
                        Ok(message)
                    },
                )
            };
            match &result {
                Ok(message) => log(format!("{}: installed {message}", row.name)),
                Err(error) => {
                    log(format!("{}: failed: {error:#}", row.name));
                    failed.lock().unwrap().insert(row.name.clone());
                }
            }
            outcomes.lock().unwrap().push(outcome(row, result));
        };
        for chunk in parallel.chunks(4) {
            std::thread::scope(|scope| {
                for row in chunk {
                    scope.spawn(|| install_row(row));
                }
            });
        }
        for row in serial {
            install_row(row);
        }
    }

    let state = state.into_inner().unwrap();
    let installed: BTreeMap<String, &ToolDef> = state
        .tools
        .keys()
        .filter_map(|name| {
            workspace
                .config
                .tools
                .get(name)
                .map(|tool| (name.clone(), tool))
        })
        .collect();
    install::init::write_aggregates(paths, &installed)?;
    let mut outcomes = outcomes.into_inner().unwrap();
    outcomes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok((state, outcomes))
}

fn shells_for(workspace: &Workspace) -> BTreeSet<String> {
    Selection::compute(workspace)
        .map(|selection| selection.shells)
        .unwrap_or_else(|_| ["zsh".to_string()].into())
}

fn outcome(row: &ToolRow, result: Result<String>) -> Outcome {
    match result {
        Ok(message) => Outcome {
            name: row.name.clone(),
            action: row.action,
            ok: true,
            message,
        },
        Err(error) => Outcome {
            name: row.name.clone(),
            action: row.action,
            ok: false,
            message: format!("{error:#}"),
        },
    }
}

/// Splits rows into waves where each tool's `requires` (among the rows)
/// are in earlier waves. Cycles are broken by putting the rest in one wave.
fn waves<'a>(workspace: &Workspace, rows: &[&'a ToolRow]) -> Vec<Vec<&'a ToolRow>> {
    let mut remaining: Vec<&ToolRow> = rows.to_vec();
    let names: BTreeSet<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut result = Vec::new();
    while !remaining.is_empty() {
        let (ready, blocked): (Vec<&ToolRow>, Vec<&ToolRow>) =
            remaining.into_iter().partition(|row| {
                workspace.config.tools.get(&row.name).is_none_or(|tool| {
                    tool.requires.iter().all(|required| {
                        !names.contains(required.as_str()) || done.contains(required)
                    })
                })
            });
        if ready.is_empty() {
            result.push(blocked);
            break;
        }
        done.extend(ready.iter().map(|row| row.name.clone()));
        result.push(ready);
        remaining = blocked;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostConfig;
    use crate::config::tests::{SAMPLE, workspace_with};
    use crate::state::InstalledVersion;

    fn resolved(version: &str, method: Method) -> Result<Resolved, String> {
        Ok(Resolved {
            method,
            version: Some(version.into()),
            source: None,
            asset: None,
            checksum: None,
            notes: vec![],
        })
    }

    fn record(workspace: &Workspace, name: &str, version: &str) -> ToolRecord {
        ToolRecord {
            current: InstalledVersion {
                version: version.into(),
                install_dir: None,
                links: vec![],
            },
            previous: None,
            method: Method::Prebuilt,
            source: None,
            fingerprint: workspace.config.tools[name].fingerprint(),
            sha256: None,
            verified: false,
            installed_at: 0,
            skip_version: None,
        }
    }

    fn setup(host: HostConfig) -> (tempfile::TempDir, Workspace, Selection) {
        let home = tempfile::tempdir().unwrap();
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        let selection = Selection::compute(&workspace).unwrap();
        (home, workspace, selection)
    }

    fn action(plan: &Plan, name: &str) -> Action {
        plan.rows
            .iter()
            .find(|row| row.name == name)
            .unwrap()
            .action
    }

    #[test]
    fn installs_missing_upgrades_changed_and_keeps_current() {
        let (_home, workspace, selection) = setup(HostConfig {
            presets: vec!["claude".into()],
            ..HostConfig::default()
        });
        let mut state = ToolState::default();
        state
            .tools
            .insert("ripgrep".into(), record(&workspace, "ripgrep", "14.1.0"));
        state
            .tools
            .insert("node".into(), record(&workspace, "node", "v22.0.0"));
        let lookups: BTreeMap<_, _> = [
            ("ripgrep".to_string(), resolved("14.1.0", Method::Prebuilt)),
            ("node".to_string(), resolved("v22.1.0", Method::Prebuilt)),
            ("ccstatusline".to_string(), resolved("2.0.0", Method::Npm)),
        ]
        .into();
        let plan = build(
            &workspace,
            &selection,
            &state,
            &lookups,
            &Options::default(),
        );
        assert_eq!(action(&plan, "ripgrep"), Action::Keep);
        assert_eq!(action(&plan, "node"), Action::Upgrade);
        assert_eq!(action(&plan, "ccstatusline"), Action::Install);

        let reinstall = Options {
            mode: Mode::Reinstall(Some(["ripgrep".to_string()].into())),
            only: None,
        };
        let plan = build(&workspace, &selection, &state, &lookups, &reinstall);
        assert_eq!(action(&plan, "ripgrep"), Action::Reinstall);
    }

    #[test]
    fn a_rolled_back_version_is_skipped_until_a_newer_one_or_a_reinstall() {
        let (_home, workspace, selection) = setup(HostConfig {
            presets: vec!["claude".into()],
            ..HostConfig::default()
        });
        let mut held = record(&workspace, "ripgrep", "14.1.0");
        held.skip_version = Some("14.1.1".into());
        let mut state = ToolState::default();
        state.tools.insert("ripgrep".into(), held);
        let plan_with = |version: &str, options: &Options| {
            let lookups: BTreeMap<_, _> =
                [("ripgrep".to_string(), resolved(version, Method::Prebuilt))].into();
            build(&workspace, &selection, &state, &lookups, options)
        };
        let held_plan = plan_with("14.1.1", &Options::default());
        assert_eq!(action(&held_plan, "ripgrep"), Action::Keep);
        let row = |plan: &Plan| plan.rows.iter().find(|row| row.name == "ripgrep").cloned();
        assert!(row(&held_plan).unwrap().held);
        let newer = plan_with("14.2.0", &Options::default());
        assert_eq!(action(&newer, "ripgrep"), Action::Upgrade);
        assert!(!row(&newer).unwrap().held);
        let reinstall = Options {
            mode: Mode::Reinstall(Some(["ripgrep".to_string()].into())),
            only: None,
        };
        assert_eq!(
            action(&plan_with("14.1.1", &reinstall), "ripgrep"),
            Action::Upgrade
        );
    }

    #[test]
    fn recipe_and_method_changes_trigger_reinstalls() {
        let (_home, workspace, selection) = setup(HostConfig {
            presets: vec!["base".into()],
            ..HostConfig::default()
        });
        let mut state = ToolState::default();
        let mut stale = record(&workspace, "ripgrep", "14.1.0");
        stale.fingerprint = "old".into();
        state.tools.insert("ripgrep".into(), stale);
        let lookups: BTreeMap<_, _> =
            [("ripgrep".to_string(), resolved("14.1.0", Method::Prebuilt))].into();
        let plan = build(
            &workspace,
            &selection,
            &state,
            &lookups,
            &Options::default(),
        );
        assert_eq!(action(&plan, "ripgrep"), Action::Reinstall);

        state.tools.get_mut("ripgrep").unwrap().fingerprint =
            workspace.config.tools["ripgrep"].fingerprint();
        let lookups: BTreeMap<_, _> =
            [("ripgrep".to_string(), resolved("14.1.0", Method::Cargo))].into();
        let plan = build(
            &workspace,
            &selection,
            &state,
            &lookups,
            &Options::default(),
        );
        assert_eq!(action(&plan, "ripgrep"), Action::Reinstall);
    }

    #[test]
    fn deselected_tools_are_removed_except_outside_only() {
        let (_home, workspace, selection) = setup(HostConfig::default());
        let mut state = ToolState::default();
        state
            .tools
            .insert("ripgrep".into(), record(&workspace, "ripgrep", "14.1.0"));
        let lookups = BTreeMap::new();
        let plan = build(
            &workspace,
            &selection,
            &state,
            &lookups,
            &Options::default(),
        );
        assert_eq!(action(&plan, "ripgrep"), Action::Uninstall);
        let only = Options {
            mode: Mode::Changed,
            only: Some(["node".to_string()].into()),
        };
        let plan = build(&workspace, &selection, &state, &lookups, &only);
        assert_eq!(action(&plan, "ripgrep"), Action::Keep);
        assert_eq!(action(&plan, "node"), Action::Available);
    }

    #[test]
    fn lookup_failures_become_error_rows() {
        let (_home, workspace, selection) = setup(HostConfig {
            presets: vec!["base".into()],
            ..HostConfig::default()
        });
        let lookups: BTreeMap<_, _> =
            [("ripgrep".to_string(), Err("rate limited".to_string()))].into();
        let plan = build(
            &workspace,
            &selection,
            &ToolState::default(),
            &lookups,
            &Options::default(),
        );
        assert_eq!(action(&plan, "ripgrep"), Action::Error);
        assert_eq!(plan.errors().count(), 1);
    }

    #[test]
    fn waves_respect_requires() {
        let (_home, workspace, selection) = setup(HostConfig {
            presets: vec!["claude".into()],
            ..HostConfig::default()
        });
        let lookups: BTreeMap<_, _> = [
            ("ripgrep".to_string(), resolved("1", Method::Prebuilt)),
            ("node".to_string(), resolved("1", Method::Prebuilt)),
            ("ccstatusline".to_string(), resolved("1", Method::Npm)),
        ]
        .into();
        let plan = build(
            &workspace,
            &selection,
            &ToolState::default(),
            &lookups,
            &Options::default(),
        );
        let rows: Vec<&ToolRow> = plan.changes().collect();
        let waves = waves(&workspace, &rows);
        let first: Vec<&str> = waves[0].iter().map(|row| row.name.as_str()).collect();
        assert!(first.contains(&"node") && !first.contains(&"ccstatusline"));
        assert_eq!(waves[1][0].name, "ccstatusline");
    }
}

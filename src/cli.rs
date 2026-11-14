//! Command-line interface. Every command has a `--json` form for ansible;
//! JSON output always carries a top-level `changed` flag.

use crate::config::{BuildMode, PresetDef, ToolDef, ToolKind};
use crate::deploy;
use crate::ops::{self, ApplyReport, Session};
use crate::paths::{Paths, Scope};
use crate::plan::{Action, Mode, Options, Plan};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeSet;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "dfutil",
    version,
    about = "Install command-line tools and deploy dotfiles from a content repo"
)]
pub struct Cli {
    /// Host config file.
    #[arg(long, global = true, env = "DFUTIL_HOST_CONFIG")]
    pub host_config: Option<PathBuf>,
    /// Use a different home directory (for testing a config in a scratch home).
    #[arg(long, global = true, env = "DFUTIL_HOME", hide = true)]
    pub home: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Don't ask for confirmation. Required when stdin isn't a terminal.
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,
    /// Use cached version lookups only; never touch the network.
    #[arg(long, global = true)]
    pub offline: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Set up this host: write host.toml pointing at a content repo.
    Init {
        /// Content repo: a local path, or a git URL to clone.
        #[arg(long)]
        repo: String,
        /// Where to clone a URL (default ~/dotfiles).
        #[arg(long)]
        clone_to: Option<PathBuf>,
        /// Presets to select (default: the repo's `settings.presets`).
        #[arg(long = "preset", value_delimiter = ',')]
        presets: Vec<String>,
        /// Install tools for this user or system-wide.
        #[arg(long, value_parser = parse_scope)]
        scope: Option<Scope>,
    },
    /// List tools with installed and available versions.
    Status {
        /// Also look up versions of tools that aren't selected.
        #[arg(long)]
        all: bool,
    },
    /// Show what `apply` would change.
    Plan(ApplyArgs),
    /// Install and update the selected tools and deploy dotfiles.
    Apply(ApplyArgs),
    /// Install tools on this host (adds them to host.toml).
    Install {
        #[arg(required = true)]
        tools: Vec<String>,
    },
    /// Reinstall tools (all selected tools when none are named).
    Reinstall { tools: Vec<String> },
    /// Remove tools from this host (updates host.toml).
    Uninstall {
        #[arg(required = true)]
        tools: Vec<String>,
    },
    /// Switch a tool back to the version it had before its last install.
    Rollback { tool: String },
    /// Sync repos, then install whatever changed (what the schedule runs).
    Update,
    /// Manage tool definitions.
    #[command(subcommand)]
    Tool(ToolCommand),
    /// Manage presets.
    #[command(subcommand)]
    Preset(PresetCommand),
    /// List or switch color palettes.
    #[command(subcommand)]
    Palette(PaletteCommand),
    /// List or set template variables.
    #[command(subcommand)]
    Var(VarCommand),
    /// Link dotfiles into $HOME, rendering templates.
    Deploy {
        #[arg(long)]
        dry_run: bool,
        /// Overwrite rendered files edited in place, discarding the edits.
        #[arg(long)]
        force: bool,
    },
    /// Copy edits made to rendered files back into their templates.
    Capture {
        #[arg(long)]
        dry_run: bool,
    },
    /// Show incoming or outgoing changes in the content repos.
    #[command(subcommand)]
    Diff(DiffCommand),
    /// Capture edits, pull every repo (stashing local changes), and redeploy.
    Sync,
    /// Git hooks.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Scheduled automatic updates.
    #[command(subcommand)]
    Schedule(ScheduleCommand),
    /// Check the configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
    /// Open the terminal UI (the default on a terminal with no command).
    Tui,
}

#[derive(Args, Debug, Clone, Default)]
pub struct ApplyArgs {
    /// Reinstall these tools even if unchanged (all selected when given with no names).
    #[arg(long, num_args = 0.., value_delimiter = ',')]
    pub reinstall: Option<Vec<String>>,
    /// Only touch these tools; skips dotfile deployment.
    #[arg(long, value_delimiter = ',')]
    pub only: Option<Vec<String>>,
}

impl ApplyArgs {
    fn options(&self) -> Options {
        Options {
            mode: match &self.reinstall {
                None => Mode::Changed,
                Some(names) if names.is_empty() => Mode::Reinstall(None),
                Some(names) => Mode::Reinstall(Some(names.iter().cloned().collect())),
            },
            only: self
                .only
                .as_ref()
                .map(|names| names.iter().cloned().collect()),
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum ToolCommand {
    /// Add a tool definition to dfutil.toml.
    Add(ToolAddArgs),
}

#[derive(Args, Debug)]
pub struct ToolAddArgs {
    pub name: String,
    /// github (default with --repo), cargo, npm, uv, or script.
    #[arg(long, value_parser = parse_kind)]
    pub kind: Option<ToolKind>,
    /// GitHub owner/name.
    #[arg(long)]
    pub repo: Option<String>,
    /// Version pin: empty, 14, 14.1, 14.1.0, v14.1.0, or a prefix ending in `*`.
    #[arg(long)]
    pub pin: Option<String>,
    /// crates.io crate, for source builds.
    #[arg(long = "crate")]
    pub crate_name: Option<String>,
    /// npm or PyPI package name.
    #[arg(long)]
    pub package: Option<String>,
    /// Asset glob per platform, as KEY=GLOB (KEY like linux-x86_64, linux, any).
    #[arg(long = "asset")]
    pub assets: Vec<String>,
    /// Binary to link: NAME or NAME=path/in/archive.
    #[arg(long = "bin")]
    pub bins: Vec<String>,
    #[arg(long)]
    pub description: Option<String>,
    /// Also add the tool to this host.
    #[arg(long)]
    pub install: bool,
    /// File to add it to (default: the main repo's dfutil.toml).
    #[arg(long)]
    pub file: Option<PathBuf>,
}

#[derive(Subcommand, Debug)]
pub enum PresetCommand {
    /// List presets and what they select.
    List,
    /// Create a preset in dfutil.toml.
    Create {
        name: String,
        #[arg(long, value_delimiter = ',')]
        tools: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        packages: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        extends: Vec<String>,
        #[arg(long)]
        description: Option<String>,
        /// Also select it on this host.
        #[arg(long)]
        select: bool,
    },
    /// Select a preset on this host.
    Select { name: String },
    /// Deselect a preset on this host.
    Deselect { name: String },
}

#[derive(Subcommand, Debug)]
pub enum PaletteCommand {
    List,
    /// Switch palettes and re-render templates.
    Set {
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum VarCommand {
    /// List variables (secret values are hidden).
    List,
    /// Set a variable's value for this host.
    Set { name: String, value: String },
}

#[derive(Subcommand, Debug)]
pub enum DiffCommand {
    /// Commits and changes waiting upstream.
    Incoming {
        /// Compare against what was last fetched.
        #[arg(long)]
        no_fetch: bool,
    },
    /// Local changes not yet pushed, including edits to rendered templates.
    Outgoing,
}

#[derive(Subcommand, Debug)]
pub enum HookCommand {
    /// Install the secret-blocking pre-commit hook in each content repo.
    Install,
    /// Run the pre-commit check (called by the hook).
    PreCommit {
        #[arg(long)]
        repo: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum ScheduleCommand {
    Enable {
        #[arg(long, visible_alias = "interval", value_enum, default_value = "daily")]
        frequency: crate::schedule::Frequency,
    },
    Disable,
    Status,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Load and check every config file.
    Validate,
    /// Print the merged config as TOML.
    Show,
}

fn parse_scope(text: &str) -> Result<Scope, String> {
    match text {
        "user" => Ok(Scope::User),
        "system" => Ok(Scope::System),
        _ => Err("expected `user` or `system`".into()),
    }
}

fn parse_kind(text: &str) -> Result<ToolKind, String> {
    toml::Value::String(text.to_string())
        .try_into()
        .map_err(|_| "expected github, cargo, npm, uv, or script".to_string())
}

/// Runs the CLI; returns the process exit code.
pub fn run(cli: Cli) -> Result<i32> {
    let host_path = cli.host_config.clone().unwrap_or_else(|| match &cli.home {
        Some(home) => Paths::for_home(home, Scope::User).host_config(),
        None => Paths::from_env(Scope::User).host_config(),
    });
    let output = Output { json: cli.json };
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let log_json = cli.json;
    let log = move |line: String| {
        if log_json {
            eprintln!("{line}");
        } else {
            println!("{line}");
        }
    };

    let command = match cli.command {
        Some(command) => command,
        None if interactive && !cli.json => Command::Tui,
        None => bail!("no command given; see `dfutil --help`"),
    };

    if let Command::Init {
        repo,
        clone_to,
        presets,
        scope,
    } = &command
    {
        let home = cli
            .home
            .clone()
            .unwrap_or_else(|| Paths::from_env(Scope::User).home);
        let checkout = ops::init(
            &host_path,
            &home,
            repo,
            clone_to.as_deref(),
            presets.clone(),
            *scope,
            &log,
        )?;
        if interactive && !cli.yes && !cli.json {
            init_prompts(&host_path, &checkout, presets.is_empty())?;
        }
        let repo_config =
            crate::config::RepoConfig::load(&checkout.join(crate::config::REPO_CONFIG_FILE))?;
        let host_vars = crate::config::HostConfig::load(&host_path)?.vars;
        let unset: Vec<&String> = repo_config
            .vars
            .iter()
            .filter(|(name, definition)| {
                definition.default_value().is_none() && !host_vars.contains_key(*name)
            })
            .map(|(name, _)| name)
            .collect();
        if !unset.is_empty() && !cli.json {
            println!(
                "set these template variables before deploying: {} (`dfutil var set <name> <value>`)",
                unset
                    .iter()
                    .map(|name| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        output.done(
            true,
            &format!(
                "wrote {} (repo {}); run `dfutil apply` next",
                host_path.display(),
                checkout.display()
            ),
            json!({ "host_config": host_path, "repo": checkout }),
        );
        return Ok(0);
    }
    if let Command::Hook(HookCommand::PreCommit { repo }) = &command {
        return hook_pre_commit(&host_path, cli.home.as_deref(), repo);
    }

    let mut session = Session::load(&host_path, cli.home.as_deref(), cli.offline)?;
    let confirm = |plan_text: &str| -> Result<bool> {
        if cli.yes {
            return Ok(true);
        }
        if !interactive {
            bail!("refusing to make changes without confirmation; pass --yes");
        }
        print!("{plan_text}Proceed? [y/N] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
    };

    match command {
        Command::Init { .. } | Command::Hook(HookCommand::PreCommit { .. }) => unreachable!(),
        Command::Tui => {
            crate::tui::run(session)?;
            Ok(0)
        }
        Command::Status { all } => {
            let plan = session.plan(&Options::default(), all)?;
            output.plan(&plan, all, &session.workspace.notes);
            Ok(0)
        }
        Command::Plan(args) => {
            let plan = session.plan(&args.options(), false)?;
            let deploy_items = if args.only.is_none() {
                session.deploy(&deploy::Options {
                    dry_run: true,
                    force: false,
                })?
            } else {
                Vec::new()
            };
            if output.json {
                let changed = plan.has_changes()
                    || deploy_items
                        .iter()
                        .any(|item| item.change != deploy::Change::Unchanged);
                output.print_json(
                    json!({ "changed": changed, "tools": plan.rows, "dotfiles": deploy_items }),
                );
            } else {
                print!("{}", plan_text(&plan));
                print!("{}", deploy_text(&deploy_items, true));
            }
            Ok(if plan.errors().next().is_some() { 1 } else { 0 })
        }
        Command::Apply(args) => {
            let options = args.options();
            if !cli.yes {
                let plan = session.plan(&options, false)?;
                if !plan.has_changes() && !output.json && args.only.is_some() {
                    println!("nothing to do");
                    return Ok(0);
                }
                if plan.has_changes() && !confirm(&plan_text(&plan))? {
                    return Ok(1);
                }
                let report = session.apply_plan(&plan, args.only.is_none(), &log)?;
                return Ok(output.report(&report));
            }
            let report = session.apply(&options, &log)?;
            Ok(output.report(&report))
        }
        Command::Install { tools } => {
            let report = session.install(&tools, &log)?;
            Ok(output.report(&report))
        }
        Command::Reinstall { tools } => {
            let options = Options {
                mode: Mode::Reinstall(if tools.is_empty() {
                    None
                } else {
                    Some(tools.iter().cloned().collect())
                }),
                only: if tools.is_empty() {
                    None
                } else {
                    Some(tools.iter().cloned().collect())
                },
            };
            let plan = session.plan(&options, false)?;
            if !confirm(&plan_text(&plan))? {
                return Ok(1);
            }
            let report = session.apply_plan(&plan, false, &log)?;
            Ok(output.report(&report))
        }
        Command::Uninstall { tools } => {
            if !confirm(&format!("Remove {} from this host.\n", tools.join(", ")))? {
                return Ok(1);
            }
            let report = session.uninstall(&tools, &log)?;
            Ok(output.report(&report))
        }
        Command::Rollback { tool } => {
            let version = session.rollback(&tool)?;
            output.done(
                true,
                &format!("{tool} is back on {version}"),
                json!({ "tool": tool, "version": version }),
            );
            Ok(0)
        }
        Command::Update => {
            let report = session.update(&log)?;
            Ok(output.report(&report))
        }
        Command::Tool(ToolCommand::Add(args)) => tool_add(&mut session, args, &output, &log),
        Command::Preset(command) => preset(&mut session, command, &output),
        Command::Palette(PaletteCommand::List) => {
            let active = session.workspace.palette_name().map(str::to_string);
            let names: Vec<&String> = session.workspace.palettes.keys().collect();
            if output.json {
                output.print_json(json!({ "changed": false, "active": active, "palettes": names }));
            } else {
                for name in names {
                    let marker = if Some(name) == active.as_ref() {
                        "*"
                    } else {
                        " "
                    };
                    println!("{marker} {name}");
                }
            }
            Ok(0)
        }
        Command::Palette(PaletteCommand::Set { name }) => {
            let items = session.set_palette(&name)?;
            output.deploy(&items, false);
            Ok(0)
        }
        Command::Var(VarCommand::List) => {
            let vars = crate::deploy::template::workspace_vars(&session.workspace)?;
            let rows: Vec<serde_json::Value> = session
                .workspace
                .config
                .vars
                .iter()
                .map(|(name, definition)| {
                    let value = vars.get(name).map(|value| if definition.is_secret() { "(secret)".to_string() } else { value.clone() });
                    json!({ "name": name, "value": value, "secret": definition.is_secret(), "description": definition.description() })
                })
                .collect();
            if output.json {
                output.print_json(json!({ "changed": false, "vars": rows }));
            } else {
                for row in &rows {
                    let value = row["value"].as_str().unwrap_or("(not set)");
                    println!("{:<24} {value}", row["name"].as_str().unwrap_or_default());
                }
            }
            Ok(0)
        }
        Command::Var(VarCommand::Set { name, value }) => {
            if !session.workspace.config.vars.contains_key(&name) {
                bail!("`{name}` isn't declared in [vars] of dfutil.toml");
            }
            crate::config_edit::host_var(&session.host_path, &name, &value)?;
            output.done(
                true,
                &format!("set {name}; run `dfutil deploy` to re-render templates"),
                json!({ "var": name }),
            );
            Ok(0)
        }
        Command::Deploy { dry_run, force } => {
            let items = session.deploy(&deploy::Options { dry_run, force })?;
            output.deploy(&items, dry_run);
            Ok(
                if items
                    .iter()
                    .any(|item| item.change == deploy::Change::Error)
                {
                    1
                } else {
                    0
                },
            )
        }
        Command::Capture { dry_run } => {
            // Diffs are worked out before writing, against the old template.
            let preview = session.capture(false)?;
            let edited = if dry_run {
                preview.clone()
            } else {
                session.capture(true)?
            };
            if output.json {
                let changed = !dry_run && edited.iter().any(|edit| edit.problem.is_none());
                output.print_json(json!({ "changed": changed, "captured": edited }));
            } else if edited.is_empty() {
                println!("no rendered files were edited");
            } else {
                for edit in &edited {
                    let shown = preview
                        .iter()
                        .find(|before| before.target == edit.target && edit.problem.is_none())
                        .unwrap_or(edit);
                    print!("{}", capture_text(shown, dry_run));
                }
            }
            Ok(if edited.iter().any(|edit| edit.problem.is_some()) {
                1
            } else {
                0
            })
        }
        Command::Diff(DiffCommand::Incoming { no_fetch }) => {
            let incoming = crate::gitops::incoming(&session.workspace, !no_fetch);
            if output.json {
                output.print_json(json!({ "changed": false, "repos": incoming }));
            } else {
                for repo in &incoming {
                    println!("== {} ({})", repo.root, repo.path.display());
                    if let Some(problem) = &repo.problem {
                        println!("  {problem}");
                    } else if repo.commits.is_empty() {
                        println!(
                            "  up to date with {}",
                            repo.upstream.clone().unwrap_or_default()
                        );
                    } else {
                        for commit in &repo.commits {
                            println!("  {commit}");
                        }
                        println!("{}", repo.diff);
                    }
                }
            }
            Ok(0)
        }
        Command::Diff(DiffCommand::Outgoing) => {
            let edited = session.capture(false)?;
            let outgoing = crate::gitops::outgoing(&session.workspace);
            if output.json {
                output.print_json(
                    json!({ "changed": false, "repos": outgoing, "uncaptured": edited }),
                );
            } else {
                for edit in &edited {
                    print!("{}", capture_text(edit, true));
                }
                for repo in &outgoing {
                    println!("== {} ({})", repo.root, repo.path.display());
                    if let Some(problem) = &repo.problem {
                        println!("  {problem}");
                    }
                    for line in &repo.status {
                        println!("  {line}");
                    }
                    for commit in &repo.commits {
                        println!("  unpushed: {commit}");
                    }
                    if !repo.diff.is_empty() {
                        println!("{}", repo.diff);
                    }
                }
            }
            Ok(0)
        }
        Command::Sync => {
            let report = session.sync(&log)?;
            Ok(output.report(&report))
        }
        Command::Hook(HookCommand::Install) => {
            let exe = std::env::current_exe().context("finding the dfutil executable")?;
            let hooks = crate::gitops::install_hooks(&session.workspace, &exe)?;
            output.done(
                true,
                &format!("installed {} hook(s)", hooks.len()),
                json!({ "hooks": hooks }),
            );
            Ok(0)
        }
        Command::Schedule(command) => {
            let home = session.workspace.paths.home.clone();
            match command {
                ScheduleCommand::Enable { frequency } => {
                    let exe = std::env::current_exe().context("finding the dfutil executable")?;
                    let host = std::fs::canonicalize(&session.host_path)
                        .unwrap_or(session.host_path.clone());
                    match crate::schedule::enable(&home, &exe, &host, frequency)? {
                        crate::schedule::Enabled::Systemd { timer } => output.done(
                            true,
                            &format!("enabled {}", timer.display()),
                            json!({ "timer": timer }),
                        ),
                        crate::schedule::Enabled::Cron { line } => output.done(
                            false,
                            &format!(
                                "no systemd user session; add this line with `crontab -e`:\n{line}"
                            ),
                            json!({ "cron": line }),
                        ),
                    }
                }
                ScheduleCommand::Disable => {
                    crate::schedule::disable(&home)?;
                    output.done(true, "disabled the scheduled update", json!({}));
                }
                ScheduleCommand::Status => {
                    let status = crate::schedule::status(&home);
                    if output.json {
                        output.print_json(json!({ "changed": false, "schedule": status }));
                    } else if !status.installed {
                        println!("no scheduled update");
                    } else {
                        println!("schedule: {}", status.schedule.unwrap_or_default());
                        println!(
                            "next run: {}",
                            status.next_run.unwrap_or_else(|| "unknown".into())
                        );
                        println!(
                            "last result: {}",
                            status.last_result.unwrap_or_else(|| "unknown".into())
                        );
                    }
                }
            }
            Ok(0)
        }
        Command::Config(ConfigCommand::Validate) => {
            // Loading already validated; report the notes too.
            output.done(
                false,
                &validate_text(&session),
                json!({ "notes": session.workspace.notes }),
            );
            Ok(0)
        }
        Command::Config(ConfigCommand::Show) => {
            let text = toml::to_string_pretty(&serde_json::to_value(ShowConfig::from(&session))?)?;
            print!("{text}");
            Ok(0)
        }
    }
}

#[derive(Serialize)]
struct ShowConfig {
    host: crate::config::HostConfig,
    selected_tools: BTreeSet<String>,
    selected_packages: BTreeSet<String>,
    shells: BTreeSet<String>,
    build: BuildMode,
    scope: Scope,
}

impl From<&Session> for ShowConfig {
    fn from(session: &Session) -> Self {
        Self {
            host: session.workspace.host.clone(),
            selected_tools: session.selection.tools.clone(),
            selected_packages: session.selection.packages.clone(),
            shells: session.selection.shells.clone(),
            build: session.workspace.build_mode(),
            scope: session.workspace.paths.scope,
        }
    }
}

fn prompt(question: &str, hidden: bool) -> Result<String> {
    print!("{question}");
    std::io::stdout().flush()?;
    let toggle = |flag: &str| {
        std::process::Command::new("stty")
            .arg(flag)
            .stdin(std::process::Stdio::inherit())
            .status()
            .ok();
    };
    if hidden {
        toggle("-echo");
    }
    let mut answer = String::new();
    let result = std::io::stdin().read_line(&mut answer);
    if hidden {
        toggle("echo");
        println!();
    }
    result?;
    Ok(answer.trim().to_string())
}

/// On a terminal, `init` asks for presets (when none were given on the
/// command line) and for variables that have no default.
fn init_prompts(
    host_path: &std::path::Path,
    checkout: &std::path::Path,
    ask_presets: bool,
) -> Result<()> {
    let config = crate::config::RepoConfig::load(&checkout.join(crate::config::REPO_CONFIG_FILE))?;
    if ask_presets && !config.presets.is_empty() {
        println!("Presets:");
        for (name, preset) in &config.presets {
            println!(
                "  {name:<16} {}",
                preset.description.clone().unwrap_or_default()
            );
        }
        let current = crate::config::HostConfig::load(host_path)?.presets;
        let answer = prompt(
            &format!(
                "Presets to select, comma-separated [{}]: ",
                current.join(",")
            ),
            false,
        )?;
        if !answer.is_empty() {
            let chosen: Vec<String> = answer
                .split(',')
                .map(|name| name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect();
            if let Some(unknown) = chosen
                .iter()
                .find(|name| !config.presets.contains_key(*name))
            {
                bail!("no preset named `{unknown}`; rerun `dfutil preset select` to fix");
            }
            for name in &current {
                crate::config_edit::host_list(host_path, "presets", name, false)?;
            }
            for name in &chosen {
                crate::config_edit::host_list(host_path, "presets", name, true)?;
            }
        }
    }
    for (name, definition) in &config.vars {
        if definition.default_value().is_some() {
            continue;
        }
        let description = definition
            .description()
            .map(|text| format!(" ({text})"))
            .unwrap_or_default();
        let value = prompt(
            &format!("Value for `{name}`{description}, empty to skip: "),
            definition.is_secret(),
        )?;
        if !value.is_empty() {
            crate::config_edit::host_var(host_path, name, &value)?;
        }
    }
    Ok(())
}

fn validate_text(session: &Session) -> String {
    let mut text = format!(
        "configuration is valid: {} tools, {} presets, {} packages, {} palettes",
        session.workspace.config.tools.len(),
        session.workspace.config.presets.len(),
        session.workspace.config.packages.len(),
        session.workspace.palettes.len()
    );
    for note in &session.workspace.notes {
        text.push_str(&format!("\nnote: {note}"));
    }
    text
}

fn tool_add(
    session: &mut Session,
    args: ToolAddArgs,
    output: &Output,
    log: ops::Log,
) -> Result<i32> {
    let mut assets = std::collections::BTreeMap::new();
    for entry in &args.assets {
        let (key, glob) = entry
            .split_once('=')
            .with_context(|| format!("--asset `{entry}`: expected KEY=GLOB"))?;
        assets.insert(key.to_string(), glob.to_string());
    }
    let tool = ToolDef {
        kind: args.kind,
        description: args.description,
        repo: args.repo,
        pin: args.pin,
        crate_name: args.crate_name,
        package: args.package,
        assets,
        bins: args.bins,
        ..ToolDef::default()
    };
    if tool.kind().is_none() {
        bail!("give --repo, --crate, or --kind");
    }
    let file = args
        .file
        .unwrap_or_else(|| session.workspace.main_config_file());
    crate::config_edit::add_tool(&file, &args.name, &tool)?;
    if let Err(error) = session.reload() {
        // Leave the file as it was before the bad definition went in.
        bail!(
            "{error:#}\nThe new definition is in {}; fix or remove it.",
            file.display()
        );
    }
    if args.install {
        let report = session.install(std::slice::from_ref(&args.name), log)?;
        return Ok(output.report(&report));
    }
    output.done(
        true,
        &format!("added tools.{} to {}", args.name, file.display()),
        json!({ "tool": args.name, "file": file }),
    );
    Ok(0)
}

fn preset(session: &mut Session, command: PresetCommand, output: &Output) -> Result<i32> {
    match command {
        PresetCommand::List => {
            let selected = crate::selection::expand_presets(
                &session.workspace.host.presets,
                &session.workspace.config.presets,
            );
            if output.json {
                output.print_json(json!({ "changed": false, "presets": session.workspace.config.presets, "selected": selected }));
            } else {
                for (name, preset) in &session.workspace.config.presets {
                    let marker = if selected.contains(name) { "*" } else { " " };
                    let mut line = format!("{marker} {name:<16} {}", preset.tools.join(" "));
                    if !preset.extends.is_empty() {
                        line.push_str(&format!("  (extends {})", preset.extends.join(", ")));
                    }
                    println!("{line}");
                }
            }
        }
        PresetCommand::Create {
            name,
            tools,
            packages,
            extends,
            description,
            select,
        } => {
            let preset = PresetDef {
                description,
                extends,
                tools,
                packages,
                ..PresetDef::default()
            };
            let file = session.workspace.main_config_file();
            crate::config_edit::add_preset(&file, &name, &preset)?;
            if select {
                crate::config_edit::host_list(&session.host_path, "presets", &name, true)?;
            }
            session.reload().with_context(|| {
                format!("the new preset is in {}; fix or remove it", file.display())
            })?;
            output.done(
                true,
                &format!("added presets.{name} to {}", file.display()),
                json!({ "preset": name }),
            );
        }
        PresetCommand::Select { name } => {
            if !session.workspace.config.presets.contains_key(&name) {
                bail!("no preset named `{name}`");
            }
            let changed =
                crate::config_edit::host_list(&session.host_path, "presets", &name, true)?;
            output.done(
                changed,
                &format!("selected {name}; run `dfutil apply`"),
                json!({ "preset": name }),
            );
        }
        PresetCommand::Deselect { name } => {
            let changed =
                crate::config_edit::host_list(&session.host_path, "presets", &name, false)?;
            output.done(
                changed,
                &format!("deselected {name}; run `dfutil apply` to remove its tools"),
                json!({ "preset": name }),
            );
        }
    }
    Ok(0)
}

fn hook_pre_commit(
    host_path: &std::path::Path,
    home: Option<&std::path::Path>,
    repo: &std::path::Path,
) -> Result<i32> {
    let workspace = crate::config::Workspace::load(host_path, home)?;
    let secrets = crate::deploy::template::secret_values(&workspace);
    let leaks = crate::gitops::staged_leaks(repo, &secrets)?;
    if leaks.is_empty() {
        return Ok(0);
    }
    eprintln!("dfutil: commit blocked; staged changes contain secret template values:");
    for leak in &leaks {
        eprintln!("  {}: the value of `{}`", leak.file, leak.var);
    }
    eprintln!("Replace each with `{{{{ <name> }}}}` in a .tmpl file, or unstage it.");
    Ok(1)
}

/// Human or JSON output.
struct Output {
    json: bool,
}

impl Output {
    fn print_json(&self, value: serde_json::Value) {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_default()
        );
    }

    fn done(&self, changed: bool, message: &str, mut extra: serde_json::Value) {
        if self.json {
            if let Some(object) = extra.as_object_mut() {
                object.insert("changed".into(), json!(changed));
                object.insert("message".into(), json!(message));
            }
            self.print_json(extra);
        } else {
            println!("{message}");
        }
    }

    fn plan(&self, plan: &Plan, all: bool, notes: &[String]) {
        if self.json {
            self.print_json(json!({ "changed": false, "tools": plan.rows, "notes": notes }));
            return;
        }
        println!(
            "{:<20} {:<9} {:<18} {:<18} {:<10} STATUS",
            "TOOL", "SELECTED", "INSTALLED", "AVAILABLE", "PIN"
        );
        for row in &plan.rows {
            if !all && !row.selected && row.installed.is_none() {
                continue;
            }
            let status = match row.action {
                Action::Keep => "ok".to_string(),
                Action::Available => String::new(),
                Action::Error => format!("error: {}", row.error.clone().unwrap_or_default()),
                action => format!("{action:?}: {}", row.why).to_lowercase(),
            };
            println!(
                "{:<20} {:<9} {:<18} {:<18} {:<10} {}",
                row.name,
                if row.selected { "yes" } else { "" },
                row.installed.clone().unwrap_or_else(|| "-".into()),
                row.available.clone().unwrap_or_else(|| "-".into()),
                row.pin,
                status
            );
        }
        for note in notes {
            println!("note: {note}");
        }
    }

    fn deploy(&self, items: &[deploy::Item], dry_run: bool) {
        if self.json {
            let changed = !dry_run
                && items.iter().any(|item| {
                    !matches!(
                        item.change,
                        deploy::Change::Unchanged | deploy::Change::Skipped | deploy::Change::Error
                    )
                });
            self.print_json(json!({ "changed": changed, "dotfiles": items }));
        } else {
            print!("{}", deploy_text(items, dry_run));
        }
    }

    /// Prints a report and returns the exit code.
    fn report(&self, report: &ApplyReport) -> i32 {
        if self.json {
            let mut value = serde_json::to_value(report).unwrap_or_default();
            if let Some(object) = value.as_object_mut() {
                object.insert("changed".into(), json!(report.changed()));
                object.insert("failed".into(), json!(report.failed()));
            }
            self.print_json(value);
        } else {
            for outcome in &report.tools {
                let status = if outcome.ok { "ok" } else { "FAILED" };
                println!(
                    "{status:<7} {:<20} {:?} {}",
                    outcome.name, outcome.action, outcome.message
                );
            }
            for error in &report.resolve_errors {
                println!("FAILED  {error}");
            }
            for sync in &report.sync {
                println!("sync    {}: {}", sync.root, ops::describe_sync(sync));
            }
            print!("{}", deploy_text(&report.dotfiles, false));
            for note in &report.notes {
                println!("note: {note}");
            }
            if !report.changed() && !report.failed() {
                println!("everything is up to date");
            }
        }
        i32::from(report.failed())
    }
}

fn plan_text(plan: &Plan) -> String {
    let mut text = String::new();
    for row in plan.changes() {
        text.push_str(&format!(
            "{:<10} {:<20} {}\n",
            format!("{:?}", row.action).to_lowercase(),
            row.name,
            row.why
        ));
        for note in &row.notes {
            text.push_str(&format!("{:<10} {:<20} note: {note}\n", "", ""));
        }
    }
    for row in plan.errors() {
        text.push_str(&format!(
            "{:<10} {:<20} {}\n",
            "error",
            row.name,
            row.error.clone().unwrap_or_default()
        ));
    }
    if text.is_empty() {
        text.push_str("tools: nothing to do\n");
    }
    text
}

fn deploy_text(items: &[deploy::Item], dry_run: bool) -> String {
    let mut text = String::new();
    for item in items
        .iter()
        .filter(|item| item.change != deploy::Change::Unchanged)
    {
        let verb = match (item.change, dry_run) {
            (deploy::Change::Linked, false) => "linked",
            (deploy::Change::Linked, true) => "would link",
            (deploy::Change::Rerendered, false) => "re-rendered",
            (deploy::Change::Rerendered, true) => "would re-render",
            (deploy::Change::BackedUp, false) => "backed up",
            (deploy::Change::BackedUp, true) => "would back up",
            (deploy::Change::Pruned, false) => "removed",
            (deploy::Change::Pruned, true) => "would remove",
            (deploy::Change::Skipped, _) => "skipped",
            (deploy::Change::Error, _) => "error",
            (deploy::Change::Unchanged, _) => "unchanged",
        };
        text.push_str(&format!("{verb:<16} {}", item.target.display()));
        if !item.detail.is_empty() {
            text.push_str(&format!(" ({})", item.detail));
        }
        text.push('\n');
    }
    text
}

fn capture_text(edit: &deploy::Edited, dry_run: bool) -> String {
    let mut text = format!("{} was edited in place\n", edit.target.display());
    match (&edit.problem, &edit.captured) {
        (Some(problem), _) => text.push_str(&format!("  can't capture: {problem}\n")),
        (None, Some(captured)) => {
            let current = std::fs::read_to_string(&edit.template).unwrap_or_default();
            let diff = similar::TextDiff::from_lines(&current, captured);
            let verb = if dry_run { "would update" } else { "updated" };
            text.push_str(&format!("  {verb} {}:\n", edit.template.display()));
            text.push_str(&diff.unified_diff().context_radius(2).to_string());
            if !edit.restored_lines.is_empty() {
                text.push_str(&format!(
                    "  check lines {:?}: variables were restored by matching their values\n",
                    edit.restored_lines
                ));
            }
        }
        (None, None) => {}
    }
    text
}

//! Application state and key handling. Nothing here touches the terminal:
//! [`App::handle_key`] and [`App::on_event`] are plain state transitions, and
//! the event loop in `mod.rs` carries out the [`Effect`]s they return.

use super::input::{Field, Form, FormEvent, FormKind};
use super::job::{JobContext, JobEvent, JobOutput, JobRequest};
use super::state::*;
use crate::config::{PresetDef, ToolDef, ToolKind, is_valid_name};
use crate::deploy::{self, Change, Edited, Item};
use crate::gitops::SyncResult;
use crate::ops::{self, ApplyReport, Session};
use crate::plan::{Action, Plan};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{BTreeMap, VecDeque};

/// Lines kept in the log pane.
const LOG_CAP: usize = 1000;
/// Rows moved by Page Up and Page Down.
const PAGE: usize = 10;

/// Field positions in the add-tool form.
pub const F_NAME: usize = 0;
pub const F_KIND: usize = 1;
pub const F_REPO: usize = 2;
pub const F_PIN: usize = 3;
pub const F_CRATE: usize = 4;
pub const F_ASSET: usize = 5;
pub const F_BINS: usize = 6;
pub const F_DESCRIPTION: usize = 7;
pub const F_INSTALL_NOW: usize = 8;

/// Kinds offered by the add-tool form. `script` tools need install commands
/// the form has no room for, so they stay a hand edit of `dfapp.toml`.
const KIND_CHOICES: [&str; 5] = ["auto", "github", "cargo", "npm", "uv"];

/// What the event loop must do after a key or an event.
#[derive(Debug)]
pub enum Effect {
    None,
    Quit,
    /// Run this job on a worker thread. The app has already marked it running.
    Start(JobRequest),
}

/// A yes/no dialog. Without a request it only shows information.
#[derive(Debug)]
pub struct Confirm {
    pub title: String,
    pub lines: Vec<String>,
    pub scroll: usize,
    pub request: Option<JobRequest>,
}

#[derive(Debug)]
pub enum Modal {
    Help,
    Confirm(Confirm),
    Form(Form),
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub text: String,
    pub error: bool,
}

/// The one-line message under the tabs; cleared by the next key press.
#[derive(Clone, Debug)]
pub struct StatusMessage {
    pub text: String,
    pub error: bool,
}

#[derive(Debug)]
pub struct RunningJob {
    pub label: String,
    /// Reload the session and refresh these views when the job ends.
    reload: bool,
    after: Vec<JobRequest>,
    /// Whether this was the version lookup, whose failure the Tools tab shows.
    is_plan: bool,
}

pub struct App {
    pub session: Session,
    pub tab: Tab,
    pub tools: ToolsState,
    pub presets: PresetsState,
    pub dotfiles: DotfilesState,
    pub sync: SyncState,
    pub settings: SettingsState,
    pub modal: Option<Modal>,
    pub log: VecDeque<LogLine>,
    pub log_large: bool,
    pub status: Option<StatusMessage>,
    pub job: Option<RunningJob>,
    pub quit_armed: bool,
    /// Animation frame for the running-job indicator.
    pub frame: usize,
    /// Read-only refreshes waiting for the worker to be free.
    wanted: VecDeque<JobRequest>,
}

impl App {
    pub fn new(session: Session) -> Self {
        let mut app = Self {
            session,
            tab: Tab::Tools,
            tools: ToolsState::default(),
            presets: PresetsState::default(),
            dotfiles: DotfilesState::default(),
            sync: SyncState::default(),
            settings: SettingsState::default(),
            modal: None,
            log: VecDeque::new(),
            log_large: false,
            status: None,
            job: None,
            quit_armed: false,
            frame: 0,
            wanted: VecDeque::new(),
        };
        app.want(JobRequest::LoadPlan);
        app.want(JobRequest::LoadDotfiles);
        let notes = app.session.workspace.notes.clone();
        for note in notes {
            app.log_line(format!("note: {note}"));
        }
        app
    }

    pub fn context(&self) -> JobContext {
        JobContext::from_session(&self.session)
    }

    // ----- log and status -------------------------------------------------

    pub fn log_line(&mut self, text: impl AsRef<str>) {
        self.push_log(text.as_ref(), false);
    }

    pub fn log_error(&mut self, text: impl AsRef<str>) {
        self.push_log(text.as_ref(), true);
    }

    fn push_log(&mut self, text: &str, error: bool) {
        for line in text.lines() {
            if self.log.len() >= LOG_CAP {
                self.log.pop_front();
            }
            self.log.push_back(LogLine {
                text: clean(line),
                error,
            });
        }
    }

    fn info(&mut self, text: impl Into<String>) {
        self.status = Some(StatusMessage {
            text: text.into(),
            error: false,
        });
    }

    /// An error message in the status line and the log.
    fn fail(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.log_error(&text);
        self.status = Some(StatusMessage {
            text: clean(text.lines().next().unwrap_or("")),
            error: true,
        });
    }

    // ----- jobs -----------------------------------------------------------

    /// Queues a read-only refresh for when the worker is free.
    pub fn want(&mut self, request: JobRequest) {
        let kind = std::mem::discriminant(&request);
        if !self
            .wanted
            .iter()
            .any(|queued| std::mem::discriminant(queued) == kind)
        {
            self.wanted.push_back(request);
        }
    }

    /// Starts the next queued refresh if no job is running.
    pub fn pump(&mut self) -> Effect {
        if self.job.is_some() {
            return Effect::None;
        }
        match self.wanted.pop_front() {
            Some(request) => self.begin(request),
            None => Effect::None,
        }
    }

    /// Starts a user-requested job, or refuses it while another runs.
    fn start(&mut self, request: JobRequest) -> Effect {
        if self.refuse_if_busy() {
            return Effect::None;
        }
        self.begin(request)
    }

    fn refuse_if_busy(&mut self) -> bool {
        let Some(job) = &self.job else { return false };
        let message = format!(
            "busy: {} is still running; try again when it finishes",
            job.label
        );
        self.fail(message);
        true
    }

    fn begin(&mut self, request: JobRequest) -> Effect {
        let mut after = Vec::new();
        if request.mutates() {
            after.push(JobRequest::LoadPlan);
            after.push(JobRequest::LoadDotfiles);
            if self.sync.requested || matches!(request, JobRequest::Sync | JobRequest::Update) {
                after.push(JobRequest::LoadSync);
            }
        }
        let label = request.label();
        self.log_line(format!("> {label}"));
        self.job = Some(RunningJob {
            label,
            reload: request.mutates(),
            after,
            is_plan: matches!(request, JobRequest::LoadPlan),
        });
        Effect::Start(request)
    }

    /// Applies a message from the worker.
    pub fn on_event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Log(line) => self.log_line(line),
            JobEvent::Done(result) => {
                let job = self.job.take();
                match result {
                    Ok(output) => self.apply_output(output),
                    Err(error) => {
                        if let Some(job) = &job {
                            if job.is_plan {
                                self.tools.error = Some(error.clone());
                            }
                            self.fail(format!("{} failed: {error}", job.label));
                        } else {
                            self.fail(error);
                        }
                    }
                }
                if let Some(job) = job {
                    if job.reload {
                        self.reload_session();
                    }
                    for request in job.after {
                        self.want(request);
                    }
                }
            }
        }
    }

    fn reload_session(&mut self) {
        match self.session.reload() {
            Ok(()) => {
                let tools = self.session.workspace.config.tools.len();
                self.tools.list.clamp(tools);
                let presets = self.session.workspace.config.presets.len();
                self.presets.list.clamp(presets);
            }
            Err(error) => self.fail(format!("reloading the config failed: {error:#}")),
        }
    }

    fn apply_output(&mut self, output: JobOutput) {
        match output {
            JobOutput::Plan(plan) => {
                let errors = plan.errors().count();
                if errors > 0 {
                    self.log_error(format!(
                        "{errors} tool(s) could not be looked up (see the Tools tab)"
                    ));
                }
                self.tools.set_plan(plan);
            }
            JobOutput::Dotfiles {
                items,
                edited,
                problems,
            } => {
                for problem in &problems {
                    self.log_error(problem);
                }
                let len = items.len();
                self.dotfiles.items = Some(items);
                self.dotfiles.edited = edited;
                self.dotfiles.problems = problems;
                self.dotfiles.targets.clamp(len);
            }
            JobOutput::Sync { incoming, outgoing } => {
                self.sync.data = Some((incoming, outgoing));
                let len = self.sync.entries().len();
                self.sync.list.clamp(len);
                self.sync.diff_scroll = 0;
            }
            JobOutput::Schedule(status) => self.settings.schedule = Some(status),
            JobOutput::Report(report) => self.show_report(&report),
            JobOutput::Deployed(items) => {
                let (lines, errors) = deploy_summary(&items);
                for line in lines {
                    self.log_line(line);
                }
                if errors > 0 {
                    self.fail(format!("deploy finished with {errors} error(s)"));
                } else {
                    self.info("dotfiles deployed");
                }
            }
            JobOutput::Captured(edited) => {
                let mut problems = 0;
                for edit in &edited {
                    match &edit.problem {
                        None => self.log_line(format!(
                            "captured {} into {}",
                            edit.target.display(),
                            edit.template.display()
                        )),
                        Some(problem) => {
                            problems += 1;
                            self.log_error(format!("{}: {problem}", edit.target.display()));
                        }
                    }
                }
                if problems > 0 {
                    self.fail(format!("{problems} edit(s) could not be captured"));
                } else {
                    self.info(format!("captured {} edited file(s)", edited.len()));
                }
            }
            JobOutput::Message(text) => {
                self.log_line(&text);
                self.info(text);
            }
        }
    }

    fn show_report(&mut self, report: &ApplyReport) {
        for outcome in &report.tools {
            let line = format!(
                "{}: {} {}{}",
                outcome.name,
                action_label(outcome.action),
                if outcome.ok { "ok" } else { "FAILED" },
                if outcome.message.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", outcome.message)
                }
            );
            if outcome.ok {
                self.log_line(line);
            } else {
                self.log_error(line);
            }
        }
        for error in &report.resolve_errors {
            self.log_error(format!("could not look up {error}"));
        }
        let (lines, _) = deploy_summary(&report.dotfiles);
        for line in lines {
            self.log_line(line);
        }
        for sync in &report.sync {
            let line = format!("sync {}: {}", sync.root, ops::describe_sync(sync));
            if matches!(
                sync.result,
                SyncResult::RolledBack { .. } | SyncResult::Failed { .. }
            ) {
                self.log_error(line);
            } else {
                self.log_line(line);
            }
        }
        for note in &report.notes {
            self.log_line(format!("note: {note}"));
        }
        if report.failed() {
            self.fail("finished with failures; see the log");
        } else if report.changed() {
            self.info("done");
        } else {
            self.info("done, nothing changed");
        }
    }

    // ----- keys -----------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        let armed = std::mem::take(&mut self.quit_armed);
        self.status = None;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            return self.request_quit(armed);
        }
        if let Some(modal) = self.modal.take() {
            return self.modal_key(modal, key);
        }
        if self.tools.filtering {
            self.filter_key(key);
            return Effect::None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc if !control => return self.request_quit(armed),
            KeyCode::Tab => return self.goto(self.tab.next()),
            KeyCode::BackTab => return self.goto(self.tab.prev()),
            KeyCode::Char(digit @ '1'..='5') if !control => {
                return self.goto(Tab::ALL[digit as usize - '1' as usize]);
            }
            KeyCode::Char('?') => {
                self.modal = Some(Modal::Help);
                return Effect::None;
            }
            KeyCode::Char('l') if !control => {
                self.log_large = !self.log_large;
                return Effect::None;
            }
            _ => {}
        }
        match self.tab {
            Tab::Tools => self.tools_key(key),
            Tab::Presets => self.presets_key(key),
            Tab::Dotfiles => self.dotfiles_key(key),
            Tab::Sync => self.sync_key(key),
            Tab::Settings => self.settings_key(key),
        }
    }

    fn request_quit(&mut self, armed: bool) -> Effect {
        if self.job.is_some() && !armed {
            self.quit_armed = true;
            let label = self.job.as_ref().map(|job| job.label.clone());
            self.info(format!(
                "{} is still running; press q again to quit anyway",
                label.unwrap_or_default()
            ));
            return Effect::None;
        }
        Effect::Quit
    }

    fn goto(&mut self, tab: Tab) -> Effect {
        self.tab = tab;
        match tab {
            Tab::Sync if !self.sync.requested => {
                self.sync.requested = true;
                self.want(JobRequest::LoadSync);
            }
            Tab::Settings if !self.settings.requested => {
                self.settings.requested = true;
                self.want(JobRequest::LoadSchedule);
            }
            _ => {}
        }
        Effect::None
    }

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) -> Effect {
        match modal {
            Modal::Help => Effect::None,
            Modal::Confirm(mut confirm) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') if confirm.request.is_some() => {
                        if let Some(job) = &self.job {
                            // Keep the dialog so the choice isn't lost.
                            let label = job.label.clone();
                            self.info(format!(
                                "{label} is still running; press y again when it finishes"
                            ));
                            self.modal = Some(Modal::Confirm(confirm));
                            return Effect::None;
                        }
                        let request = confirm.request.take().expect("checked above");
                        return self.start(request);
                    }
                    KeyCode::Down | KeyCode::Char('j') => confirm.scroll += 1,
                    KeyCode::Up | KeyCode::Char('k') => {
                        confirm.scroll = confirm.scroll.saturating_sub(1)
                    }
                    KeyCode::PageDown => confirm.scroll += PAGE,
                    KeyCode::PageUp => confirm.scroll = confirm.scroll.saturating_sub(PAGE),
                    KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => return Effect::None,
                    KeyCode::Enter if confirm.request.is_none() => return Effect::None,
                    _ => {}
                }
                confirm.scroll = confirm.scroll.min(confirm.lines.len().saturating_sub(1));
                self.modal = Some(Modal::Confirm(confirm));
                Effect::None
            }
            Modal::Form(mut form) => match form.handle_key(key) {
                FormEvent::Pending => {
                    self.modal = Some(Modal::Form(form));
                    Effect::None
                }
                FormEvent::Cancel => Effect::None,
                FormEvent::Submit => match self.submit_form(&form) {
                    Ok(effect) => effect,
                    Err(message) => {
                        form.error = Some(message);
                        self.modal = Some(Modal::Form(form));
                        Effect::None
                    }
                },
            },
        }
    }

    // ----- tools tab ------------------------------------------------------

    fn filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.tools.filtering = false,
            KeyCode::Esc => {
                self.tools.filter.clear();
                self.tools.filtering = false;
            }
            _ => {
                self.tools.filter.handle(key);
            }
        }
        self.tools.list.home();
    }

    fn tools_key(&mut self, key: KeyEvent) -> Effect {
        let len = self.tools.visible().len();
        let list = &mut self.tools.list;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => list.down(1, len),
            KeyCode::Up | KeyCode::Char('k') => list.up(1),
            KeyCode::PageDown => list.down(PAGE, len),
            KeyCode::PageUp => list.up(PAGE),
            KeyCode::Home | KeyCode::Char('g') => list.home(),
            KeyCode::End | KeyCode::Char('G') => list.end(len),
            KeyCode::Char(' ') => {
                if let Some(name) = self.tools.current().map(|row| row.name.clone())
                    && !self.tools.marked.remove(&name)
                {
                    self.tools.marked.insert(name);
                }
                self.tools.list.down(1, len);
            }
            KeyCode::Char('m') => {
                let names: Vec<String> = self
                    .tools
                    .visible()
                    .iter()
                    .map(|row| row.name.clone())
                    .collect();
                if names.iter().all(|name| self.tools.marked.contains(name)) {
                    for name in &names {
                        self.tools.marked.remove(name);
                    }
                } else {
                    self.tools.marked.extend(names);
                }
            }
            KeyCode::Char('/') => self.tools.filtering = true,
            KeyCode::Char('R') => {
                self.want(JobRequest::LoadPlan);
                self.info("refreshing tool versions");
            }
            KeyCode::Char('a') => self.tools_apply(),
            KeyCode::Char('r') => self.tools_reinstall(),
            KeyCode::Char('i') => return self.tools_install(),
            KeyCode::Char('u') => self.tools_uninstall(),
            KeyCode::Char('b') => self.tools_rollback(),
            KeyCode::Char('n') => self.open_add_tool(),
            _ => {}
        }
        Effect::None
    }

    fn tools_apply(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let Some(plan) = self.tools.plan.clone() else {
            self.fail("the tool list hasn't loaded yet");
            return;
        };
        let count = plan.changes().count();
        if count == 0 {
            let errors = plan.errors().count();
            self.info(if errors > 0 {
                format!("nothing to apply ({errors} tool(s) could not be looked up)")
            } else {
                "nothing to apply: every selected tool is up to date".to_string()
            });
            return;
        }
        let mut lines = plan_lines(&plan);
        lines.push(String::new());
        lines.push("Dotfiles are deployed afterwards.".into());
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("Apply {count} change(s)?"),
            lines,
            scroll: 0,
            request: Some(JobRequest::Apply(plan)),
        }));
    }

    fn tools_reinstall(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let names: Vec<String> = self
            .tools
            .targets()
            .into_iter()
            .filter(|name| self.row(name).is_some_and(|row| row.selected))
            .collect();
        if names.is_empty() {
            self.fail("reinstall works on selected tools; mark some, or highlight one");
            return;
        }
        let lines = names
            .iter()
            .map(
                |name| match self.row(name).and_then(|row| row.installed.clone()) {
                    Some(version) => format!("reinstall {name} (installed {version})"),
                    None => format!("install {name} (not installed yet)"),
                },
            )
            .collect();
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("Reinstall {} tool(s)?", names.len()),
            lines,
            scroll: 0,
            request: Some(JobRequest::Reinstall(names)),
        }));
    }

    fn tools_install(&mut self) -> Effect {
        let Some(row) = self.tools.current() else {
            return Effect::None;
        };
        let (name, selected) = (row.name.clone(), row.selected);
        if !self.session.workspace.config.tools.contains_key(&name) {
            self.fail(format!("{name} isn't defined in the config"));
            return Effect::None;
        }
        if selected {
            self.info(format!("{name} is already selected; press a to install it"));
            return Effect::None;
        }
        self.start(JobRequest::Install(vec![name]))
    }

    fn tools_uninstall(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let names: Vec<String> = self
            .tools
            .targets()
            .into_iter()
            .filter(|name| {
                self.row(name)
                    .is_some_and(|row| row.selected || row.installed.is_some())
            })
            .collect();
        if names.is_empty() {
            self.fail("nothing to uninstall: the tool is neither selected nor installed");
            return;
        }
        let mut lines: Vec<String> = names
            .iter()
            .map(
                |name| match self.row(name).and_then(|row| row.installed.clone()) {
                    Some(version) => format!("uninstall {name} (installed {version})"),
                    None => format!("deselect {name} (not installed)"),
                },
            )
            .collect();
        lines.push(String::new());
        lines.push(
            "The tools are dropped from this host's list, or excluded when a preset selects them."
                .into(),
        );
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("Uninstall {} tool(s)?", names.len()),
            lines,
            scroll: 0,
            request: Some(JobRequest::Uninstall(names)),
        }));
    }

    fn tools_rollback(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let Some(row) = self.tools.current() else {
            return;
        };
        if !row.has_previous {
            let name = row.name.clone();
            self.fail(format!("{name} has no previous version to roll back to"));
            return;
        }
        let (name, installed) = (row.name.clone(), row.installed.clone().unwrap_or_default());
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("Roll back {name}?"),
            lines: vec![format!(
                "{name} goes from {installed} back to the version it had before its last install."
            )],
            scroll: 0,
            request: Some(JobRequest::Rollback(name)),
        }));
    }

    fn row(&self, name: &str) -> Option<&crate::plan::ToolRow> {
        self.tools
            .plan
            .as_ref()?
            .rows
            .iter()
            .find(|row| row.name == name)
    }

    // ----- add-tool form --------------------------------------------------

    /// The key of the asset table entry the form writes (`linux-x86_64`).
    pub fn asset_key(&self) -> String {
        let [key, ..] = self.session.workspace.platform.asset_keys();
        key
    }

    fn open_add_tool(&mut self) {
        let key = self.asset_key();
        let fields = vec![
            Field::text("name", "letters, digits, . _ -", ""),
            Field::choice(
                "kind",
                "left/right to change; auto picks github from a repo, cargo from a crate",
                &KIND_CHOICES,
                0,
            ),
            Field::text("repo", "GitHub owner/name, for release downloads", ""),
            Field::text(
                "pin",
                "version pin, such as 15, 15.1, or v1.2.3; empty for newest",
                "",
            ),
            Field::text("crate", "crates.io crate, enables source builds", ""),
            Field::text(
                &format!("asset ({key})"),
                "release asset glob for this platform, such as tool-*-linux-x86_64.tar.gz",
                "",
            ),
            Field::text(
                "bins",
                "binaries to link, separated by spaces or commas",
                "",
            ),
            Field::text("description", "one line", ""),
            Field::check("install now", "space toggles; installs after adding", false),
        ];
        self.modal = Some(Modal::Form(Form::new(
            FormKind::AddTool,
            "Add a tool",
            fields,
        )));
    }

    /// Builds the tool the form describes, or says what is wrong with it.
    pub fn tool_from_form(&self, form: &Form) -> Result<(String, ToolDef), String> {
        let name = form.value(F_NAME).to_string();
        if !is_valid_name(&name) {
            return Err(
                "name: use letters, digits, `.`, `_`, `-`, and don't start with `.`".into(),
            );
        }
        if self.session.workspace.config.tools.contains_key(&name) {
            return Err(format!("name: tools.{name} already exists"));
        }
        let optional = |index: usize| {
            let value = form.value(index);
            (!value.is_empty()).then(|| value.to_string())
        };
        let kind = match form.value(F_KIND) {
            "github" => Some(ToolKind::Github),
            "cargo" => Some(ToolKind::Cargo),
            "npm" => Some(ToolKind::Npm),
            "uv" => Some(ToolKind::Uv),
            _ => None,
        };
        let mut assets = BTreeMap::new();
        if let Some(glob) = optional(F_ASSET) {
            assets.insert(self.asset_key(), glob);
        }
        let bins = form
            .value(F_BINS)
            .split([' ', ','])
            .filter(|bin| !bin.is_empty())
            .map(str::to_string)
            .collect();
        let tool = ToolDef {
            kind,
            description: optional(F_DESCRIPTION),
            repo: optional(F_REPO),
            pin: optional(F_PIN),
            crate_name: optional(F_CRATE),
            assets,
            bins,
            ..ToolDef::default()
        };
        if tool.kind().is_none() {
            return Err("give a repo or a crate, or pick a kind".into());
        }
        // Let the engine's own checks judge the result before anything is written.
        let mut trial = self.session.workspace.clone();
        trial.config.tools.insert(name.clone(), tool.clone());
        let marker = format!("tools.{name}");
        let problems: Vec<String> = trial
            .validate()
            .into_iter()
            .filter_map(|problem| {
                problem
                    .find(&marker)
                    .map(|position| problem[position..].to_string())
            })
            .collect();
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        Ok((name, tool))
    }

    fn submit_form(&mut self, form: &Form) -> Result<Effect, String> {
        if let Some(job) = &self.job {
            return Err(format!(
                "{} is still running; wait for it to finish",
                job.label
            ));
        }
        match &form.kind {
            FormKind::AddTool => {
                let (name, tool) = self.tool_from_form(form)?;
                let file = self.session.workspace.main_config_file();
                crate::config_edit::add_tool(&file, &name, &tool)
                    .map_err(|error| format!("{error:#}"))?;
                self.session.reload().map_err(|error| {
                    format!("{error:#}\nthe new definition is in {}", file.display())
                })?;
                self.log_line(format!("added tools.{name} to {}", file.display()));
                self.info(format!("added {name}"));
                self.want(JobRequest::LoadPlan);
                if form.checked(F_INSTALL_NOW) {
                    return Ok(self.start(JobRequest::Install(vec![name])));
                }
                Ok(Effect::None)
            }
            FormKind::CreatePreset => {
                let name = form.value(0).to_string();
                if !is_valid_name(&name) {
                    return Err(
                        "name: use letters, digits, `.`, `_`, `-`, and don't start with `.`".into(),
                    );
                }
                if self.session.workspace.config.presets.contains_key(&name) {
                    return Err(format!("name: presets.{name} already exists"));
                }
                let tools: Vec<String> = self.tools.marked.iter().cloned().collect();
                if tools.is_empty() {
                    return Err("mark tools on the Tools tab first (space)".into());
                }
                if let Some(unknown) = tools
                    .iter()
                    .find(|tool| !self.session.workspace.config.tools.contains_key(*tool))
                {
                    return Err(format!("{unknown} isn't defined in the config"));
                }
                let description = form.value(1);
                let preset = PresetDef {
                    description: (!description.is_empty()).then(|| description.to_string()),
                    tools,
                    ..PresetDef::default()
                };
                let file = self.session.workspace.main_config_file();
                crate::config_edit::add_preset(&file, &name, &preset)
                    .map_err(|error| format!("{error:#}"))?;
                self.session.reload().map_err(|error| {
                    format!("{error:#}\nthe new preset is in {}", file.display())
                })?;
                self.log_line(format!("added presets.{name} to {}", file.display()));
                self.info(format!(
                    "created preset {name}; select it on the Presets tab"
                ));
                Ok(Effect::None)
            }
            FormKind::EditVar(name) => {
                let secret = self
                    .session
                    .workspace
                    .config
                    .vars
                    .get(name)
                    .is_some_and(|definition| definition.is_secret());
                let value = form.fields[0].input.text().to_string();
                if secret && value.is_empty() {
                    return Err("type a value, or press Esc to keep the current one".into());
                }
                crate::config_edit::host_var(&self.session.host_path, name, &value)
                    .map_err(|error| format!("{error:#}"))?;
                self.session
                    .reload()
                    .map_err(|error| format!("{error:#}"))?;
                self.log_line(format!("set variable {name}"));
                self.info(format!("set {name}; press d on the Dotfiles tab to deploy"));
                self.want(JobRequest::LoadDotfiles);
                Ok(Effect::None)
            }
        }
    }

    // ----- presets tab ----------------------------------------------------

    pub fn preset_names(&self) -> Vec<String> {
        self.session
            .workspace
            .config
            .presets
            .keys()
            .cloned()
            .collect()
    }

    fn presets_key(&mut self, key: KeyEvent) -> Effect {
        let len = self.session.workspace.config.presets.len();
        let list = &mut self.presets.list;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => list.down(1, len),
            KeyCode::Up | KeyCode::Char('k') => list.up(1),
            KeyCode::PageDown => list.down(PAGE, len),
            KeyCode::PageUp => list.up(PAGE),
            KeyCode::Home | KeyCode::Char('g') => list.home(),
            KeyCode::End | KeyCode::Char('G') => list.end(len),
            KeyCode::Char(' ') | KeyCode::Enter => self.toggle_preset(),
            KeyCode::Char('n') => self.open_create_preset(),
            _ => {}
        }
        Effect::None
    }

    fn toggle_preset(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let Some(name) = self.preset_names().get(self.presets.list.cursor).cloned() else {
            return;
        };
        let selected = self.session.workspace.host.presets.contains(&name);
        let result =
            crate::config_edit::host_list(&self.session.host_path, "presets", &name, !selected);
        if let Err(error) = result {
            self.fail(format!("{error:#}"));
            return;
        }
        self.reload_session();
        self.want(JobRequest::LoadPlan);
        self.want(JobRequest::LoadDotfiles);
        let verb = if selected { "deselected" } else { "selected" };
        self.log_line(format!("{verb} preset {name}"));
        self.info(format!(
            "{verb} preset {name}; press a on the Tools tab to apply"
        ));
    }

    fn open_create_preset(&mut self) {
        if self.tools.marked.is_empty() {
            self.fail("mark tools on the Tools tab first (space), then press n here");
            return;
        }
        let hint = format!(
            "{} marked tool(s) go into the preset",
            self.tools.marked.len()
        );
        self.modal = Some(Modal::Form(Form::new(
            FormKind::CreatePreset,
            "Create a preset from the marked tools",
            vec![
                Field::text("name", &hint, ""),
                Field::text("description", "one line, optional", ""),
            ],
        )));
    }

    // ----- dotfiles tab ---------------------------------------------------

    pub fn palette_names(&self) -> Vec<String> {
        self.session.workspace.palettes.keys().cloned().collect()
    }

    pub fn var_names(&self) -> Vec<String> {
        self.session.workspace.config.vars.keys().cloned().collect()
    }

    fn dotfiles_key(&mut self, key: KeyEvent) -> Effect {
        let lens = (
            self.dotfiles.visible_items().len(),
            self.session.workspace.palettes.len(),
            self.session.workspace.config.vars.len(),
        );
        let focus = self.dotfiles.focus;
        let (list, len) = match focus {
            DotPane::Targets => (&mut self.dotfiles.targets, lens.0),
            DotPane::Palettes => (&mut self.dotfiles.palettes, lens.1),
            DotPane::Vars => (&mut self.dotfiles.vars, lens.2),
        };
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => list.down(1, len),
            KeyCode::Up | KeyCode::Char('k') => list.up(1),
            KeyCode::PageDown => list.down(PAGE, len),
            KeyCode::PageUp => list.up(PAGE),
            KeyCode::Home | KeyCode::Char('g') => list.home(),
            KeyCode::End | KeyCode::Char('G') => list.end(len),
            KeyCode::Right => self.dotfiles.focus = focus.right(),
            KeyCode::Left => self.dotfiles.focus = focus.left(),
            KeyCode::Char('v') => {
                self.dotfiles.changes_only = !self.dotfiles.changes_only;
                self.dotfiles.targets.home();
            }
            KeyCode::Char('R') => {
                self.want(JobRequest::LoadDotfiles);
                self.info("refreshing the dotfiles preview");
            }
            KeyCode::Char('d') => return self.start(JobRequest::Deploy),
            KeyCode::Char('c') => self.dotfiles_capture(),
            KeyCode::Char('p') => return self.cycle_palette(),
            KeyCode::Char('e') => self.open_edit_var(),
            KeyCode::Enter | KeyCode::Char(' ') if focus == DotPane::Palettes => {
                let names = self.palette_names();
                if let Some(name) = names.get(self.dotfiles.palettes.cursor) {
                    return self.pick_palette(name.clone());
                }
            }
            KeyCode::Enter if focus == DotPane::Vars => self.open_edit_var(),
            _ => {}
        }
        Effect::None
    }

    fn pick_palette(&mut self, name: String) -> Effect {
        if self.session.workspace.palette_name() == Some(name.as_str()) {
            self.info(format!("{name} is already the active palette"));
            return Effect::None;
        }
        self.start(JobRequest::SetPalette(name))
    }

    fn cycle_palette(&mut self) -> Effect {
        let names = self.palette_names();
        if names.len() < 2 {
            self.info("there is no other palette to switch to");
            return Effect::None;
        }
        let current = self.session.workspace.palette_name();
        let next = match names.iter().position(|name| Some(name.as_str()) == current) {
            Some(index) => (index + 1) % names.len(),
            None => 0,
        };
        self.pick_palette(names[next].clone())
    }

    fn dotfiles_capture(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        if self.dotfiles.edited.is_empty() {
            self.info("no rendered files were edited in place");
            return;
        }
        let mut lines = Vec::new();
        for edit in &self.dotfiles.edited {
            lines.extend(capture_preview(edit));
            lines.push(String::new());
        }
        let capturable = self
            .dotfiles
            .edited
            .iter()
            .filter(|edit| edit.problem.is_none())
            .count();
        let request = (capturable > 0).then_some(JobRequest::Capture);
        self.modal = Some(Modal::Confirm(Confirm {
            title: if capturable > 0 {
                format!("Capture {capturable} edited file(s) into their templates?")
            } else {
                "Edits that can't be captured".into()
            },
            lines,
            scroll: 0,
            request,
        }));
    }

    fn open_edit_var(&mut self) {
        let names = self.var_names();
        let Some(name) = names.get(self.dotfiles.vars.cursor).cloned() else {
            self.info("this config declares no template variables");
            return;
        };
        let definition = self.session.workspace.config.vars.get(&name);
        let secret = definition.is_some_and(|definition| definition.is_secret());
        let describe = definition
            .and_then(|definition| definition.description())
            .unwrap_or("template variable");
        let field = if secret {
            Field::secret("value", &format!("{describe} (secret, shown as asterisks)"))
        } else {
            let current = self
                .session
                .workspace
                .host
                .vars
                .get(&name)
                .map(String::as_str)
                .or_else(|| definition.and_then(|definition| definition.default_value()))
                .unwrap_or("");
            Field::text("value", describe, current)
        };
        self.modal = Some(Modal::Form(Form::new(
            FormKind::EditVar(name.clone()),
            &format!("Set variable {name}"),
            vec![field],
        )));
    }

    // ----- sync tab -------------------------------------------------------

    fn sync_key(&mut self, key: KeyEvent) -> Effect {
        let len = self.sync.entries().len();
        let diff_len = self.sync.diff_len();
        let last = diff_len.saturating_sub(1);
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('d') if control => {
                self.sync.diff_scroll = (self.sync.diff_scroll + PAGE).min(last)
            }
            KeyCode::Char('u') if control => {
                self.sync.diff_scroll = self.sync.diff_scroll.saturating_sub(PAGE)
            }
            _ if control => {}
            KeyCode::Down | KeyCode::Char('j') => {
                self.sync.list.down(1, len);
                self.sync.diff_scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.sync.list.up(1);
                self.sync.diff_scroll = 0;
            }
            KeyCode::Char('J') | KeyCode::PageDown => {
                self.sync.diff_scroll = (self.sync.diff_scroll + PAGE).min(last)
            }
            KeyCode::Char('K') | KeyCode::PageUp => {
                self.sync.diff_scroll = self.sync.diff_scroll.saturating_sub(PAGE)
            }
            KeyCode::Home => self.sync.diff_scroll = 0,
            KeyCode::End => self.sync.diff_scroll = last,
            KeyCode::Char('R') => {
                self.sync.requested = true;
                self.want(JobRequest::LoadSync);
                self.info("checking the repos");
            }
            KeyCode::Char('s') => return self.start(JobRequest::Sync),
            KeyCode::Char('U') => self.confirm_update(),
            _ => {}
        }
        Effect::None
    }

    fn confirm_update(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let mut lines = vec![
            "Update syncs every repo (local changes are stashed and reapplied),".to_string(),
            "redeploys dotfiles, then installs whatever tool changes that brought.".to_string(),
        ];
        if let Some((incoming, _)) = &self.sync.data {
            lines.push(String::new());
            for repo in incoming {
                lines.push(format!(
                    "{}: {} incoming commit(s)",
                    repo.root,
                    repo.commits.len()
                ));
            }
        }
        self.modal = Some(Modal::Confirm(Confirm {
            title: "Update now?".into(),
            lines,
            scroll: 0,
            request: Some(JobRequest::Update),
        }));
    }

    // ----- settings tab ---------------------------------------------------

    fn settings_key(&mut self, key: KeyEvent) -> Effect {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => self.settings.scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings.scroll = self.settings.scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.settings.scroll += PAGE,
            KeyCode::PageUp => self.settings.scroll = self.settings.scroll.saturating_sub(PAGE),
            KeyCode::Home | KeyCode::Char('g') => self.settings.scroll = 0,
            KeyCode::Char('R') => {
                self.settings.requested = true;
                self.want(JobRequest::LoadSchedule);
                self.info("reading the schedule");
            }
            _ => {}
        }
        Effect::None
    }
}

/// One line per planned change, for the confirm dialog.
pub fn plan_lines(plan: &Plan) -> Vec<String> {
    let mut lines: Vec<String> = plan
        .changes()
        .map(|row| {
            let detail = match row.action {
                Action::Install => row.available.clone().unwrap_or_default(),
                _ => row.why.clone(),
            };
            format!(
                "{:<10} {:<20} {}",
                action_label(row.action),
                row.name,
                clean(&detail)
            )
        })
        .collect();
    let errors = plan.errors().count();
    if errors > 0 {
        lines.push(String::new());
        lines.push(format!(
            "{errors} tool(s) could not be looked up and are left alone."
        ));
    }
    lines
}

/// What would change when `edit` is captured, as unified-diff lines.
pub fn capture_preview(edit: &Edited) -> Vec<String> {
    let mut text = format!("{} was edited in place\n", edit.target.display());
    match (&edit.problem, &edit.captured) {
        (Some(problem), _) => text.push_str(&format!("  can't capture: {problem}\n")),
        (None, Some(captured)) => {
            let current = std::fs::read_to_string(&edit.template).unwrap_or_default();
            let diff = similar::TextDiff::from_lines(&current, captured);
            text.push_str(&format!("  would update {}:\n", edit.template.display()));
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
    text.lines().map(clean).collect()
}

/// Past-tense log lines for a deploy, plus the number of errors.
pub fn deploy_summary(items: &[Item]) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut errors = 0;
    let mut changed = 0;
    for item in items {
        let verb = match item.change {
            Change::Unchanged | Change::Skipped => continue,
            Change::Error => {
                errors += 1;
                "error"
            }
            Change::Linked => "linked",
            Change::Rerendered => "re-rendered",
            Change::BackedUp => "backed up and linked",
            Change::Pruned => "removed",
        };
        if item.change != Change::Error {
            changed += 1;
        }
        let detail = if item.detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", item.detail)
        };
        lines.push(format!("dotfile {verb}: {}{detail}", item.target.display()));
    }
    if changed == 0 && errors == 0 && !items.is_empty() {
        lines.push("dotfiles: nothing to change".into());
    }
    (lines, errors)
}

/// The verb shown next to a dotfile target in the dry-run list.
pub fn change_verb(change: deploy::Change) -> &'static str {
    match change {
        Change::Unchanged => "ok",
        Change::Linked => "link",
        Change::Rerendered => "re-render",
        Change::BackedUp => "back up",
        Change::Pruned => "remove",
        Change::Skipped => "skip",
        Change::Error => "error",
    }
}

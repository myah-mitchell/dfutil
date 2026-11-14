//! Application state, keys, and mouse handling. Nothing here touches the
//! terminal: [`App::handle_key`], [`App::handle_mouse`], and
//! [`App::on_event`] are plain state transitions, and the event loop in
//! `mod.rs` carries out the [`Effect`]s they return.

use super::input::{Field, FieldKind, Form, FormEvent, FormKind};
use super::job::{JobContext, JobEvent, JobOutput, JobRequest};
use super::mouse::{Hit, HitMap};
use super::state::*;
use crate::config::{PresetDef, ToolDef, ToolKind, is_valid_name};
use crate::deploy::{self, Change, Edited, Item};
use crate::gitops::SyncResult;
use crate::ops::{self, ApplyReport, Session};
use crate::plan::{Action, Plan, ToolRow};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

/// Lines kept in the log.
const LOG_CAP: usize = 1000;
/// Rows moved by Page Up and Page Down.
const PAGE: usize = 10;
/// Lines the scroll wheel moves a scrollable pane.
const WHEEL: usize = 3;

/// Field positions in the add-tool form.
pub const F_NAME: usize = 0;
pub const F_KIND: usize = 1;
pub const F_REPO: usize = 2;
pub const F_PIN: usize = 3;
pub const F_CRATE: usize = 4;
pub const F_ASSET: usize = 5;
pub const F_BINS: usize = 6;
pub const F_CATEGORY: usize = 7;
pub const F_DESCRIPTION: usize = 8;
pub const F_INSTALL_NOW: usize = 9;

/// Kinds offered by the add-tool form. `script` tools need install commands
/// the form has no room for, so they stay a hand edit of `dfutil.toml`.
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
    Help {
        scroll: usize,
    },
    /// The full log. `None` follows the newest line.
    Log {
        scroll: Option<usize>,
    },
    Confirm(Confirm),
    Form(Form),
}

#[derive(Clone, Debug)]
pub struct LogLine {
    pub text: String,
    pub error: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Success,
    Error,
}

/// The message in the job strip; cleared by the next key press or click.
#[derive(Clone, Debug)]
pub struct StatusMessage {
    pub text: String,
    pub tone: Tone,
}

#[derive(Debug)]
pub struct RunningJob {
    pub label: String,
    pub started: Instant,
    /// The newest log line, shown beside the spinner.
    pub last_line: Option<String>,
    /// Reload the session and refresh these views when the job ends.
    reload: bool,
    after: Vec<JobRequest>,
    /// Whether this was the version lookup, whose failure the Tools panel shows.
    is_plan: bool,
}

/// A key chip in the hint bar, or a button in the detail pane. Clicking one
/// presses its key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Button {
    pub keys: &'static str,
    pub code: KeyCode,
    pub label: String,
    pub primary: bool,
}

fn button(keys: &'static str, code: KeyCode, label: impl Into<String>) -> Button {
    Button {
        keys,
        code,
        label: label.into(),
        primary: false,
    }
}

fn primary(keys: &'static str, code: KeyCode, label: impl Into<String>) -> Button {
    Button {
        primary: true,
        ..button(keys, code, label)
    }
}

pub struct App {
    pub session: Session,
    pub focus: Panel,
    pub tools: ToolsState,
    pub presets: PresetsState,
    pub dotfiles: DotfilesState,
    pub repos: ReposState,
    pub modal: Option<Modal>,
    pub log: VecDeque<LogLine>,
    pub status: Option<StatusMessage>,
    pub job: Option<RunningJob>,
    pub quit_armed: bool,
    /// Animation frame for the running-job indicator.
    pub frame: usize,
    pub detail_scroll: usize,
    /// The last scroll position the detail pane can use, set by the renderer.
    pub detail_max: Cell<usize>,
    /// The last scroll position the open dialog can use, set by the renderer.
    pub modal_max: Cell<usize>,
    /// The selection the detail pane was showing, so a new one starts at the top.
    detail_for: String,
    pub hits: HitMap,
    /// Read-only refreshes waiting for the worker to be free.
    wanted: VecDeque<JobRequest>,
}

impl App {
    pub fn new(session: Session) -> Self {
        let mut app = Self {
            session,
            focus: Panel::Tools,
            tools: ToolsState::default(),
            presets: PresetsState::default(),
            dotfiles: DotfilesState::default(),
            repos: ReposState::default(),
            modal: None,
            log: VecDeque::new(),
            status: None,
            job: None,
            quit_armed: false,
            frame: 0,
            detail_scroll: 0,
            detail_max: Cell::new(0),
            modal_max: Cell::new(0),
            detail_for: String::new(),
            hits: HitMap::default(),
            wanted: VecDeque::new(),
        };
        app.want(JobRequest::LoadPlan);
        app.want(JobRequest::LoadDotfiles);
        app.want(JobRequest::LoadSchedule);
        app.want(JobRequest::LoadSync);
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

    fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.status = Some(StatusMessage {
            text: clean(&text.into()),
            tone,
        });
    }

    fn info(&mut self, text: impl Into<String>) {
        self.say(text, Tone::Info);
    }

    /// An error message in the job strip and the log.
    fn fail(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.log_error(&text);
        self.say(text.lines().next().unwrap_or(""), Tone::Error);
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
            after.push(JobRequest::LoadSync);
        }
        let label = request.label();
        self.log_line(format!("> {label}"));
        self.job = Some(RunningJob {
            label,
            started: Instant::now(),
            last_line: None,
            reload: request.mutates(),
            after,
            is_plan: matches!(request, JobRequest::LoadPlan),
        });
        Effect::Start(request)
    }

    /// Applies a message from the worker.
    pub fn on_event(&mut self, event: JobEvent) {
        match event {
            JobEvent::Log(line) => {
                if let Some(job) = &mut self.job {
                    job.last_line = Some(clean(&line));
                }
                self.log_line(line);
            }
            JobEvent::Done(result) => {
                let job = self.job.take();
                let label = job.as_ref().map(|job| job.label.clone());
                match result {
                    Ok(output) => self.apply_output(output, label.as_deref()),
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
        self.settle();
    }

    fn reload_session(&mut self) {
        match self.session.reload() {
            Ok(()) => self.clamp_lists(),
            Err(error) => self.fail(format!("reloading the config failed: {error:#}")),
        }
    }

    fn clamp_lists(&mut self) {
        let lens = (
            self.tool_items().len(),
            self.preset_names().len(),
            self.dot_rows().len(),
            self.repo_rows().len(),
        );
        self.tools.list.clamp(lens.0);
        self.presets.list.clamp(lens.1);
        self.dotfiles.list.clamp(lens.2);
        self.repos.list.clamp(lens.3);
    }

    fn apply_output(&mut self, output: JobOutput, label: Option<&str>) {
        match output {
            JobOutput::Plan(plan) => {
                let errors = plan.errors().count();
                if errors > 0 {
                    self.log_error(format!(
                        "{errors} tool(s) could not be looked up (see Needs attention)"
                    ));
                }
                self.set_plan(plan);
            }
            JobOutput::Dotfiles {
                items,
                edited,
                problems,
            } => {
                for problem in &problems {
                    self.log_error(problem);
                }
                self.dotfiles.items = Some(items);
                self.dotfiles.edited = edited;
                self.dotfiles.problems = problems;
                let len = self.dot_rows().len();
                self.dotfiles.list.clamp(len);
            }
            JobOutput::Sync { incoming, outgoing } => {
                self.repos.data = Some((incoming, outgoing));
                let len = self.repo_rows().len();
                self.repos.list.clamp(len);
            }
            JobOutput::Schedule(status) => self.repos.schedule = Some(status),
            JobOutput::Report(report) => self.show_report(&report, label.unwrap_or("job")),
            JobOutput::Deployed(items) => {
                let (lines, errors) = deploy_summary(&items);
                let changed = lines.len();
                for line in lines {
                    self.log_line(line);
                }
                if errors > 0 {
                    self.fail(format!(
                        "deploy finished with {errors} error(s); press L for the log"
                    ));
                } else {
                    self.say("✓ dotfiles deployed", Tone::Success);
                    if changed == 0 {
                        self.say("✓ dotfiles deployed, nothing changed", Tone::Success);
                    }
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
                    self.say(
                        format!("✓ captured {} edited file(s)", edited.len()),
                        Tone::Success,
                    );
                }
            }
            JobOutput::Message(text) => {
                self.log_line(&text);
                self.say(format!("✓ {text}"), Tone::Success);
            }
        }
    }

    fn show_report(&mut self, report: &ApplyReport, label: &str) {
        let mut done: BTreeMap<&'static str, usize> = BTreeMap::new();
        let mut failed = 0;
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
                *done.entry(past_tense(outcome.action)).or_default() += 1;
                self.log_line(line);
            } else {
                failed += 1;
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
        let counts: Vec<String> = done
            .iter()
            .map(|(verb, count)| format!("{count} {verb}"))
            .collect();
        let summary = if counts.is_empty() {
            String::new()
        } else {
            format!(": {}", counts.join(", "))
        };
        if report.failed() {
            let failures = failed + report.resolve_errors.len();
            self.fail(format!(
                "{label} finished with {} failure(s){summary}; press L for the log",
                failures.max(1)
            ));
        } else if report.changed() {
            self.say(format!("✓ {label}: done{summary}"), Tone::Success);
        } else {
            self.say(format!("✓ {label}: done, nothing changed"), Tone::Success);
        }
    }

    // ----- derived views ----------------------------------------------------

    /// Presets in effect on this host, including ones pulled in by `extends`.
    pub fn active_presets(&self) -> BTreeSet<String> {
        let workspace = &self.session.workspace;
        crate::selection::expand_presets(&workspace.host.presets, &workspace.config.presets)
            .into_iter()
            .collect()
    }

    pub fn tool_items(&self) -> Vec<ToolItem> {
        self.tools.items(
            &self.session.workspace.config.presets,
            &self.active_presets(),
        )
    }

    pub fn current_item(&self) -> Option<ToolItem> {
        self.tool_items().get(self.tools.list.cursor).cloned()
    }

    pub fn current_tool(&self) -> Option<&ToolRow> {
        match self.current_item()? {
            ToolItem::Tool(index) => self.tools.rows().get(index),
            ToolItem::Header { .. } => None,
        }
    }

    /// Marked tool names, or the highlighted one when nothing is marked.
    pub fn tool_targets(&self) -> Vec<String> {
        if self.tools.marked.is_empty() {
            self.current_tool()
                .map(|row| vec![row.name.clone()])
                .unwrap_or_default()
        } else {
            self.tools.marked.iter().cloned().collect()
        }
    }

    fn tool_anchor(&self) -> Option<ToolAnchor> {
        let item = self.current_item()?;
        Some(ToolsState::anchor(&item, self.tools.rows()))
    }

    /// Puts the cursor back on `anchor` after the list changed, or on the
    /// header of the group it disappeared into.
    fn restore_tool_anchor(&mut self, anchor: Option<ToolAnchor>) {
        let items = self.tool_items();
        if let Some(anchor) = anchor
            && let Some(index) = ToolsState::find(&items, self.tools.rows(), &anchor)
        {
            self.tools.list.cursor = index;
        }
        self.tools.list.clamp(items.len());
    }

    fn set_plan(&mut self, plan: Plan) {
        let anchor = self.tool_anchor();
        self.tools
            .marked
            .retain(|marked| plan.rows.iter().any(|row| &row.name == marked));
        self.tools.plan = Some(plan);
        self.tools.error = None;
        self.restore_tool_anchor(anchor);
    }

    pub fn preset_names(&self) -> Vec<String> {
        self.session
            .workspace
            .config
            .presets
            .keys()
            .cloned()
            .collect()
    }

    pub fn current_preset(&self) -> Option<String> {
        self.preset_names().get(self.presets.list.cursor).cloned()
    }

    pub fn palette_names(&self) -> Vec<String> {
        self.session.workspace.palettes.keys().cloned().collect()
    }

    pub fn var_names(&self) -> Vec<String> {
        self.session.workspace.config.vars.keys().cloned().collect()
    }

    pub fn dot_rows(&self) -> Vec<DotRow> {
        self.dotfiles.rows(
            !self.session.workspace.palettes.is_empty(),
            &self.var_names(),
        )
    }

    pub fn current_dot(&self) -> Option<DotRow> {
        self.dot_rows().get(self.dotfiles.list.cursor).cloned()
    }

    pub fn repo_rows(&self) -> Vec<RepoRow> {
        let mut rows: Vec<RepoRow> = (0..self.session.workspace.roots.len())
            .map(RepoRow::Root)
            .collect();
        rows.push(RepoRow::Schedule);
        rows
    }

    pub fn current_repo(&self) -> Option<RepoRow> {
        self.repo_rows().get(self.repos.list.cursor).cloned()
    }

    /// Identifies what the detail pane shows, so a new selection scrolls it
    /// back to the top.
    fn detail_key(&self) -> String {
        let which = match self.focus {
            Panel::Tools => format!("{:?}", self.tool_anchor()),
            Panel::Presets => format!("{:?}", self.current_preset()),
            Panel::Dotfiles => format!("{:?}", self.current_dot()),
            Panel::Repos => format!("{:?}", self.current_repo()),
        };
        format!("{:?}/{which}", self.focus)
    }

    fn settle(&mut self) {
        let key = self.detail_key();
        if key != self.detail_for {
            self.detail_for = key;
            self.detail_scroll = 0;
        }
    }

    // ----- keys -----------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        let effect = self.key(key);
        self.settle();
        effect
    }

    fn key(&mut self, key: KeyEvent) -> Effect {
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
            KeyCode::Char('d') if control => return self.scroll_detail(PAGE as isize),
            KeyCode::Char('u') if control => return self.scroll_detail(-(PAGE as isize)),
            _ if control => return Effect::None,
            KeyCode::Char('q') => return self.request_quit(armed),
            KeyCode::Esc => {
                if !self.tools.filter.is_empty() {
                    self.change_tools(|tools| tools.filter.clear());
                } else if !self.tools.marked.is_empty() {
                    self.tools.marked.clear();
                    self.info("marks cleared");
                } else {
                    return self.request_quit(armed);
                }
                return Effect::None;
            }
            KeyCode::Tab => self.focus = self.focus.next(),
            KeyCode::BackTab => self.focus = self.focus.prev(),
            KeyCode::Char(digit @ '1'..='4') => {
                self.focus = Panel::ALL[digit as usize - '1' as usize];
            }
            KeyCode::Char('?') => self.modal = Some(Modal::Help { scroll: 0 }),
            KeyCode::Char('L') => self.modal = Some(Modal::Log { scroll: None }),
            KeyCode::Char('J') => return self.scroll_detail(1),
            KeyCode::Char('K') => return self.scroll_detail(-1),
            KeyCode::Char('a') => self.apply_all(),
            KeyCode::Char('U') => self.confirm_update(),
            _ => {
                return match self.focus {
                    Panel::Tools => self.tools_key(key),
                    Panel::Presets => self.presets_key(key),
                    Panel::Dotfiles => self.dotfiles_key(key),
                    Panel::Repos => self.repos_key(key),
                };
            }
        }
        Effect::None
    }

    fn scroll_detail(&mut self, by: isize) -> Effect {
        let max = self.detail_max.get();
        self.detail_scroll = self.detail_scroll.saturating_add_signed(by).min(max);
        Effect::None
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

    fn modal_key(&mut self, modal: Modal, key: KeyEvent) -> Effect {
        match modal {
            Modal::Help { mut scroll } => {
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => scroll += 1,
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll += PAGE,
                    KeyCode::PageUp => scroll = scroll.saturating_sub(PAGE),
                    _ => return Effect::None,
                }
                let scroll = scroll.min(self.modal_max.get());
                self.modal = Some(Modal::Help { scroll });
                Effect::None
            }
            Modal::Log { scroll } => {
                let last = self.modal_max.get();
                let at = scroll.unwrap_or(last);
                let scroll = match key.code {
                    KeyCode::Down | KeyCode::Char('j') => Some(at + 1),
                    KeyCode::Up | KeyCode::Char('k') => Some(at.saturating_sub(1)),
                    KeyCode::PageDown => Some(at + PAGE),
                    KeyCode::PageUp => Some(at.saturating_sub(PAGE)),
                    KeyCode::Home | KeyCode::Char('g') => Some(0),
                    KeyCode::End | KeyCode::Char('G') => None,
                    _ => return Effect::None,
                };
                // Scrolling to the end follows new lines again.
                let scroll = scroll.filter(|line| *line < last);
                self.modal = Some(Modal::Log { scroll });
                Effect::None
            }
            Modal::Confirm(mut confirm) => {
                match key.code {
                    KeyCode::Char('y' | 'Y') if confirm.request.is_some() => {
                        if let Some(job) = &self.job {
                            // Keep the dialog so the choice isn't lost.
                            let label = job.label.clone();
                            self.info(format!(
                                "{label} is still running; confirm again when it finishes"
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
                confirm.scroll = confirm.scroll.min(self.modal_max.get());
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

    // ----- mouse ----------------------------------------------------------

    pub fn handle_mouse(&mut self, mouse: MouseEvent) -> Effect {
        let hit = self.hits.at(mouse.column, mouse.row);
        let effect = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.quit_armed = false;
                self.status = None;
                match hit {
                    Some(hit) => self.click(hit),
                    None => Effect::None,
                }
            }
            MouseEventKind::ScrollDown => self.wheel(hit, 1),
            MouseEventKind::ScrollUp => self.wheel(hit, -1),
            _ => Effect::None,
        };
        self.settle();
        effect
    }

    fn press(&mut self, code: KeyCode) -> Effect {
        self.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn click(&mut self, hit: Hit) -> Effect {
        if self.modal.is_some() {
            return self.modal_click(hit);
        }
        if self.tools.filtering && hit != Hit::Filter {
            self.tools.filtering = false;
        }
        match hit {
            Hit::Focus(panel) => self.focus = panel,
            Hit::ToolItem(index) => {
                self.focus = Panel::Tools;
                self.tools.list.cursor = index;
                if let Some(ToolItem::Header { key, open, .. }) = self.tool_items().get(index) {
                    self.set_open(key.clone(), !open);
                }
            }
            Hit::ToolMark(index) => {
                self.focus = Panel::Tools;
                self.tools.list.cursor = index;
                self.toggle_mark();
            }
            Hit::Grouping(grouping) => {
                self.focus = Panel::Tools;
                self.change_tools(|tools| tools.grouping = grouping);
            }
            Hit::Filter => {
                self.focus = Panel::Tools;
                self.tools.filtering = true;
            }
            Hit::Preset(index) => {
                self.focus = Panel::Presets;
                self.presets.list.cursor = index;
            }
            Hit::PresetToggle(index) => {
                self.focus = Panel::Presets;
                self.presets.list.cursor = index;
                self.toggle_preset();
            }
            Hit::DotRow(index) => {
                self.focus = Panel::Dotfiles;
                self.dotfiles.list.cursor = index;
            }
            Hit::RepoRow(index) => {
                self.focus = Panel::Repos;
                self.repos.list.cursor = index;
            }
            Hit::Key(code) => return self.press(code),
            Hit::Palette => return self.cycle_palette(),
            Hit::Detail | Hit::Backdrop | Hit::ModalBody | Hit::FormField(_) => {}
        }
        Effect::None
    }

    fn modal_click(&mut self, hit: Hit) -> Effect {
        match hit {
            Hit::Key(code) => self.press(code),
            Hit::Backdrop => {
                // A click outside closes a dialog, but never throws away a
                // half-filled form.
                if !matches!(self.modal, Some(Modal::Form(_))) {
                    self.modal = None;
                }
                Effect::None
            }
            Hit::FormField(index) => {
                if let Some(Modal::Form(form)) = &mut self.modal
                    && index < form.fields.len()
                {
                    let already = form.focus == index;
                    form.focus = index;
                    let field = &mut form.fields[index];
                    match &mut field.kind {
                        FieldKind::Choice { options, selected } if already => {
                            *selected = (*selected + 1) % options.len();
                        }
                        FieldKind::Check(value) => *value = !*value,
                        _ => {}
                    }
                }
                Effect::None
            }
            _ => Effect::None,
        }
    }

    fn wheel(&mut self, hit: Option<Hit>, direction: isize) -> Effect {
        if let Some(modal) = &mut self.modal {
            let by = WHEEL as isize * direction;
            let max = self.modal_max.get();
            match modal {
                Modal::Help { scroll } => *scroll = scroll.saturating_add_signed(by).min(max),
                Modal::Confirm(confirm) => {
                    confirm.scroll = confirm.scroll.saturating_add_signed(by).min(max);
                }
                Modal::Log { scroll } => {
                    let last = max;
                    let at = scroll.unwrap_or(last).saturating_add_signed(by);
                    *scroll = (at < last).then_some(at);
                }
                Modal::Form(_) => {}
            }
            return Effect::None;
        }
        let Some(hit) = hit else {
            return Effect::None;
        };
        let panel = match hit {
            Hit::Detail => return self.scroll_detail(WHEEL as isize * direction),
            Hit::Focus(panel) => panel,
            Hit::ToolItem(_) | Hit::ToolMark(_) | Hit::Grouping(_) | Hit::Filter => Panel::Tools,
            Hit::Preset(_) | Hit::PresetToggle(_) => Panel::Presets,
            Hit::DotRow(_) => Panel::Dotfiles,
            Hit::RepoRow(_) => Panel::Repos,
            _ => return Effect::None,
        };
        self.focus = panel;
        let code = if direction > 0 {
            KeyCode::Down
        } else {
            KeyCode::Up
        };
        self.press(code)
    }

    // ----- hints and buttons --------------------------------------------------

    /// The hint bar for the focused panel. Every chip is clickable.
    pub fn hints(&self) -> Vec<Button> {
        let mut hints = match self.focus {
            Panel::Tools if self.tools.filtering => vec![
                button("enter", KeyCode::Enter, "keep filter"),
                button("esc", KeyCode::Esc, "clear filter"),
            ],
            Panel::Tools => vec![
                button("space", KeyCode::Char(' '), "mark"),
                button("u", KeyCode::Char('u'), "update"),
                button("r", KeyCode::Char('r'), "reinstall"),
                button("b", KeyCode::Char('b'), "roll back"),
                button("x", KeyCode::Char('x'), "uninstall"),
                button("g", KeyCode::Char('g'), "group"),
                button("/", KeyCode::Char('/'), "filter"),
                button("n", KeyCode::Char('n'), "new tool"),
            ],
            Panel::Presets => vec![
                button("space", KeyCode::Char(' '), "turn on/off"),
                button("n", KeyCode::Char('n'), "new from marked"),
            ],
            Panel::Dotfiles => vec![
                button("d", KeyCode::Char('d'), "deploy"),
                button("c", KeyCode::Char('c'), "capture edits"),
                button("p", KeyCode::Char('p'), "palette"),
                button("e", KeyCode::Char('e'), "edit variable"),
            ],
            Panel::Repos => vec![
                button("s", KeyCode::Char('s'), "sync"),
                button("R", KeyCode::Char('R'), "check again"),
            ],
        };
        if !self.tools.filtering {
            let changes = self
                .tools
                .plan
                .as_ref()
                .map_or(0, |plan| plan.changes().count());
            hints.push(button(
                "a",
                KeyCode::Char('a'),
                format!("apply all ({changes})"),
            ));
            hints.push(button("U", KeyCode::Char('U'), "update everything"));
            hints.push(button("L", KeyCode::Char('L'), "log"));
            hints.push(button("?", KeyCode::Char('?'), "help"));
            hints.push(button("q", KeyCode::Char('q'), "quit"));
        }
        hints
    }

    /// Buttons under the detail pane for the focused selection.
    pub fn detail_actions(&self) -> Vec<Button> {
        match self.focus {
            Panel::Tools => {
                let Some(row) = self.current_tool() else {
                    return match self.current_item() {
                        Some(ToolItem::Header { open, .. }) => vec![primary(
                            "space",
                            KeyCode::Char(' '),
                            if open { "Close group" } else { "Open group" },
                        )],
                        None => Vec::new(),
                        Some(ToolItem::Tool(_)) => Vec::new(),
                    };
                };
                let available = row.available.clone().unwrap_or_default();
                let mut buttons = match Status::of(row) {
                    Status::Upgrade => {
                        vec![primary(
                            "u",
                            KeyCode::Char('u'),
                            format!("Update to {available}"),
                        )]
                    }
                    Status::Install => {
                        vec![primary(
                            "u",
                            KeyCode::Char('u'),
                            format!("Install {available}"),
                        )]
                    }
                    Status::Reinstall => vec![primary("u", KeyCode::Char('u'), "Reinstall now")],
                    Status::Off => {
                        vec![primary("u", KeyCode::Char('u'), "Install on this host")]
                    }
                    Status::Remove => vec![
                        primary("x", KeyCode::Char('x'), "Remove now"),
                        button("u", KeyCode::Char('u'), "Keep it on this host"),
                    ],
                    Status::Failed => {
                        vec![primary("R", KeyCode::Char('R'), "Look up again")]
                    }
                    Status::Held => vec![primary(
                        "r",
                        KeyCode::Char('r'),
                        format!("Reinstall {available}"),
                    )],
                    Status::Current => Vec::new(),
                };
                if row.installed.is_some() && Status::of(row) != Status::Held {
                    buttons.push(button("r", KeyCode::Char('r'), "Reinstall"));
                }
                if row.has_previous {
                    buttons.push(button("b", KeyCode::Char('b'), "Roll back"));
                }
                if (row.selected || row.installed.is_some()) && Status::of(row) != Status::Remove {
                    buttons.push(button("x", KeyCode::Char('x'), "Uninstall"));
                }
                buttons
            }
            Panel::Presets => match self.current_preset() {
                Some(name) => {
                    let on = self.session.workspace.host.presets.contains(&name);
                    vec![
                        primary(
                            "space",
                            KeyCode::Char(' '),
                            if on { "Turn off" } else { "Turn on" },
                        ),
                        button("n", KeyCode::Char('n'), "New preset from marked tools"),
                    ]
                }
                None => Vec::new(),
            },
            Panel::Dotfiles => match self.current_dot() {
                Some(DotRow::Edited(_)) => vec![
                    primary("c", KeyCode::Char('c'), "Capture into the template"),
                    button("d", KeyCode::Char('d'), "Deploy"),
                ],
                Some(DotRow::Palette) => vec![primary("p", KeyCode::Char('p'), "Next palette")],
                Some(DotRow::Var(_)) => vec![primary("e", KeyCode::Char('e'), "Edit value")],
                Some(_) => vec![primary("d", KeyCode::Char('d'), "Deploy")],
                None => Vec::new(),
            },
            Panel::Repos => match self.current_repo() {
                Some(RepoRow::Root(_)) => vec![
                    primary("s", KeyCode::Char('s'), "Sync now"),
                    button("U", KeyCode::Char('U'), "Sync and apply"),
                    button("R", KeyCode::Char('R'), "Check again"),
                ],
                Some(RepoRow::Schedule) => vec![button("R", KeyCode::Char('R'), "Check again")],
                None => Vec::new(),
            },
        }
    }

    // ----- tools panel ------------------------------------------------------

    /// Changes the tools view while keeping the cursor on the same item.
    fn change_tools(&mut self, change: impl FnOnce(&mut ToolsState)) {
        let anchor = self.tool_anchor();
        change(&mut self.tools);
        self.restore_tool_anchor(anchor);
    }

    fn set_open(&mut self, key: String, open: bool) {
        self.change_tools(|tools| {
            tools.open.insert(key, open);
        });
    }

    fn filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Enter => self.tools.filtering = false,
            KeyCode::Esc => {
                self.tools.filtering = false;
                self.change_tools(|tools| tools.filter.clear());
            }
            _ => {
                self.tools.filter.handle(key);
                self.tools.list.home();
                let len = self.tool_items().len();
                // Land on the first match rather than its group header.
                if len > 1 {
                    self.tools.list.cursor = 1;
                }
            }
        }
    }

    fn toggle_mark(&mut self) {
        if let Some(name) = self.current_tool().map(|row| row.name.clone())
            && !self.tools.marked.remove(&name)
        {
            self.tools.marked.insert(name);
        }
    }

    fn tools_key(&mut self, key: KeyEvent) -> Effect {
        let items = self.tool_items();
        let len = items.len();
        let current = items.get(self.tools.list.cursor).cloned();
        let list = &mut self.tools.list;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => list.down(1, len),
            KeyCode::Up | KeyCode::Char('k') => list.up(1),
            KeyCode::PageDown => list.down(PAGE, len),
            KeyCode::PageUp => list.up(PAGE),
            KeyCode::Home => list.home(),
            KeyCode::End | KeyCode::Char('G') => list.end(len),
            KeyCode::Char(' ') | KeyCode::Enter => match current {
                Some(ToolItem::Header { key, open, .. }) => self.set_open(key, !open),
                Some(ToolItem::Tool(_)) if key.code == KeyCode::Char(' ') => {
                    self.toggle_mark();
                    self.tools.list.down(1, len);
                }
                _ => {}
            },
            KeyCode::Left | KeyCode::Char('h') => {
                // Close the group the cursor is in, and land on its header.
                let header = items[..=self.tools.list.cursor.min(len.saturating_sub(1))]
                    .iter()
                    .rposition(|item| matches!(item, ToolItem::Header { .. }));
                if let Some(index) = header
                    && let ToolItem::Header { key, .. } = &items[index]
                {
                    self.tools.list.cursor = index;
                    self.set_open(key.clone(), false);
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(ToolItem::Header { key, .. }) = current {
                    self.set_open(key, true);
                }
            }
            KeyCode::Char('m') => {
                let names: Vec<String> = items
                    .iter()
                    .filter_map(|item| match item {
                        ToolItem::Tool(index) => Some(self.tools.rows()[*index].name.clone()),
                        ToolItem::Header { .. } => None,
                    })
                    .collect();
                if names.iter().all(|name| self.tools.marked.contains(name)) {
                    for name in &names {
                        self.tools.marked.remove(name);
                    }
                } else {
                    self.tools.marked.extend(names);
                }
            }
            KeyCode::Char('g') => {
                let next = self.tools.grouping.next();
                self.change_tools(|tools| tools.grouping = next);
                self.info(format!("tools grouped by {}", next.label()));
            }
            KeyCode::Char('/') => self.tools.filtering = true,
            KeyCode::Char('R') => {
                self.want(JobRequest::LoadPlan);
                self.info("looking up tool versions again");
            }
            KeyCode::Char('u') => self.tools_update(),
            KeyCode::Char('r') => self.tools_reinstall(),
            KeyCode::Char('x') => self.tools_uninstall(),
            KeyCode::Char('b') => self.tools_rollback(),
            KeyCode::Char('n') => self.open_add_tool(),
            _ => {}
        }
        Effect::None
    }

    fn apply_all(&mut self) {
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

    /// Installs, upgrades, or keeps the marked or highlighted tools.
    fn tools_update(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let mut names = Vec::new();
        let mut lines = Vec::new();
        let mut skipped = Vec::new();
        for name in self.tool_targets() {
            let Some(row) = self.tools.row(&name) else {
                continue;
            };
            let available = clean(row.available.as_deref().unwrap_or("newest"));
            let installed = clean(row.installed.as_deref().unwrap_or("-"));
            let (verb, detail) = match Status::of(row) {
                Status::Install => ("install", available),
                Status::Upgrade => ("upgrade", format!("{installed} -> {available}")),
                Status::Reinstall => ("reinstall", clean(&row.why)),
                Status::Off => ("add", format!("{available}, as a one-off on this host")),
                Status::Remove => ("keep", format!("{installed}, added to this host")),
                Status::Current | Status::Held | Status::Failed => {
                    skipped.push(name.clone());
                    continue;
                }
            };
            if !self.session.workspace.config.tools.contains_key(&name) {
                skipped.push(name.clone());
                continue;
            }
            lines.push(format!("{verb:<10} {name:<20} {detail}"));
            names.push(name);
        }
        if names.is_empty() {
            self.info(match skipped.as_slice() {
                [] => "nothing highlighted to update".to_string(),
                [one] => format!("{one} has nothing to update"),
                many => format!("none of the {} tools has anything to update", many.len()),
            });
            return;
        }
        if !skipped.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "Left alone (current, held, or failed): {}",
                skipped.join(", ")
            ));
        }
        self.modal = Some(Modal::Confirm(Confirm {
            title: format!("Update {} tool(s)?", names.len()),
            lines,
            scroll: 0,
            request: Some(JobRequest::Install(names)),
        }));
    }

    fn tools_reinstall(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let names: Vec<String> = self
            .tool_targets()
            .into_iter()
            .filter(|name| self.tools.row(name).is_some_and(|row| row.selected))
            .collect();
        if names.is_empty() {
            self.fail("reinstall works on selected tools; mark some, or highlight one");
            return;
        }
        let lines = names
            .iter()
            .map(
                |name| match self.tools.row(name).and_then(|row| row.installed.clone()) {
                    Some(version) => format!("reinstall  {name:<20} installed {version}"),
                    None => format!("install    {name:<20} not installed yet"),
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

    fn tools_uninstall(&mut self) {
        if self.refuse_if_busy() {
            return;
        }
        let names: Vec<String> = self
            .tool_targets()
            .into_iter()
            .filter(|name| {
                self.tools
                    .row(name)
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
                |name| match self.tools.row(name).and_then(|row| row.installed.clone()) {
                    Some(version) => format!("uninstall  {name:<20} installed {version}"),
                    None => format!("uninstall  {name:<20} deselect, not installed"),
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
        let Some(row) = self.current_tool() else {
            self.info("highlight a tool to roll back");
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
            lines: vec![
                format!("rollback   {name:<20} {installed} -> the version before its last install"),
                String::new(),
                "Updates skip the version you roll back from until a newer one is out.".into(),
            ],
            scroll: 0,
            request: Some(JobRequest::Rollback(name)),
        }));
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
                "left/right or click to change; auto picks github from a repo, cargo from a crate",
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
            Field::text(
                "category",
                "optional group for the tools list, such as search or git",
                "",
            ),
            Field::text("description", "one line", ""),
            Field::check(
                "install now",
                "space or click toggles; installs after adding",
                false,
            ),
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
            category: optional(F_CATEGORY),
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
                self.say(format!("✓ added {name}"), Tone::Success);
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
                    return Err("mark tools in the Tools panel first (space)".into());
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
                self.say(
                    format!("✓ created preset {name}; turn it on in the Presets panel"),
                    Tone::Success,
                );
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
                self.say(format!("✓ set {name}; press d to deploy"), Tone::Success);
                self.want(JobRequest::LoadDotfiles);
                Ok(Effect::None)
            }
        }
    }

    // ----- presets panel ----------------------------------------------------

    fn presets_key(&mut self, key: KeyEvent) -> Effect {
        let len = self.preset_names().len();
        let list = &mut self.presets.list;
        match key.code {
            KeyCode::Down | KeyCode::Right | KeyCode::Char('j' | 'l') => list.down(1, len),
            KeyCode::Up | KeyCode::Left | KeyCode::Char('k' | 'h') => list.up(1),
            KeyCode::Home => list.home(),
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
        let Some(name) = self.current_preset() else {
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
        let verb = if selected { "turned off" } else { "turned on" };
        self.log_line(format!("{verb} preset {name}"));
        self.say(
            format!(
                "✓ {verb} {name} in host.toml; the plan is being recomputed, then press a to apply"
            ),
            Tone::Success,
        );
    }

    fn open_create_preset(&mut self) {
        if self.tools.marked.is_empty() {
            self.fail("mark tools in the Tools panel first (space), then press n here");
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

    // ----- dotfiles panel ---------------------------------------------------

    fn dotfiles_key(&mut self, key: KeyEvent) -> Effect {
        let len = self.dot_rows().len();
        let current = self.current_dot();
        let list = &mut self.dotfiles.list;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => list.down(1, len),
            KeyCode::Up | KeyCode::Char('k') => list.up(1),
            KeyCode::PageDown => list.down(PAGE, len),
            KeyCode::PageUp => list.up(PAGE),
            KeyCode::Home => list.home(),
            KeyCode::End | KeyCode::Char('G') => list.end(len),
            KeyCode::Enter => match current {
                Some(DotRow::Edited(_)) => self.dotfiles_capture(),
                Some(DotRow::Palette) => return self.cycle_palette(),
                Some(DotRow::Var(name)) => self.open_edit_var(name),
                _ => {}
            },
            KeyCode::Char('R') => {
                self.want(JobRequest::LoadDotfiles);
                self.info("checking dotfiles again");
            }
            KeyCode::Char('d') => return self.start(JobRequest::Deploy),
            KeyCode::Char('c') => self.dotfiles_capture(),
            KeyCode::Char('p') => return self.cycle_palette(),
            KeyCode::Char('e') => match current {
                Some(DotRow::Var(name)) => self.open_edit_var(name),
                _ => match self.var_names().first().cloned() {
                    Some(name) => self.open_edit_var(name),
                    None => self.info("this config declares no template variables"),
                },
            },
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

    fn open_edit_var(&mut self, name: String) {
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

    // ----- repos panel ------------------------------------------------------

    fn repos_key(&mut self, key: KeyEvent) -> Effect {
        let len = self.repo_rows().len();
        let list = &mut self.repos.list;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => list.down(1, len),
            KeyCode::Up | KeyCode::Char('k') => list.up(1),
            KeyCode::Home => list.home(),
            KeyCode::End | KeyCode::Char('G') => list.end(len),
            KeyCode::Char('R') => {
                self.want(JobRequest::LoadSync);
                self.want(JobRequest::LoadSchedule);
                self.info("checking the repos and the schedule");
            }
            KeyCode::Char('s') => return self.start(JobRequest::Sync),
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
        if let Some((incoming, _)) = &self.repos.data {
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
            title: "Update everything now?".into(),
            lines,
            scroll: 0,
            request: Some(JobRequest::Update),
        }));
    }
}

fn past_tense(action: Action) -> &'static str {
    match action {
        Action::Install => "installed",
        Action::Upgrade => "upgraded",
        Action::Reinstall => "reinstalled",
        Action::Uninstall => "removed",
        Action::Keep | Action::Available | Action::Error => "kept",
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

/// What a deploy would do to a dotfile target, in a few words.
pub fn change_verb(change: deploy::Change) -> &'static str {
    match change {
        Change::Unchanged => "linked",
        Change::Linked => "will link",
        Change::Rerendered => "will re-render",
        Change::BackedUp => "real file in the way",
        Change::Pruned => "will remove",
        Change::Skipped => "skipped",
        Change::Error => "error",
    }
}

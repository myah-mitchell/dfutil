//! Tests drive [`App`] with key presses and mouse clicks and render it into a
//! `TestBackend`. Every fixture lives in a temp directory used as `$HOME`, and
//! tools are `script` kinds, so nothing touches the network or the real home.

use super::app::{App, Effect, Modal, Tone};
use super::job::{self, JobEvent, JobOutput, JobRequest};
use super::mouse::Hit;
use super::state::{DotRow, Grouping, Panel, RepoRow, ToolItem};
use super::ui;
use crate::gitops::{Incoming, Outgoing};
use crate::ops::{self, Session};
use crate::schedule;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::Color;

const REPO: &str = r#"
[settings]
palette = "mocha"

[vars]
editor = "nvim"
token = { value = "s3cretvalue", secret = true }

[packages]
zsh = {}

[tools.hello]
kind = "script"
category = "greet"
description = "says hello"
install = "mkdir -p \"$DFUTIL_TOOL_DIR\" && printf '#!/bin/sh\necho hi\n' > \"$DFUTIL_TOOL_DIR/hello\" && chmod +x \"$DFUTIL_TOOL_DIR/hello\""
version = "echo 1.0.0"
bins = ["hello=hello"]

[tools.extra]
kind = "script"
install = "true"
version = "echo 2.0.0"

[presets.base]
description = "the basics"
tools = ["hello"]
packages = ["zsh"]

[presets.more]
extends = ["base"]
tools = ["extra"]
"#;

const WIDTH: u16 = 140;
const HEIGHT: u16 = 44;

struct Fixture {
    home: tempfile::TempDir,
    app: App,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("dotfiles");
        let write = |relative: &str, text: &str| {
            let path = repo.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("dfutil.toml", REPO);
        write("zsh/.zshrc", "plain\n");
        write(
            "zsh/.config/zsh/theme.zsh.tmpl",
            "export EDITOR={{ editor }}\nplain line\n",
        );
        write("palettes/mocha.toml", "base = \"#111111\"\n");
        write("palettes/latte.toml", "base = \"#eeeeee\"\n");
        let host = home.path().join(".config/dfutil/host.toml");
        let quiet = |_: String| {};
        ops::init(
            &host,
            home.path(),
            "~/dotfiles",
            None,
            vec!["base".into()],
            None,
            &quiet,
        )
        .unwrap();
        let session = Session::load(&host, Some(home.path()), true).unwrap();
        Self {
            home,
            app: App::new(session),
        }
    }

    fn config_text(&self) -> String {
        std::fs::read_to_string(self.home.path().join("dotfiles/dfutil.toml")).unwrap()
    }

    fn host_text(&self) -> String {
        std::fs::read_to_string(self.home.path().join(".config/dfutil/host.toml")).unwrap()
    }

    /// Runs one job to completion on this thread, as the worker would.
    fn run(&mut self, request: JobRequest) {
        let context = self.app.context();
        let result =
            job::run(request, &context, &|_line: String| {}).map_err(|error| format!("{error:#}"));
        self.app.on_event(JobEvent::Done(result));
    }

    /// Runs every queued refresh, standing in for the event loop's `pump`.
    /// The schedule lookup is faked: it would read the real systemd units.
    fn settle(&mut self) {
        for _ in 0..20 {
            match self.app.pump() {
                Effect::Start(JobRequest::LoadSchedule) => {
                    self.app
                        .on_event(JobEvent::Done(Ok(JobOutput::Schedule(schedule::Status {
                            installed: false,
                            schedule: None,
                            next_run: None,
                            last_result: None,
                        }))));
                }
                Effect::Start(request) => self.run(request),
                _ => return,
            }
        }
        panic!("refreshes kept coming");
    }

    /// Re-reads the dotfiles preview, as the app does after a change.
    fn refresh_dotfiles(&mut self) {
        self.app.want(JobRequest::LoadDotfiles);
        self.settle();
    }

    fn press(&mut self, code: KeyCode) -> Effect {
        self.app.handle_key(KeyEvent::new_with_kind(
            code,
            KeyModifiers::NONE,
            KeyEventKind::Press,
        ))
    }

    fn ch(&mut self, character: char) -> Effect {
        self.press(KeyCode::Char(character))
    }

    fn type_text(&mut self, text: &str) {
        for character in text.chars() {
            self.ch(character);
        }
    }

    fn screen(&self) -> String {
        buffer_text(&draw(&self.app, WIDTH, HEIGHT))
    }

    fn status(&self) -> String {
        self.app
            .status
            .as_ref()
            .map(|status| status.text.clone())
            .unwrap_or_default()
    }

    fn mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) -> Effect {
        // Draw first, as the event loop does, so the hit map is current.
        draw(&self.app, WIDTH, HEIGHT);
        self.app.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    /// Left-clicks the first cell where `needle` appears on screen.
    fn click_text(&mut self, needle: &str) -> Effect {
        let (column, row) = position_of(&self.screen(), needle);
        self.mouse(MouseEventKind::Down(MouseButton::Left), column, row)
    }

    /// Left-clicks the region the last frame drew for `hit`.
    fn click_hit(&mut self, hit: Hit) -> Effect {
        draw(&self.app, WIDTH, HEIGHT);
        let rect = self
            .app
            .hits
            .rect_of(&hit)
            .unwrap_or_else(|| panic!("{hit:?} not drawn"));
        self.mouse(MouseEventKind::Down(MouseButton::Left), rect.x, rect.y)
    }

    fn wheel_at(&mut self, needle: &str, down: bool) -> Effect {
        let (column, row) = position_of(&self.screen(), needle);
        let kind = if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        };
        self.mouse(kind, column, row)
    }

    /// Opens every tool group, so each tool has a line.
    fn open_all_groups(&mut self) {
        for item in self.app.tool_items() {
            if let ToolItem::Header { key, .. } = item {
                self.app.tools.open.insert(key, true);
            }
        }
    }

    /// Moves the Tools cursor onto `name`, opening its group if needed.
    fn select_tool(&mut self, name: &str) {
        self.app.focus = Panel::Tools;
        self.open_all_groups();
        let rows = self.app.tools.rows().to_vec();
        let index = self
            .app
            .tool_items()
            .iter()
            .position(|item| matches!(item, ToolItem::Tool(row) if rows[*row].name == name))
            .unwrap_or_else(|| panic!("no tool line {name}"));
        self.app.tools.list.cursor = index;
    }

    /// The tool names that have a line in the list, in order.
    fn tool_lines(&self) -> Vec<String> {
        let rows = self.app.tools.rows();
        self.app
            .tool_items()
            .iter()
            .filter_map(|item| match item {
                ToolItem::Tool(index) => Some(rows[*index].name.clone()),
                ToolItem::Header { .. } => None,
            })
            .collect()
    }

    fn header_labels(&self) -> Vec<String> {
        self.app
            .tool_items()
            .iter()
            .filter_map(|item| match item {
                ToolItem::Header { label, .. } => Some(label.clone()),
                ToolItem::Tool(_) => None,
            })
            .collect()
    }

    /// Moves the Dotfiles cursor onto `row`.
    fn select_dot(&mut self, row: DotRow) {
        self.app.focus = Panel::Dotfiles;
        let index = self
            .app
            .dot_rows()
            .iter()
            .position(|candidate| *candidate == row)
            .unwrap_or_else(|| panic!("no dotfiles row {row:?}"));
        self.app.dotfiles.list.cursor = index;
    }
}

fn draw(app: &App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn buffer_text(buffer: &Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The column and row of the first place `needle` appears in `screen`.
fn position_of(screen: &str, needle: &str) -> (u16, u16) {
    let (row, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{screen}"));
    let column = line[..line.find(needle).unwrap()].chars().count();
    (column as u16, row as u16)
}

/// The foreground color of the first cell where `needle` appears on screen.
fn color_of(app: &App, needle: &str) -> Option<Color> {
    let buffer = draw(app, WIDTH, HEIGHT);
    let (column, row) = position_of(&buffer_text(&buffer), needle);
    buffer[(column, row)].style().fg
}

fn start_request(effect: Effect) -> JobRequest {
    match effect {
        Effect::Start(request) => request,
        other => panic!("expected a job to start, got {other:?}"),
    }
}

fn confirm_request(app: &App) -> Option<&JobRequest> {
    match &app.modal {
        Some(Modal::Confirm(confirm)) => confirm.request.as_ref(),
        _ => panic!("expected a confirm dialog"),
    }
}

// ----- rendering ----------------------------------------------------------

#[test]
fn every_panel_renders_with_a_detail_pane() {
    let mut fixture = Fixture::new();
    fixture.settle();

    let screen = fixture.screen();
    for text in [
        " dfutil ",
        "base",
        "palette mocha",
        "1 Tools",
        "2 Presets",
        "3 Dotfiles",
        "4 Repos",
        "Needs attention",
        "Not selected",
        "? help",
    ] {
        assert!(screen.contains(text), "{text:?} missing:\n{screen}");
    }
    fixture.select_tool("hello");
    let screen = fixture.screen();
    assert!(screen.contains("says hello"), "{screen}");
    assert!(screen.contains("greet"), "category");
    assert!(screen.contains("Install on this host") || screen.contains("Install"));

    fixture.ch('2');
    let screen = fixture.screen();
    assert!(screen.contains("the basics"), "{screen}");

    fixture.ch('3');
    let screen = fixture.screen();
    assert!(screen.contains("~/.zshrc"), "{screen}");
    assert!(screen.contains("will link"));
    assert!(screen.contains("palette  mocha  of 2"), "{screen}");
    assert!(screen.contains("editor = nvim"), "{screen}");

    fixture.ch('4');
    fixture.ch('G');
    let screen = fixture.screen();
    assert!(screen.contains("Schedule and host"), "{screen}");
    assert!(screen.contains("valid: 2 tools"), "{screen}");
    assert!(screen.contains("No scheduled update"));
}

#[test]
fn small_terminals_do_not_panic() {
    let mut fixture = Fixture::new();
    fixture.settle();
    for panel in Panel::ALL {
        fixture.app.focus = panel;
        for (width, height) in [
            (1, 1),
            (10, 3),
            (20, 5),
            (30, 8),
            (60, 12),
            (89, 24),
            (90, 24),
            (200, 60),
        ] {
            draw(&fixture.app, width, height);
        }
    }
    assert!(buffer_text(&draw(&fixture.app, 20, 5)).contains("dfutil"));
    fixture.ch('1');
    for modal in ['?', 'L'] {
        fixture.ch(modal);
        draw(&fixture.app, 30, 8);
        draw(&fixture.app, 31, 9);
        fixture.press(KeyCode::Esc);
    }
    fixture.ch('n');
    assert!(matches!(fixture.app.modal, Some(Modal::Form(_))));
    draw(&fixture.app, 30, 8);
}

#[test]
fn narrow_terminals_put_the_detail_pane_under_the_panels() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.select_tool("hello");
    let wide = buffer_text(&draw(&fixture.app, 120, 40));
    let narrow = buffer_text(&draw(&fixture.app, 80, 40));
    let (_, tools_wide) = position_of(&wide, "1 Tools");
    let (_, detail_wide) = position_of(&wide, "says hello");
    let (_, repos_narrow) = position_of(&narrow, "4 Repos");
    let (_, detail_narrow) = position_of(&narrow, "says hello");
    assert!(detail_wide - tools_wide < 8, "side by side:\n{wide}");
    assert!(detail_narrow > repos_narrow, "stacked:\n{narrow}");
}

#[test]
fn tool_lines_show_a_status_glyph() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.open_all_groups();
    let screen = fixture.screen();
    assert!(screen.contains("[ ] + hello"), "{screen}");
    assert!(screen.contains("[ ] ○ extra"), "{screen}");
}

// ----- grouping, groups, and filtering ----------------------------------

#[test]
fn g_switches_between_status_preset_and_category_groups() {
    let mut fixture = Fixture::new();
    fixture.settle();
    assert_eq!(fixture.app.tools.grouping, Grouping::Status);
    assert_eq!(fixture.header_labels(), ["Needs attention", "Not selected"]);

    fixture.ch('g');
    assert_eq!(fixture.app.tools.grouping, Grouping::Preset);
    let labels = fixture.header_labels();
    assert!(labels.iter().any(|label| label == "base"), "{labels:?}");
    assert!(labels.iter().any(|label| label == "more"), "{labels:?}");
    assert!(fixture.status().contains("grouped by preset"));

    fixture.ch('g');
    assert_eq!(fixture.app.tools.grouping, Grouping::Category);
    let labels = fixture.header_labels();
    assert!(labels.iter().any(|label| label == "greet"), "{labels:?}");
    assert_eq!(labels.len(), 2, "{labels:?}");

    fixture.ch('g');
    assert_eq!(fixture.app.tools.grouping, Grouping::Status);
}

#[test]
fn groups_open_and_close_from_the_keyboard() {
    let mut fixture = Fixture::new();
    fixture.settle();
    // What needs attention starts open; the rest starts closed.
    assert_eq!(fixture.tool_lines(), ["hello"]);
    let header = fixture
        .app
        .tool_items()
        .iter()
        .position(|item| matches!(item, ToolItem::Header { label, .. } if label == "Not selected"))
        .unwrap();
    fixture.app.tools.list.cursor = header;
    fixture.ch('l');
    assert_eq!(fixture.tool_lines(), ["hello", "extra"]);
    fixture.ch('h');
    assert_eq!(fixture.tool_lines(), ["hello"]);
    fixture.press(KeyCode::Enter);
    assert_eq!(fixture.tool_lines(), ["hello", "extra"]);
    fixture.ch(' ');
    assert_eq!(fixture.tool_lines(), ["hello"]);
}

#[test]
fn filter_narrows_the_list_and_escape_clears_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('/');
    fixture.type_text("xtr");
    // A filter shows matches even in closed groups.
    assert_eq!(fixture.tool_lines(), ["extra"]);
    assert!(fixture.screen().contains("/xtr"));
    // Typing `q` while filtering edits the filter instead of quitting.
    assert!(matches!(fixture.ch('q'), Effect::None));
    assert!(fixture.tool_lines().is_empty());
    fixture.press(KeyCode::Backspace);
    fixture.press(KeyCode::Enter);
    assert!(!fixture.app.tools.filtering);
    assert_eq!(fixture.tool_lines(), ["extra"]);
    assert!(matches!(fixture.press(KeyCode::Esc), Effect::None));
    assert!(fixture.app.tools.filter.text().is_empty());
    assert_eq!(fixture.tool_lines(), ["hello"]);
}

#[test]
fn space_marks_tools_and_m_marks_all_shown() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.select_tool("hello");
    fixture.ch(' ');
    assert!(fixture.app.tools.marked.contains("hello"));
    assert!(fixture.screen().contains("[x] + hello"));
    fixture.ch('m');
    fixture.ch('m');
    let marked = fixture.app.tools.marked.len();
    assert!(marked == 0 || marked == 2, "m toggles all shown: {marked}");
    if marked == 0 {
        fixture.ch('m');
    }
    assert_eq!(fixture.app.tools.marked.len(), 2);
    // Esc clears marks before it would quit.
    assert!(matches!(fixture.press(KeyCode::Esc), Effect::None));
    assert!(fixture.app.tools.marked.is_empty());
    assert!(fixture.status().contains("marks cleared"));
    assert!(matches!(fixture.press(KeyCode::Esc), Effect::Quit));
}

// ----- confirm dialogs and jobs -------------------------------------------

#[test]
fn apply_asks_first_and_then_starts_the_confirmed_plan() {
    let mut fixture = Fixture::new();
    fixture.settle();

    assert!(matches!(fixture.ch('a'), Effect::None));
    let screen = fixture.screen();
    assert!(screen.contains("Apply 1 change(s)?"), "{screen}");
    assert!(screen.contains("y Confirm") && screen.contains("n Cancel"));

    // Declining starts nothing.
    assert!(matches!(fixture.ch('n'), Effect::None));
    assert!(fixture.app.modal.is_none() && fixture.app.job.is_none());

    fixture.ch('a');
    match start_request(fixture.ch('y')) {
        JobRequest::Apply(plan) => {
            let names: Vec<&str> = plan.changes().map(|row| row.name.as_str()).collect();
            assert_eq!(names, ["hello"]);
        }
        other => panic!("wrong request: {other:?}"),
    }
    assert!(fixture.app.modal.is_none());
    assert!(fixture.app.job.is_some());
}

#[test]
fn a_second_job_is_refused_while_one_runs() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    assert!(matches!(fixture.ch('d'), Effect::Start(JobRequest::Deploy)));
    assert!(fixture.app.job.is_some());

    assert!(matches!(fixture.ch('d'), Effect::None));
    let status = fixture.app.status.clone().unwrap();
    assert!(
        status.tone == Tone::Error && status.text.contains("busy"),
        "{status:?}"
    );
    assert!(
        fixture
            .app
            .log
            .iter()
            .any(|line| line.error && line.text.contains("busy"))
    );

    // Queued refreshes wait for the worker.
    assert!(matches!(fixture.app.pump(), Effect::None));
    fixture.run(JobRequest::Deploy);
    assert!(fixture.app.job.is_none());
    assert!(matches!(fixture.app.pump(), Effect::Start(_)));
}

#[test]
fn quitting_needs_two_presses_while_a_job_runs() {
    let mut fixture = Fixture::new();
    assert!(
        matches!(fixture.ch('q'), Effect::Quit),
        "idle quits at once"
    );

    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    start_request(fixture.ch('d'));
    assert!(matches!(fixture.ch('q'), Effect::None));
    assert!(fixture.status().contains("press q again"));
    assert!(matches!(fixture.ch('q'), Effect::Quit));
}

#[test]
fn control_c_quits_like_q() {
    let mut fixture = Fixture::new();
    let effect = fixture
        .app
        .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(matches!(effect, Effect::Quit));
}

#[test]
fn update_reinstall_uninstall_and_rollback_build_the_right_requests() {
    let mut fixture = Fixture::new();
    fixture.settle();

    fixture.select_tool("hello");
    fixture.ch('u');
    assert!(fixture.screen().contains("Update 1 tool(s)?"));
    match confirm_request(&fixture.app) {
        Some(JobRequest::Install(names)) => assert_eq!(names, &["hello"]),
        other => panic!("{other:?}"),
    }
    fixture.ch('n');

    // An unselected tool is added as a one-off.
    fixture.select_tool("extra");
    fixture.ch('u');
    match confirm_request(&fixture.app) {
        Some(JobRequest::Install(names)) => assert_eq!(names, &["extra"]),
        other => panic!("{other:?}"),
    }
    fixture.ch('n');

    // Reinstall works on selected tools only.
    fixture.select_tool("extra");
    fixture.ch('r');
    assert!(fixture.app.modal.is_none());
    assert!(
        fixture
            .status()
            .contains("reinstall works on selected tools")
    );
    fixture.select_tool("hello");
    fixture.ch('r');
    match confirm_request(&fixture.app) {
        Some(JobRequest::Reinstall(names)) => assert_eq!(names, &["hello"]),
        other => panic!("{other:?}"),
    }
    assert!(fixture.screen().contains("Reinstall 1 tool(s)?"));
    fixture.ch('n');

    // Marked tools win over the highlighted one.
    fixture.select_tool("hello");
    fixture.ch(' ');
    fixture.select_tool("extra");
    fixture.ch('x');
    match confirm_request(&fixture.app) {
        Some(JobRequest::Uninstall(names)) => assert_eq!(names, &["hello"]),
        other => panic!("{other:?}"),
    }
    assert!(fixture.screen().contains("Uninstall 1 tool(s)?"));
    assert!(fixture.screen().contains("deselect, not installed"));
    fixture.ch('n');

    // Rollback needs a kept previous version.
    fixture.press(KeyCode::Esc);
    fixture.select_tool("hello");
    fixture.ch('b');
    assert!(fixture.app.modal.is_none());
    assert!(fixture.status().contains("no previous version"));
}

#[test]
fn applying_installs_a_tool_and_refreshes_the_views() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('a');
    let request = start_request(fixture.ch('y'));
    fixture.run(request);
    assert!(fixture.app.job.is_none());
    assert!(fixture.home.path().join(".local/bin/hello").exists());
    let status = fixture.app.status.clone().unwrap();
    assert_eq!(status.tone, Tone::Success, "{status:?}");
    assert!(
        fixture
            .app
            .log
            .iter()
            .any(|line| line.text.contains("hello: install ok"))
    );

    fixture.settle();
    fixture.select_tool("hello");
    let screen = fixture.screen();
    assert!(screen.contains("1.0.0"), "{screen}");
    assert!(screen.contains("● hello"), "{screen}");
}

#[test]
fn a_failed_job_is_reported_and_unblocks_the_ui() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    start_request(fixture.ch('d'));
    fixture
        .app
        .on_event(JobEvent::Done(Err("disk on fire".into())));
    assert!(fixture.app.job.is_none());
    let status = fixture.app.status.clone().unwrap();
    assert!(status.tone == Tone::Error && status.text.contains("disk on fire"));
    assert!(fixture.screen().contains("disk on fire"));
    assert_eq!(color_of(&fixture.app, "disk on fire"), Some(Color::Red));
    assert!(matches!(fixture.app.pump(), Effect::Start(_)));
}

#[test]
fn worker_threads_deliver_logs_and_results() {
    let fixture = Fixture::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    job::spawn(JobRequest::LoadPlan, fixture.app.context(), sender);
    loop {
        match receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the worker answers")
        {
            JobEvent::Log(_) => {}
            JobEvent::Done(result) => {
                let Ok(JobOutput::Plan(plan)) = result else {
                    panic!("expected a plan");
                };
                assert_eq!(plan.rows.len(), 2);
                break;
            }
        }
    }
}

#[test]
fn a_job_that_cannot_load_its_session_reports_an_error() {
    let fixture = Fixture::new();
    let (sender, receiver) = std::sync::mpsc::channel();
    let mut context = fixture.app.context();
    context.host_path = fixture.home.path().join("missing.toml");
    job::spawn(JobRequest::Deploy, context, sender);
    let JobEvent::Done(result) = receiver
        .recv_timeout(std::time::Duration::from_secs(30))
        .unwrap()
    else {
        panic!("expected the result");
    };
    assert!(result.unwrap_err().contains("missing.toml"));
}

// ----- add-tool form ---------------------------------------------------------

fn tab_forward(fixture: &mut Fixture, count: usize) {
    for _ in 0..count {
        fixture.press(KeyCode::Tab);
    }
}

fn open_add_form(fixture: &mut Fixture) {
    fixture.ch('1');
    fixture.ch('n');
    assert!(matches!(fixture.app.modal, Some(Modal::Form(_))));
}

fn form_error(fixture: &Fixture) -> String {
    match &fixture.app.modal {
        Some(Modal::Form(form)) => form.error.clone().expect("the form shows an error"),
        _ => panic!("the form closed"),
    }
}

#[test]
fn add_tool_form_rejects_bad_input_and_writes_nothing() {
    let mut fixture = Fixture::new();
    fixture.settle();
    let before = fixture.config_text();
    open_add_form(&mut fixture);
    let screen = fixture.screen();
    assert!(
        screen.contains("enter Save") && screen.contains("esc Cancel"),
        "{screen}"
    );

    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).starts_with("name:"));
    fixture.type_text("hello");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("already exists"));
    for _ in 0..5 {
        fixture.press(KeyCode::Backspace);
    }
    fixture.type_text("fd");
    fixture.press(KeyCode::Enter);
    assert_eq!(
        form_error(&fixture),
        "give a repo or a crate, or pick a kind"
    );

    // A click outside keeps the form; Esc leaves the config untouched.
    fixture.mouse(MouseEventKind::Down(MouseButton::Left), 0, 0);
    assert!(matches!(fixture.app.modal, Some(Modal::Form(_))));
    fixture.press(KeyCode::Esc);
    assert!(fixture.app.modal.is_none());
    assert_eq!(fixture.config_text(), before);
}

#[test]
fn add_tool_form_writes_the_definition_with_a_category() {
    let mut fixture = Fixture::new();
    fixture.settle();
    open_add_form(&mut fixture);
    let labels: Vec<String> = match &fixture.app.modal {
        Some(Modal::Form(form)) => form.fields.iter().map(|f| f.label.clone()).collect(),
        _ => unreachable!(),
    };
    let at = |name: &str| {
        labels
            .iter()
            .position(|label| label.starts_with(name))
            .unwrap_or_else(|| panic!("no field {name} in {labels:?}"))
    };
    let (repo, asset, category, install) = (at("repo"), at("asset"), at("category"), at("install"));

    fixture.type_text("fd");
    tab_forward(&mut fixture, repo);
    fixture.type_text("sharkdp/fd");
    tab_forward(&mut fixture, asset - repo);
    fixture.type_text("fd-*-linux.tar.gz");
    tab_forward(&mut fixture, category - asset);
    fixture.type_text("search");
    tab_forward(&mut fixture, install - category);
    fixture.ch(' ');
    let effect = fixture.press(KeyCode::Enter);

    assert!(fixture.app.modal.is_none(), "{:?}", fixture.app.modal);
    match start_request(effect) {
        JobRequest::Install(names) => assert_eq!(names, ["fd"]),
        other => panic!("{other:?}"),
    }
    let tool = &fixture.app.session.workspace.config.tools["fd"];
    assert_eq!(tool.repo.as_deref(), Some("sharkdp/fd"));
    assert_eq!(tool.category.as_deref(), Some("search"));
    let text = fixture.config_text();
    assert!(text.contains("[tools.fd]") && text.contains("category = \"search\""));
    assert!(text.contains("[tools.hello]"));
}

// ----- presets ----------------------------------------------------------------

#[test]
fn space_toggles_a_preset_on_this_host() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('2');
    assert_eq!(fixture.app.current_preset().as_deref(), Some("base"));
    fixture.ch(' ');
    assert!(fixture.app.session.workspace.host.presets.is_empty());
    assert!(!fixture.host_text().contains("\"base\""));

    fixture.ch('l');
    assert_eq!(fixture.app.current_preset().as_deref(), Some("more"));
    fixture.ch(' ');
    assert_eq!(fixture.app.session.workspace.host.presets, ["more"]);
    assert_eq!(fixture.app.status.as_ref().unwrap().tone, Tone::Success);
    assert!(fixture.screen().contains("Extends"));
}

#[test]
fn n_creates_a_preset_from_the_marked_tools() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('2');
    fixture.ch('n');
    assert!(fixture.app.modal.is_none(), "no marked tools yet");
    assert!(fixture.status().contains("mark tools"));

    fixture.select_tool("hello");
    fixture.ch(' ');
    fixture.select_tool("extra");
    fixture.ch(' ');
    fixture.ch('2');
    fixture.ch('n');
    fixture.type_text("mine");
    fixture.press(KeyCode::Enter);
    assert!(fixture.app.modal.is_none(), "{:?}", fixture.app.modal);
    let preset = &fixture.app.session.workspace.config.presets["mine"];
    assert_eq!(preset.tools, ["extra", "hello"]);
    assert!(fixture.config_text().contains("[presets.mine]"));
}

// ----- dotfiles -----------------------------------------------------------------

#[test]
fn p_and_the_palette_row_switch_to_the_next_palette() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    match start_request(fixture.ch('p')) {
        JobRequest::SetPalette(name) => assert_eq!(name, "latte"),
        other => panic!("{other:?}"),
    }
    fixture.run(JobRequest::SetPalette("latte".into()));
    assert!(fixture.host_text().contains("latte"));
    fixture.settle();
    assert!(fixture.screen().contains("palette latte"));

    fixture.select_dot(DotRow::Palette);
    match start_request(fixture.press(KeyCode::Enter)) {
        JobRequest::SetPalette(name) => assert_eq!(name, "mocha"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn secret_variables_stay_hidden_everywhere() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.select_dot(DotRow::Var("token".into()));
    let screen = fixture.screen();
    assert!(!screen.contains("s3cretvalue"), "{screen}");

    fixture.press(KeyCode::Enter);
    assert!(matches!(fixture.app.modal, Some(Modal::Form(_))));
    fixture.type_text("hunter2hunter2");
    assert!(!fixture.screen().contains("hunter2"));
    fixture.press(KeyCode::Enter);
    assert!(fixture.app.modal.is_none(), "{:?}", fixture.app.modal);
    assert!(fixture.host_text().contains("hunter2hunter2"));
    let screen = fixture.screen();
    assert!(!screen.contains("hunter2") && !screen.contains("s3cretvalue"));
    assert!(
        fixture
            .app
            .log
            .iter()
            .all(|line| !line.text.contains("hunter2"))
    );
}

#[test]
fn editing_a_plain_variable_saves_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    fixture.ch('e');
    assert!(matches!(fixture.app.modal, Some(Modal::Form(_))));
    fixture.type_text("x");
    fixture.press(KeyCode::Enter);
    assert_eq!(fixture.app.session.workspace.host.vars["editor"], "nvimx");
    assert!(fixture.status().contains("press d"));
}

#[test]
fn deploying_and_capturing_walk_through_a_confirm_dialog() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    fixture.ch('c');
    assert!(fixture.status().contains("no rendered files"));

    fixture.run(JobRequest::Deploy);
    let rendered = fixture.home.path().join(".config/zsh/theme.zsh");
    assert_eq!(
        std::fs::read_to_string(&rendered).unwrap(),
        "export EDITOR=nvim\nplain line\n"
    );
    std::fs::write(&rendered, "export EDITOR=nvim\nplain line\nadded by hand\n").unwrap();
    fixture.refresh_dotfiles();

    fixture.select_dot(DotRow::Edited(0));
    let screen = fixture.screen();
    assert!(screen.contains("+added by hand"), "{screen}");
    assert_eq!(color_of(&fixture.app, "+added by hand"), Some(Color::Green));

    fixture.press(KeyCode::Enter);
    assert!(matches!(
        confirm_request(&fixture.app),
        Some(JobRequest::Capture)
    ));
    let request = start_request(fixture.ch('y'));
    fixture.run(request);
    let template = std::fs::read_to_string(
        fixture
            .home
            .path()
            .join("dotfiles/zsh/.config/zsh/theme.zsh.tmpl"),
    )
    .unwrap();
    assert_eq!(
        template,
        "export EDITOR={{ editor }}\nplain line\nadded by hand\n"
    );
    fixture.refresh_dotfiles();
    assert!(fixture.app.dotfiles.edited.is_empty());
}

#[test]
fn deployed_files_fold_into_one_row() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.run(JobRequest::Deploy);
    fixture.refresh_dotfiles();
    fixture.ch('3');
    let screen = fixture.screen();
    assert!(
        screen.contains("2 files deployed and current")
            || screen.contains("2 file(s) deployed and current"),
        "{screen}"
    );
    assert!(
        !fixture
            .app
            .dot_rows()
            .iter()
            .any(|row| matches!(row, DotRow::Change(_)))
    );
}

// ----- repos and schedule -----------------------------------------------------------

fn sync_data() -> JobOutput {
    let mut diff = String::from("diff --git a/f b/f\n@@ -1 +1 @@\n");
    for n in 0..40 {
        diff.push_str(&format!("+added line {n}\n"));
    }
    diff.push_str("-removed line\n");
    JobOutput::Sync {
        incoming: vec![Incoming {
            root: "main".into(),
            upstream: Some("origin/main".into()),
            commits: vec!["abc1234 fix the thing".into()],
            diff,
            ..Incoming::default()
        }],
        outgoing: vec![Outgoing {
            root: "main".into(),
            status: vec![" M zsh/.zshrc".into()],
            commits: vec!["def5678 local work".into()],
            diff: "+local edit\x1b[31m\n".into(),
            ..Outgoing::default()
        }],
    }
}

#[test]
fn the_repos_panel_counts_changes_and_the_detail_shows_the_diffs() {
    let mut fixture = Fixture::new();
    fixture.ch('4');
    fixture.app.on_event(JobEvent::Done(Ok(sync_data())));

    let screen = fixture.screen();
    assert!(screen.contains("↓1 ↑1"), "{screen}");
    assert!(screen.contains("abc1234 fix the thing"));
    assert!(screen.contains("def5678 local work"));
    assert_eq!(color_of(&fixture.app, "+added line 3"), Some(Color::Green));

    let control = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
    for _ in 0..10 {
        fixture.app.handle_key(control);
    }
    assert!(fixture.app.detail_scroll > 0);
    assert!(fixture.app.detail_scroll <= fixture.app.detail_max.get());
    assert_eq!(color_of(&fixture.app, "-removed line"), Some(Color::Red));
    let screen = fixture.screen();
    assert!(!screen.contains('\x1b'));
}

#[test]
fn sync_starts_a_job_and_update_asks_first() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('4');
    assert!(matches!(fixture.ch('U'), Effect::None));
    assert!(fixture.screen().contains("Update everything now?"));
    assert!(matches!(
        confirm_request(&fixture.app),
        Some(JobRequest::Update)
    ));
    fixture.ch('n');
    assert!(fixture.app.job.is_none());
    assert!(matches!(fixture.ch('s'), Effect::Start(JobRequest::Sync)));
}

#[test]
fn the_schedule_row_shows_the_scheduled_update() {
    let mut fixture = Fixture::new();
    fixture.ch('4');
    fixture.ch('G');
    assert!(matches!(
        fixture.app.current_repo(),
        Some(RepoRow::Schedule)
    ));
    fixture
        .app
        .on_event(JobEvent::Done(Ok(JobOutput::Schedule(schedule::Status {
            installed: true,
            schedule: Some("weekly".into()),
            next_run: Some("Mon 04:17".into()),
            last_result: Some("success".into()),
        }))));
    let screen = fixture.screen();
    assert!(screen.contains("weekly"), "{screen}");
    assert!(screen.contains("Mon 04:17"));
    assert!(screen.contains("success"));
}

// ----- help, log, focus ----------------------------------------------------------

#[test]
fn help_scrolls_within_its_bounds_and_any_other_key_closes_it() {
    let mut fixture = Fixture::new();
    fixture.ch('?');
    assert!(fixture.screen().contains("Keys and mouse"));
    draw(&fixture.app, 80, 20);
    for _ in 0..100 {
        fixture.ch('j');
        draw(&fixture.app, 80, 20);
    }
    let Some(Modal::Help { scroll }) = fixture.app.modal else {
        panic!("help closed");
    };
    assert!(scroll > 0 && scroll == fixture.app.modal_max.get());
    fixture.ch('x');
    assert!(fixture.app.modal.is_none());
}

#[test]
fn the_log_opens_on_the_newest_lines() {
    let mut fixture = Fixture::new();
    for n in 0..100 {
        fixture.app.on_event(JobEvent::Log(format!("line {n}")));
    }
    fixture.ch('L');
    let screen = fixture.screen();
    assert!(screen.contains("following"), "{screen}");
    assert!(screen.contains("line 99") && !screen.contains("line 0 "));
}

#[test]
fn tab_keys_and_digits_move_focus_between_panels() {
    let mut fixture = Fixture::new();
    fixture.press(KeyCode::Tab);
    assert_eq!(fixture.app.focus, Panel::Presets);
    fixture.press(KeyCode::BackTab);
    fixture.press(KeyCode::BackTab);
    assert_eq!(fixture.app.focus, Panel::Repos);
    fixture.ch('3');
    assert_eq!(fixture.app.focus, Panel::Dotfiles);
}

#[test]
fn log_text_from_jobs_is_sanitized() {
    let mut fixture = Fixture::new();
    fixture
        .app
        .on_event(JobEvent::Log("ok\x1b]0;title\x07\ttabbed".into()));
    let line = fixture.app.log.back().unwrap();
    assert!(!line.text.contains('\x1b') && !line.text.contains('\x07'));
    assert!(line.text.contains("tabbed"));
}

#[test]
fn log_keeps_only_the_most_recent_lines() {
    let mut fixture = Fixture::new();
    for n in 0..1500 {
        fixture.app.on_event(JobEvent::Log(format!("line {n}")));
    }
    assert!(fixture.app.log.len() < 1500);
    assert_eq!(fixture.app.log.back().unwrap().text, "line 1499");
}

#[test]
fn the_fixture_keeps_everything_under_its_temp_home() {
    let fixture = Fixture::new();
    let paths = &fixture.app.session.workspace.paths;
    assert!(paths.home.starts_with(fixture.home.path()));
    assert!(paths.tool_state.starts_with(fixture.home.path()));
    assert!(paths.state_dir.starts_with(fixture.home.path()));
}

// ----- mouse ----------------------------------------------------------------

#[test]
fn clicking_a_panel_focuses_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.click_text("3 Dotfiles");
    assert_eq!(fixture.app.focus, Panel::Dotfiles);
    fixture.click_text("4 Repos");
    assert_eq!(fixture.app.focus, Panel::Repos);
    fixture.click_hit(Hit::Focus(Panel::Presets));
    assert_eq!(fixture.app.focus, Panel::Presets);
}

#[test]
fn clicking_tool_lines_selects_marks_and_opens_groups() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('2');
    fixture.click_text("Not selected");
    assert_eq!(fixture.app.focus, Panel::Tools);
    assert_eq!(fixture.tool_lines(), ["hello", "extra"]);
    // The tool's name is past its mark box.
    let (column, row) = position_of(&fixture.screen(), "[ ] ○ extra");
    fixture.mouse(MouseEventKind::Down(MouseButton::Left), column + 6, row);
    assert_eq!(fixture.app.current_tool().unwrap().name, "extra");
    assert!(fixture.app.tools.marked.is_empty());
    fixture.click_text("[ ] + hello");
    assert!(fixture.app.tools.marked.contains("hello"));
    assert_eq!(fixture.app.current_tool().unwrap().name, "hello");
    fixture.click_text("[x] + hello");
    assert!(fixture.app.tools.marked.is_empty());
}

#[test]
fn clicking_the_grouping_switch_and_filter() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.click_hit(Hit::Grouping(Grouping::Category));
    assert_eq!(fixture.app.tools.grouping, Grouping::Category);
    fixture.click_hit(Hit::Grouping(Grouping::Preset));
    assert_eq!(fixture.app.tools.grouping, Grouping::Preset);
    fixture.click_hit(Hit::Filter);
    assert!(fixture.app.tools.filtering);
    fixture.type_text("ext");
    assert_eq!(fixture.tool_lines(), ["extra"]);
}

#[test]
fn clicking_hint_chips_and_dialog_buttons_presses_their_keys() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.click_text("apply all (1)");
    assert!(matches!(
        confirm_request(&fixture.app),
        Some(JobRequest::Apply(_))
    ));
    fixture.click_text("n Cancel");
    assert!(fixture.app.modal.is_none());
    fixture.click_text("apply all (1)");
    assert!(matches!(
        fixture.click_text("y Confirm"),
        Effect::Start(JobRequest::Apply(_))
    ));
}

#[test]
fn clicking_detail_buttons_runs_the_action() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.select_tool("extra");
    fixture.click_text("Install on this host");
    match confirm_request(&fixture.app) {
        Some(JobRequest::Install(names)) => assert_eq!(names, &["extra"]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_click_outside_a_dialog_closes_it() {
    let mut fixture = Fixture::new();
    fixture.click_text("? help");
    assert!(matches!(fixture.app.modal, Some(Modal::Help { .. })));
    fixture.click_text("Keys and mouse");
    assert!(fixture.app.modal.is_some(), "a click inside keeps it open");
    fixture.mouse(MouseEventKind::Down(MouseButton::Left), 0, HEIGHT - 1);
    assert!(fixture.app.modal.is_none());
}

#[test]
fn clicking_preset_chips_selects_and_toggles() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.click_hit(Hit::Preset(1));
    assert_eq!(fixture.app.focus, Panel::Presets);
    assert_eq!(fixture.app.current_preset().as_deref(), Some("more"));
    assert_eq!(fixture.app.session.workspace.host.presets, ["base"]);
    fixture.click_hit(Hit::PresetToggle(1));
    assert!(
        fixture
            .app
            .session
            .workspace
            .host
            .presets
            .contains(&"more".to_string())
    );
}

#[test]
fn clicking_the_palette_switches_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    match start_request(fixture.click_hit(Hit::Palette)) {
        JobRequest::SetPalette(name) => assert_eq!(name, "latte"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn clicking_the_job_strip_opens_the_log() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.click_hit(Hit::Key(KeyCode::Char('L')));
    assert!(matches!(fixture.app.modal, Some(Modal::Log { .. })));
}

#[test]
fn the_wheel_moves_the_list_under_the_pointer_and_scrolls_the_detail() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    // Wheel over the same list cell both times: the detail pane changes with
    // the selection and could show the same text.
    let (column, row) = position_of(&fixture.screen(), "~/.zshrc");
    fixture.mouse(MouseEventKind::ScrollDown, column, row);
    assert_eq!(fixture.app.focus, Panel::Dotfiles);
    assert_eq!(fixture.app.dotfiles.list.cursor, 1);
    fixture.mouse(MouseEventKind::ScrollUp, column, row);
    assert_eq!(fixture.app.dotfiles.list.cursor, 0);

    fixture.ch('4');
    fixture.app.on_event(JobEvent::Done(Ok(sync_data())));
    fixture.wheel_at("abc1234 fix the thing", true);
    assert!(fixture.app.detail_scroll > 0);
    assert_eq!(
        fixture.app.focus,
        Panel::Repos,
        "the detail doesn't take focus"
    );
}

#[test]
fn clicking_form_fields_focuses_and_toggles() {
    let mut fixture = Fixture::new();
    fixture.settle();
    open_add_form(&mut fixture);
    let (count, check) = match &fixture.app.modal {
        Some(Modal::Form(form)) => (
            form.fields.len(),
            form.fields
                .iter()
                .position(|field| matches!(field.kind, super::input::FieldKind::Check(_)))
                .unwrap(),
        ),
        _ => unreachable!(),
    };
    fixture.click_hit(Hit::FormField(2));
    let focus = |fixture: &Fixture| match &fixture.app.modal {
        Some(Modal::Form(form)) => form.focus,
        _ => panic!("the form closed"),
    };
    assert_eq!(focus(&fixture), 2);
    fixture.click_hit(Hit::FormField(check));
    match &fixture.app.modal {
        Some(Modal::Form(form)) => assert!(form.checked(check)),
        _ => panic!("the form closed"),
    }
    assert!(check < count);
    fixture.click_text("esc Cancel");
    assert!(fixture.app.modal.is_none());
}

//! Tests drive [`App`] with key presses and render it into a `TestBackend`.
//! Every fixture lives in a temp directory used as `$HOME`, and tools are
//! `script` kinds, so nothing touches the network or the real home.

use super::app::{App, Effect, Modal};
use super::job::{self, JobEvent, JobOutput, JobRequest};
use super::state::Tab;
use super::ui;
use crate::gitops::{Incoming, Outgoing};
use crate::ops::{self, Session};
use crate::schedule;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
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
description = "says hello"
install = "mkdir -p \"$DFAPP_TOOL_DIR\" && printf '#!/bin/sh\necho hi\n' > \"$DFAPP_TOOL_DIR/hello\" && chmod +x \"$DFAPP_TOOL_DIR/hello\""
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
        write("dfapp.toml", REPO);
        write("zsh/.zshrc", "plain\n");
        write(
            "zsh/.config/zsh/theme.zsh.tmpl",
            "export EDITOR={{ editor }}\nplain line\n",
        );
        write("palettes/mocha.toml", "base = \"#111111\"\n");
        write("palettes/latte.toml", "base = \"#eeeeee\"\n");
        let host = home.path().join(".config/dfapp/host.toml");
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
        std::fs::read_to_string(self.home.path().join("dotfiles/dfapp.toml")).unwrap()
    }

    fn host_text(&self) -> String {
        std::fs::read_to_string(self.home.path().join(".config/dfapp/host.toml")).unwrap()
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
        buffer_text(&draw(&self.app, 120, 40))
    }

    fn status(&self) -> String {
        self.app
            .status
            .as_ref()
            .map(|status| status.text.clone())
            .unwrap_or_default()
    }

    /// Moves the Tools cursor onto `name`.
    fn select_tool(&mut self, name: &str) {
        let index = self
            .app
            .tools
            .visible()
            .iter()
            .position(|row| row.name == name)
            .unwrap_or_else(|| panic!("no row {name}"));
        self.app.tools.list.cursor = index;
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
fn every_tab_renders_its_content() {
    let mut fixture = Fixture::new();
    fixture.settle();

    let tools = fixture.screen();
    assert!(tools.contains("Tools 2/2"), "{tools}");
    assert!(tools.contains("hello") && tools.contains("extra"));
    assert!(tools.contains("latest"), "pin column");
    assert!(tools.contains("Details"));

    fixture.ch('2');
    let presets = fixture.screen();
    assert!(presets.contains("Presets 2"), "{presets}");
    assert!(presets.contains("[x] base") && presets.contains("[ ] more"));
    assert!(presets.contains("the basics"));

    fixture.ch('3');
    let dotfiles = fixture.screen();
    assert!(dotfiles.contains("Targets"), "{dotfiles}");
    assert!(dotfiles.contains("~/.zshrc"));
    assert!(dotfiles.contains("link"));
    assert!(dotfiles.contains("* mocha") && dotfiles.contains("latte"));
    assert!(dotfiles.contains("editor = nvim  [default]"));

    fixture.ch('4');
    fixture.settle();
    let sync = fixture.screen();
    assert!(sync.contains("Repos"), "{sync}");

    fixture.ch('5');
    fixture.settle();
    let settings = fixture.screen();
    assert!(settings.contains("Host config"), "{settings}");
    assert!(settings.contains("valid: 2 tools, 2 presets, 1 packages, 2 palettes"));
    assert!(settings.contains("no scheduled update"));
    assert!(settings.contains("present"));
}

#[test]
fn small_terminals_do_not_panic() {
    let mut fixture = Fixture::new();
    fixture.settle();
    for tab in Tab::ALL {
        fixture.app.tab = tab;
        for (width, height) in [(20, 5), (60, 12), (80, 24), (200, 60)] {
            draw(&fixture.app, width, height);
        }
    }
    fixture.ch('1');
    fixture.ch('?');
    draw(&fixture.app, 30, 8);
    fixture.press(KeyCode::Esc);
    fixture.ch('n');
    assert!(matches!(fixture.app.modal, Some(Modal::Form(_))));
    draw(&fixture.app, 30, 8);
}

#[test]
fn the_tools_table_shows_planned_actions_and_details() {
    let mut fixture = Fixture::new();
    assert!(fixture.screen().contains("Looking up tool versions"));
    fixture.settle();
    fixture.select_tool("hello");
    let screen = fixture.screen();
    assert!(screen.contains("install"), "{screen}");
    assert!(screen.contains("Selected: yes (base)"), "{screen}");
    assert!(screen.contains("says hello"));
    // Script tools only learn their version when installed.
    assert!(screen.contains("Available: unknown"));
    fixture.select_tool("extra");
    assert!(fixture.screen().contains("Selected: no"));
}

#[test]
fn log_pane_shows_lines_and_toggles_size() {
    let mut fixture = Fixture::new();
    fixture
        .app
        .on_event(JobEvent::Log("something happened".into()));
    assert!(fixture.screen().contains("something happened"));
    assert!(!fixture.app.log_large);
    fixture.ch('l');
    assert!(fixture.app.log_large);
}

// ----- filtering and marking ---------------------------------------------

#[test]
fn filter_narrows_the_table_and_escape_clears_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('/');
    fixture.type_text("HEL");
    assert_eq!(fixture.app.tools.visible().len(), 1);
    assert!(fixture.screen().contains("filter: HEL_"));
    // Typing `q` while filtering edits the filter instead of quitting.
    assert!(matches!(fixture.ch('q'), Effect::None));
    assert_eq!(fixture.app.tools.visible().len(), 0);
    fixture.press(KeyCode::Backspace);
    fixture.press(KeyCode::Enter);
    assert!(!fixture.app.tools.filtering);
    assert_eq!(fixture.app.tools.visible().len(), 1);
    assert!(!fixture.screen().contains("extra"));
    fixture.ch('/');
    fixture.press(KeyCode::Esc);
    assert_eq!(fixture.app.tools.visible().len(), 2);
}

#[test]
fn space_marks_rows_and_m_marks_all_shown() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch(' ');
    assert!(fixture.app.tools.marked.contains("extra"));
    assert_eq!(fixture.app.tools.list.cursor, 1, "space moves down");
    fixture.ch(' ');
    assert_eq!(fixture.app.tools.marked.len(), 2);
    assert!(fixture.screen().contains("2 marked"));
    fixture.ch('m');
    assert!(fixture.app.tools.marked.is_empty());
    fixture.ch('m');
    assert_eq!(fixture.app.tools.marked.len(), 2);
    fixture.ch('m');
    assert!(fixture.app.tools.marked.is_empty());
}

// ----- confirm dialogs and jobs -------------------------------------------

#[test]
fn apply_asks_first_and_then_starts_the_confirmed_plan() {
    let mut fixture = Fixture::new();
    // Nothing to apply until the plan has loaded.
    fixture.ch('a');
    assert!(fixture.app.modal.is_none());
    fixture.settle();

    assert!(matches!(fixture.ch('a'), Effect::None));
    let screen = fixture.screen();
    assert!(screen.contains("Apply 1 change(s)?"), "{screen}");
    assert!(screen.contains("install    hello"), "{screen}");
    assert!(screen.contains("y confirm"));

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
fn the_confirm_dialog_waits_when_a_refresh_is_running() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('a');
    // A queued refresh grabs the worker while the dialog is open.
    fixture.app.want(JobRequest::LoadPlan);
    assert!(matches!(
        fixture.app.pump(),
        Effect::Start(JobRequest::LoadPlan)
    ));
    assert!(matches!(fixture.ch('y'), Effect::None));
    assert!(fixture.app.modal.is_some(), "the dialog stays open");
    assert!(fixture.status().contains("press y again"));
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
    assert!(status.error && status.text.contains("busy"), "{status:?}");
    assert!(
        fixture
            .app
            .log
            .iter()
            .any(|line| line.error && line.text.contains("deploying dotfiles"))
    );

    // Dialogs and edits are refused too, before the user fills anything in.
    fixture.ch('1');
    fixture.ch('a');
    assert!(fixture.app.modal.is_none());
    assert!(fixture.status().contains("busy"));
    fixture.ch('2');
    assert!(matches!(fixture.ch(' '), Effect::None));
    assert!(fixture.status().contains("busy"));
    assert!(fixture.host_text().contains("base"), "selection untouched");

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

    // Any other key in between disarms it.
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    start_request(fixture.ch('d'));
    fixture.press(KeyCode::Esc);
    fixture.ch('j');
    assert!(matches!(fixture.press(KeyCode::Esc), Effect::None));
    assert!(matches!(fixture.press(KeyCode::Esc), Effect::Quit));
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
fn install_reinstall_uninstall_and_rollback_build_the_right_requests() {
    let mut fixture = Fixture::new();
    fixture.settle();

    // Install is for tools no preset or host entry selects.
    fixture.select_tool("hello");
    assert!(matches!(fixture.ch('i'), Effect::None));
    assert!(fixture.status().contains("already selected"));
    fixture.select_tool("extra");
    match start_request(fixture.ch('i')) {
        JobRequest::Install(names) => assert_eq!(names, ["extra"]),
        other => panic!("{other:?}"),
    }
    fixture.run(JobRequest::Install(vec!["extra".into()]));
    fixture.settle();

    // Reinstall works on selected tools only, and asks first.
    fixture.select_tool("hello");
    fixture.ch('r');
    match confirm_request(&fixture.app) {
        Some(JobRequest::Reinstall(names)) => assert_eq!(names, &["hello"]),
        other => panic!("{other:?}"),
    }
    assert!(fixture.screen().contains("Reinstall 1 tool(s)?"));
    fixture.ch('n');

    // Marked rows win over the highlighted one.
    fixture.ch(' ');
    fixture.select_tool("extra");
    fixture.ch('u');
    match confirm_request(&fixture.app) {
        Some(JobRequest::Uninstall(names)) => assert_eq!(names, &["hello"]),
        other => panic!("{other:?}"),
    }
    assert!(fixture.screen().contains("deselect hello (not installed)"));
    match start_request(fixture.ch('y')) {
        JobRequest::Uninstall(names) => assert_eq!(names, ["hello"]),
        other => panic!("{other:?}"),
    }
    fixture.run(JobRequest::Uninstall(vec!["hello".into()]));
    fixture.settle();

    // Rollback needs a kept previous version.
    fixture.select_tool("extra");
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
    assert_eq!(fixture.status(), "done");
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
    assert!(screen.contains("Installed: 1.0.0 (script)"), "{screen}");
    fixture.ch('a');
    assert!(fixture.app.modal.is_none());
    assert!(fixture.status().contains("nothing to apply"));
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
    assert!(status.error && status.text.contains("disk on fire"));
    assert!(fixture.screen().contains("disk on fire"));
    // A refresh is queued behind the failure.
    assert!(matches!(fixture.app.pump(), Effect::Start(_)));
}

#[test]
fn a_failed_version_lookup_shows_on_the_tools_tab() {
    let mut fixture = Fixture::new();
    let Effect::Start(JobRequest::LoadPlan) = fixture.app.pump() else {
        panic!("the plan loads first");
    };
    fixture
        .app
        .on_event(JobEvent::Done(Err("no network".into())));
    assert!(
        fixture
            .screen()
            .contains("Could not look up the tools: no network")
    );
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

/// Moves form focus forward `count` fields (the form opens on field 0).
fn tab_forward(fixture: &mut Fixture, count: usize) {
    for _ in 0..count {
        fixture.press(KeyCode::Tab);
    }
}

fn open_add_form(fixture: &mut Fixture) {
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
    assert!(screen.contains("Add a tool"));
    assert!(screen.contains(&format!("asset ({})", fixture.app.asset_key())));

    // Empty and malformed names.
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).starts_with("name:"));
    fixture.type_text("bad name");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).starts_with("name:"));
    assert!(fixture.screen().contains("name: use letters"));

    // An existing name.
    for _ in 0..8 {
        fixture.press(KeyCode::Backspace);
    }
    fixture.type_text("hello");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("already exists"));

    // No repo, crate, or kind.
    for _ in 0..5 {
        fixture.press(KeyCode::Backspace);
    }
    fixture.type_text("fd");
    fixture.press(KeyCode::Enter);
    assert_eq!(
        form_error(&fixture),
        "give a repo or a crate, or pick a kind"
    );

    // A repo that isn't owner/name.
    tab_forward(&mut fixture, 2);
    fixture.type_text("fd");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("isn't an owner/name slug"));

    // A github tool needs an asset glob for some platform.
    for _ in 0..2 {
        fixture.press(KeyCode::Backspace);
    }
    fixture.type_text("sharkdp/fd");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("need an `assets` table"));

    // A bad pin.
    tab_forward(&mut fixture, 1);
    fixture.type_text("1*2");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("tools.fd.pin"));

    // Editing clears the message, and Esc leaves the config untouched.
    fixture.press(KeyCode::Backspace);
    assert!(matches!(&fixture.app.modal, Some(Modal::Form(form)) if form.error.is_none()));
    fixture.press(KeyCode::Esc);
    assert!(fixture.app.modal.is_none());
    assert_eq!(fixture.config_text(), before);
}

#[test]
fn add_tool_form_writes_the_definition_and_can_install_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    open_add_form(&mut fixture);
    fixture.type_text("fd");
    tab_forward(&mut fixture, 2); // repo (the kind stays on auto)
    fixture.type_text("sharkdp/fd");
    tab_forward(&mut fixture, 1);
    fixture.type_text("10");
    tab_forward(&mut fixture, 2); // asset
    fixture.type_text("fd-*-linux.tar.gz");
    tab_forward(&mut fixture, 1);
    fixture.type_text("fd, fdfind");
    tab_forward(&mut fixture, 1);
    fixture.type_text("simple find");
    tab_forward(&mut fixture, 1);
    fixture.ch(' '); // install now
    let effect = fixture.press(KeyCode::Enter);

    assert!(fixture.app.modal.is_none());
    match start_request(effect) {
        JobRequest::Install(names) => assert_eq!(names, ["fd"]),
        other => panic!("{other:?}"),
    }
    let key = fixture.app.asset_key();
    let tool = &fixture.app.session.workspace.config.tools["fd"];
    assert_eq!(tool.repo.as_deref(), Some("sharkdp/fd"));
    assert_eq!(tool.pin.as_deref(), Some("10"));
    assert_eq!(tool.bins, ["fd", "fdfind"]);
    assert_eq!(tool.description.as_deref(), Some("simple find"));
    assert_eq!(tool.assets[&key], "fd-*-linux.tar.gz");
    let text = fixture.config_text();
    assert!(text.contains("[tools.fd]") && text.contains("sharkdp/fd"));
    // The existing file keeps its other content.
    assert!(text.contains("[tools.hello]"));
}

#[test]
fn add_tool_without_install_now_only_writes_the_config() {
    let mut fixture = Fixture::new();
    fixture.settle();
    open_add_form(&mut fixture);
    fixture.type_text("ripgrep");
    tab_forward(&mut fixture, 2);
    fixture.type_text("BurntSushi/ripgrep");
    tab_forward(&mut fixture, 3);
    fixture.type_text("rg-*.tar.gz");
    let effect = fixture.press(KeyCode::Enter);
    assert!(matches!(effect, Effect::None), "{effect:?}");
    assert!(fixture.app.modal.is_none());
    assert!(
        fixture
            .app
            .session
            .workspace
            .config
            .tools
            .contains_key("ripgrep")
    );
    // The new tool shows up once the plan refreshes.
    fixture.settle();
    assert!(fixture.screen().contains("ripgrep"));
}

#[test]
fn the_add_tool_form_cannot_submit_while_a_job_runs() {
    let mut fixture = Fixture::new();
    fixture.settle();
    open_add_form(&mut fixture);
    fixture.type_text("fd");
    tab_forward(&mut fixture, 1);
    fixture.press(KeyCode::Right); // github
    tab_forward(&mut fixture, 1);
    fixture.type_text("sharkdp/fd");
    // A refresh starts while the form is open.
    fixture.app.want(JobRequest::LoadPlan);
    assert!(matches!(fixture.app.pump(), Effect::Start(_)));
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("still running"));
    assert!(!fixture.config_text().contains("[tools.fd]"));
}

// ----- presets ----------------------------------------------------------------

#[test]
fn space_toggles_a_preset_on_this_host() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('2');
    // base is first; deselect it.
    fixture.ch(' ');
    assert!(fixture.app.session.workspace.host.presets.is_empty());
    assert!(!fixture.host_text().contains("\"base\""));
    assert!(fixture.screen().contains("[ ] base"));

    // Select `more`, which extends base: base shows as included.
    fixture.ch('j');
    fixture.ch(' ');
    assert_eq!(fixture.app.session.workspace.host.presets, ["more"]);
    let screen = fixture.screen();
    assert!(screen.contains("[+] base"), "{screen}");
    assert!(screen.contains("[x] more"));
    assert!(screen.contains("Extends: base"));
    assert!(fixture.app.session.selection.tools.contains("extra"));

    // The views refresh behind the edit.
    fixture.settle();
    fixture.ch('1');
    fixture.select_tool("extra");
    assert!(fixture.screen().contains("Selected: yes (more)"));
}

#[test]
fn n_creates_a_preset_from_the_marked_tools() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('2');
    fixture.ch('n');
    assert!(fixture.app.modal.is_none(), "no marked tools yet");
    assert!(fixture.status().contains("mark tools"));

    fixture.ch('1');
    fixture.ch('m');
    fixture.ch('2');
    fixture.ch('n');
    assert!(fixture.screen().contains("2 marked tool(s)"));
    fixture.type_text("mine");
    fixture.press(KeyCode::Tab);
    fixture.type_text("both tools");
    fixture.press(KeyCode::Enter);
    assert!(fixture.app.modal.is_none());
    let preset = &fixture.app.session.workspace.config.presets["mine"];
    assert_eq!(preset.tools, ["extra", "hello"]);
    assert_eq!(preset.description.as_deref(), Some("both tools"));
    assert!(fixture.config_text().contains("[presets.mine]"));
    // It is created, not selected.
    assert!(
        !fixture
            .app
            .session
            .workspace
            .host
            .presets
            .contains(&"mine".to_string())
    );

    // A name that exists, or isn't valid, is refused with the form kept open.
    fixture.ch('n');
    fixture.type_text("base");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("already exists"));
    for _ in 0..4 {
        fixture.press(KeyCode::Backspace);
    }
    fixture.type_text("a/b");
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).starts_with("name:"));
}

// ----- dotfiles -----------------------------------------------------------------

#[test]
fn palette_keys_start_a_switch_to_another_palette() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    match start_request(fixture.ch('p')) {
        JobRequest::SetPalette(name) => assert_eq!(name, "latte"),
        other => panic!("{other:?}"),
    }
    fixture.run(JobRequest::SetPalette("latte".into()));
    assert!(fixture.host_text().contains("latte"));
    assert_eq!(fixture.app.session.workspace.palette_name(), Some("latte"));
    fixture.settle();

    // Picking from the list: the active palette is a no-op.
    fixture.press(KeyCode::Right);
    assert!(matches!(fixture.press(KeyCode::Enter), Effect::None));
    assert!(fixture.status().contains("already the active palette"));
    fixture.ch('j');
    match start_request(fixture.press(KeyCode::Enter)) {
        JobRequest::SetPalette(name) => assert_eq!(name, "mocha"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn secret_variables_stay_hidden_everywhere() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    let screen = fixture.screen();
    assert!(screen.contains("token = ********"), "{screen}");
    assert!(!screen.contains("s3cretvalue"));

    // Move to the token and edit it: the field is masked and empty input is refused.
    fixture.press(KeyCode::Right);
    fixture.press(KeyCode::Right);
    fixture.ch('j');
    fixture.ch('e');
    fixture.press(KeyCode::Enter);
    assert!(form_error(&fixture).contains("type a value"));
    fixture.type_text("hunter2hunter2");
    let screen = fixture.screen();
    assert!(!screen.contains("hunter2"), "{screen}");
    assert!(screen.contains("**************"));
    fixture.press(KeyCode::Enter);
    assert!(fixture.app.modal.is_none());
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
fn editing_a_plain_variable_prefills_and_saves_it() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    fixture.ch('e');
    let screen = fixture.screen();
    assert!(screen.contains("Set variable editor"), "{screen}");
    assert!(screen.contains("nvim"));
    fixture.type_text("x");
    fixture.press(KeyCode::Enter);
    assert_eq!(fixture.app.session.workspace.host.vars["editor"], "nvimx");
    assert!(fixture.status().contains("press d"));
    // The preview refreshes and shows the new value.
    fixture.settle();
    assert!(fixture.screen().contains("editor = nvimx  [host]"));
}

#[test]
fn deploying_and_capturing_walk_through_a_confirm_dialog() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('3');
    fixture.ch('c');
    assert!(fixture.status().contains("no rendered files"));

    // Deploy for real, into the temp home, then edit the rendered file in place.
    fixture.run(JobRequest::Deploy);
    let rendered = fixture.home.path().join(".config/zsh/theme.zsh");
    assert_eq!(
        std::fs::read_to_string(&rendered).unwrap(),
        "export EDITOR=nvim\nplain line\n"
    );
    std::fs::write(&rendered, "export EDITOR=nvim\nplain line\nadded by hand\n").unwrap();
    fixture.refresh_dotfiles();
    assert!(
        fixture
            .screen()
            .contains("1 rendered file(s) edited in place")
    );

    fixture.ch('c');
    let screen = fixture.screen();
    assert!(screen.contains("Capture 1 edited file(s)"), "{screen}");
    assert!(screen.contains("+added by hand"), "{screen}");
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
fn changes_only_hides_unchanged_targets() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.run(JobRequest::Deploy);
    fixture.refresh_dotfiles();
    fixture.ch('3');
    assert!(fixture.screen().contains("0 to change, 2 total"));
    assert!(fixture.screen().contains("~/.zshrc"));
    fixture.ch('v');
    let screen = fixture.screen();
    assert!(screen.contains("(changes only)"), "{screen}");
    assert!(!screen.contains("~/.zshrc"));
    assert!(screen.contains("Nothing to show"));
}

// ----- sync and settings -----------------------------------------------------------

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

/// The foreground color of the first cell where `needle` appears on screen.
fn color_of(app: &App, needle: &str) -> Option<Color> {
    let buffer = draw(app, 120, 40);
    let text = buffer_text(&buffer);
    let (row, line) = text
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(needle))
        .unwrap_or_else(|| panic!("{needle} not shown"));
    let column = line[..line.find(needle).unwrap()].chars().count();
    buffer[(column as u16, row as u16)].style().fg
}

#[test]
fn the_sync_tab_lists_repos_and_colors_the_diff() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('4');
    // Visiting the tab queues the first check.
    assert!(matches!(
        fixture.app.pump(),
        Effect::Start(JobRequest::LoadSync)
    ));
    assert!(fixture.screen().contains("Checking the repos"));
    fixture.app.on_event(JobEvent::Done(Ok(sync_data())));

    let screen = fixture.screen();
    assert!(screen.contains("in  main"), "{screen}");
    assert!(screen.contains("1 commit(s)"));
    assert!(screen.contains("out main"));
    assert!(screen.contains("incoming from origin/main"));
    assert!(screen.contains("abc1234 fix the thing"));

    // Added lines are green and removed lines red.
    assert_eq!(color_of(&fixture.app, "+added line 3"), Some(Color::Green));

    // Scrolling reaches the removed line, and moving to the next repo resets it.
    fixture.ch('J');
    fixture.ch('J');
    fixture.ch('J');
    assert_eq!(fixture.app.sync.diff_scroll, 30);
    assert!(!fixture.screen().contains("+added line 3\n"));
    assert_eq!(color_of(&fixture.app, "-removed line"), Some(Color::Red));
    fixture.ch('K');
    assert_eq!(fixture.app.sync.diff_scroll, 20);
    fixture.ch('j');
    assert_eq!(fixture.app.sync.diff_scroll, 0);
    let screen = fixture.screen();
    assert!(screen.contains("local changes") && screen.contains("unpushed: def5678"));
    // Control characters in a diff never reach the screen.
    assert!(screen.contains("+local edit?[31m"), "{screen}");
    assert!(!screen.contains('\x1b'));
}

#[test]
fn sync_starts_a_job_and_update_asks_first() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('4');
    fixture.settle();
    assert!(matches!(fixture.ch('U'), Effect::None));
    let screen = fixture.screen();
    assert!(screen.contains("Update now?"), "{screen}");
    assert!(matches!(
        confirm_request(&fixture.app),
        Some(JobRequest::Update)
    ));
    fixture.ch('n');
    assert!(fixture.app.job.is_none());
    assert!(matches!(fixture.ch('s'), Effect::Start(JobRequest::Sync)));
}

#[test]
fn the_settings_tab_shows_the_schedule() {
    let mut fixture = Fixture::new();
    fixture.settle();
    fixture.ch('5');
    assert!(fixture.screen().contains("reading..."));
    fixture
        .app
        .on_event(JobEvent::Done(Ok(JobOutput::Schedule(schedule::Status {
            installed: true,
            schedule: Some("weekly".into()),
            next_run: Some("Mon 04:17".into()),
            last_result: Some("success".into()),
        }))));
    let screen = fixture.screen();
    assert!(screen.contains("schedule: weekly"), "{screen}");
    assert!(screen.contains("next run: Mon 04:17"));
    assert!(screen.contains("last result: success"));
}

#[test]
fn help_overlay_opens_and_any_key_closes_it() {
    let mut fixture = Fixture::new();
    fixture.ch('?');
    assert!(fixture.screen().contains("Keys (any key closes)"));
    fixture.ch('x');
    assert!(fixture.app.modal.is_none());
    assert!(!fixture.screen().contains("Keys (any key closes)"));
}

#[test]
fn tab_keys_cycle_through_the_tabs() {
    let mut fixture = Fixture::new();
    fixture.press(KeyCode::Tab);
    assert_eq!(fixture.app.tab, Tab::Presets);
    fixture.press(KeyCode::BackTab);
    fixture.press(KeyCode::BackTab);
    assert_eq!(fixture.app.tab, Tab::Settings);
    fixture.ch('3');
    assert_eq!(fixture.app.tab, Tab::Dotfiles);
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
    assert_eq!(fixture.app.log.len(), 1000);
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

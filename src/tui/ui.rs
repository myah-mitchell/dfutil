//! Drawing. Colors are limited to the terminal's own red, green, and yellow;
//! everything else is bold, dim, underline, or reverse video, so the screen
//! reads the same on light and dark themes.

use super::app::{App, Confirm, Modal, change_verb};
use super::input::{FieldKind, Form, Input};
use super::state::*;
use crate::config::VarDef;
use crate::deploy::Change;
use crate::plan::{Action, ToolRow};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap};

const SPINNER: [&str; 4] = ["|", "/", "-", "\\"];

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn reversed() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

fn red() -> Style {
    Style::default().fg(Color::Red)
}

fn green() -> Style {
    Style::default().fg(Color::Green)
}

fn yellow() -> Style {
    Style::default().fg(Color::Yellow)
}

fn pane(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(if focused {
            BorderType::Thick
        } else {
            BorderType::Plain
        })
        .title(title)
}

/// Draws the whole screen.
pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let log_height = if area.height < 16 {
        0
    } else if app.log_large {
        (area.height / 2).max(8)
    } else {
        6
    };
    let [tabs, body, log, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(log_height),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_tabs(frame, tabs, app);
    match app.tab {
        Tab::Tools => draw_tools(frame, body, app),
        Tab::Presets => draw_presets(frame, body, app),
        Tab::Dotfiles => draw_dotfiles(frame, body, app),
        Tab::Sync => draw_sync(frame, body, app),
        Tab::Settings => draw_settings(frame, body, app),
    }
    if log_height > 0 {
        draw_log(frame, log, app);
    }
    draw_status(frame, status, app);
    match &app.modal {
        Some(Modal::Help) => draw_help(frame, area),
        Some(Modal::Confirm(confirm)) => draw_confirm(frame, area, confirm),
        Some(Modal::Form(form)) => draw_form(frame, area, form),
        None => {}
    }
}

fn draw_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let titles: Vec<String> = Tab::ALL
        .iter()
        .map(|tab| format!(" {} {} ", tab.index() + 1, tab.title()))
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .highlight_style(reversed().add_modifier(Modifier::BOLD))
        .divider("");
    frame.render_widget(tabs, area);
}

fn hints(tab: Tab) -> &'static str {
    match tab {
        Tab::Tools => {
            "space mark  a apply  r reinstall  i install  u uninstall  b rollback  n add  / filter  R refresh  ? help  q quit"
        }
        Tab::Presets => "space select  n new from marked tools  ? help  q quit",
        Tab::Dotfiles => {
            "d deploy  c capture  p palette  e edit var  v changes only  left/right pane  ? help  q quit"
        }
        Tab::Sync => "s sync  U update  R refresh  j/k repo  J/K scroll diff  ? help  q quit",
        Tab::Settings => "R refresh  j/k scroll  ? help  q quit",
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = Vec::new();
    if let Some(job) = &app.job {
        spans.push(Span::styled(
            format!(
                "{} {}  ",
                SPINNER[app.frame / 2 % SPINNER.len()],
                clean(&job.label)
            ),
            bold(),
        ));
    }
    match &app.status {
        Some(status) => spans.push(Span::styled(
            clean(&status.text),
            if status.error {
                red().add_modifier(Modifier::BOLD)
            } else {
                bold()
            },
        )),
        None => spans.push(Span::styled(hints(app.tab), dim())),
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title(" Log (l: resize) ");
    let inner = block.inner(area);
    let height = inner.height as usize;
    let skip = app.log.len().saturating_sub(height);
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(skip)
        .map(|line| {
            Line::styled(
                line.text.clone(),
                if line.error { red() } else { Style::default() },
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ----- tools ----------------------------------------------------------------

fn action_style(action: Action) -> Style {
    match action {
        Action::Install | Action::Upgrade | Action::Reinstall => {
            green().add_modifier(Modifier::BOLD)
        }
        Action::Uninstall => yellow().add_modifier(Modifier::BOLD),
        Action::Error => red().add_modifier(Modifier::BOLD),
        Action::Keep => Style::default(),
        Action::Available => dim(),
    }
}

fn draw_tools(frame: &mut Frame, area: Rect, app: &App) {
    let [table_area, detail_area] = if area.width >= 110 {
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area)
    } else {
        Layout::vertical([Constraint::Min(6), Constraint::Length(11)]).areas(area)
    };
    let tools = &app.tools;
    let visible = tools.visible();
    let total = tools.plan.as_ref().map_or(0, |plan| plan.rows.len());
    let mut title = format!(" Tools {}/{}", visible.len(), total);
    if !tools.marked.is_empty() {
        title.push_str(&format!(" | {} marked", tools.marked.len()));
    }
    if tools.filtering || !tools.filter.is_empty() {
        title.push_str(&format!(
            " | filter: {}{}",
            clean(tools.filter.text()),
            if tools.filtering { "_" } else { "" }
        ));
    }
    title.push(' ');
    let block = pane(title, true);
    if tools.plan.is_none() {
        let text = match &tools.error {
            Some(error) => format!(
                "Could not look up the tools: {}\n\nPress R to retry.",
                clean(error)
            ),
            None => "Looking up tool versions... (this uses the network; press R to retry)".into(),
        };
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(block),
            table_area,
        );
    } else {
        let height = (table_area.height as usize).saturating_sub(3);
        let range = tools.list.window(height, visible.len());
        let rows: Vec<Row> = range
            .clone()
            .map(|index| {
                let row = visible[index];
                let marked = if tools.marked.contains(&row.name) {
                    "[x]"
                } else {
                    "[ ]"
                };
                let style = if index == tools.list.cursor {
                    reversed()
                } else {
                    Style::default()
                };
                Row::new(vec![
                    Cell::from(marked),
                    Cell::from(row.name.clone()),
                    Cell::from(if row.selected { "yes" } else { "-" }),
                    Cell::from(
                        row.installed
                            .clone()
                            .map(|v| clean(&v))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Cell::from(
                        row.available
                            .clone()
                            .map(|v| clean(&v))
                            .unwrap_or_else(|| "-".into()),
                    ),
                    Cell::from(clean(&row.pin)),
                    Cell::from(action_label(row.action)).style(if index == tools.list.cursor {
                        Style::default()
                    } else {
                        action_style(row.action)
                    }),
                ])
                .style(style)
            })
            .collect();
        let header = Row::new(["", "Tool", "Sel", "Installed", "Available", "Pin", "Action"])
            .style(bold().add_modifier(Modifier::UNDERLINED));
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
                Constraint::Length(12),
                Constraint::Length(12),
                Constraint::Length(8),
                Constraint::Length(10),
            ],
        )
        .header(header)
        .block(block);
        frame.render_widget(table, table_area);
    }
    let details = match tools.current() {
        Some(row) => tool_details(row),
        None => vec![Line::styled("No tool highlighted.", dim())],
    };
    frame.render_widget(
        Paragraph::new(details)
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(" Details ")),
        detail_area,
    );
}

fn labeled(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label}: "), bold()),
        Span::raw(value.into()),
    ])
}

fn tool_details(row: &ToolRow) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(row.name.clone(), bold())];
    if let Some(description) = &row.description {
        lines.push(Line::raw(clean(description)));
    }
    lines.push(Line::raw(""));
    lines.push(labeled(
        "Selected",
        if row.selected {
            format!("yes ({})", clean(&row.reasons.join(", ")))
        } else {
            "no".into()
        },
    ));
    lines.push(labeled(
        "Installed",
        match (&row.installed, row.installed_method) {
            (Some(version), Some(method)) => format!("{} ({method})", clean(version)),
            (Some(version), None) => clean(version),
            _ => "no".into(),
        },
    ));
    lines.push(labeled(
        "Available",
        match (&row.available, row.method) {
            (Some(version), Some(method)) => format!("{} ({method})", clean(version)),
            (Some(version), None) => clean(version),
            _ => "unknown".into(),
        },
    ));
    lines.push(labeled("Pin", clean(&row.pin)));
    lines.push(labeled(
        "Plan",
        format!("{} ({})", action_label(row.action), clean(&row.why)),
    ));
    if row.installed.is_some() {
        lines.push(labeled(
            "Checksum",
            if row.verified {
                "verified against a published checksum"
            } else {
                "not verified"
            },
        ));
    }
    if row.has_previous {
        lines.push(labeled("Rollback", "a previous version is kept (b)"));
    }
    for note in &row.notes {
        lines.push(labeled("Note", clean(note)));
    }
    if let Some(error) = &row.error {
        lines.push(Line::from(vec![
            Span::styled("Error: ", red().add_modifier(Modifier::BOLD)),
            Span::styled(clean(error), red()),
        ]));
    }
    lines
}

// ----- presets --------------------------------------------------------------

fn draw_presets(frame: &mut Frame, area: Rect, app: &App) {
    let [list_area, detail_area] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(area);
    let config = &app.session.workspace.config;
    let host = &app.session.workspace.host;
    let included = crate::selection::expand_presets(&host.presets, &config.presets);
    let names = app.preset_names();
    let block = pane(format!(" Presets {} ", names.len()), true);
    let height = (list_area.height as usize).saturating_sub(2);
    let range = app.presets.list.window(height, names.len());
    let lines: Vec<Line> = range
        .map(|index| {
            let name = &names[index];
            let marker = if host.presets.contains(name) {
                "[x]"
            } else if included.contains(name) {
                "[+]"
            } else {
                "[ ]"
            };
            let preset = &config.presets[name];
            Line::styled(
                format!("{marker} {name:<18} {} tool(s)", preset.tools.len()),
                if index == app.presets.list.cursor {
                    reversed()
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), list_area);

    let details = match names.get(app.presets.list.cursor) {
        Some(name) => {
            let preset = &config.presets[name];
            let mut lines = vec![Line::styled(name.clone(), bold())];
            if let Some(description) = &preset.description {
                lines.push(Line::raw(clean(description)));
            }
            lines.push(Line::raw(""));
            let status = if host.presets.contains(name) {
                "selected on this host"
            } else if included.contains(name) {
                "included through another selected preset"
            } else {
                "not selected"
            };
            lines.push(labeled("Status", status));
            if !preset.extends.is_empty() {
                lines.push(labeled("Extends", preset.extends.join(", ")));
            }
            lines.push(labeled("Tools", join_or_none(&preset.tools)));
            lines.push(labeled("Packages", join_or_none(&preset.packages)));
            if !preset.shells.is_empty() {
                lines.push(labeled("Shells", preset.shells.join(", ")));
            }
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "[x] selected here   [+] included by another   [ ] off",
                dim(),
            ));
            lines
        }
        None => vec![Line::styled("This config defines no presets.", dim())],
    };
    frame.render_widget(
        Paragraph::new(details)
            .wrap(Wrap { trim: true })
            .block(Block::bordered().title(" Details ")),
        detail_area,
    );
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "(none)".into()
    } else {
        items.join(", ")
    }
}

// ----- dotfiles -------------------------------------------------------------

fn cursor_style(focused: bool, current: bool) -> Style {
    match (focused, current) {
        (true, true) => reversed(),
        (false, true) => Style::default().add_modifier(Modifier::UNDERLINED),
        _ => Style::default(),
    }
}

fn change_style(change: Change) -> Style {
    match change {
        Change::Unchanged | Change::Skipped => dim(),
        Change::Error => red().add_modifier(Modifier::BOLD),
        Change::BackedUp | Change::Pruned => yellow(),
        Change::Linked | Change::Rerendered => green(),
    }
}

fn draw_dotfiles(frame: &mut Frame, area: Rect, app: &App) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).areas(area);
    let [palettes_area, vars_area] =
        Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(right);
    let state = &app.dotfiles;
    let home = &app.session.workspace.paths.home;

    // Targets.
    let visible = state.visible_items();
    let pending = state
        .items
        .iter()
        .flatten()
        .filter(|item| !matches!(item.change, Change::Unchanged | Change::Skipped))
        .count();
    let title = format!(
        " Targets: {pending} to change, {} total{} ",
        state.items.as_ref().map_or(0, Vec::len),
        if state.changes_only {
            " (changes only)"
        } else {
            ""
        }
    );
    let block = pane(title, state.focus == DotPane::Targets);
    let inner = block.inner(left);
    frame.render_widget(block, left);
    let mut notices: Vec<Line> = Vec::new();
    if !state.edited.is_empty() {
        notices.push(Line::styled(
            format!(
                "{} rendered file(s) edited in place; press c to capture",
                state.edited.len()
            ),
            yellow(),
        ));
    }
    for problem in &state.problems {
        notices.push(Line::styled(clean(problem), red()));
    }
    let notice_height = (notices.len() as u16).min(inner.height.saturating_sub(1));
    let [notice_area, list_area] =
        Layout::vertical([Constraint::Length(notice_height), Constraint::Min(1)]).areas(inner);
    frame.render_widget(Paragraph::new(notices), notice_area);
    if state.items.is_none() {
        frame.render_widget(Paragraph::new("Checking dotfiles..."), list_area);
    } else if visible.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("Nothing to show.", dim())),
            list_area,
        );
    } else {
        let range = state
            .targets
            .window(list_area.height as usize, visible.len());
        let lines: Vec<Line> = range
            .map(|index| {
                let item = visible[index];
                let shown = item
                    .target
                    .strip_prefix(home)
                    .map(|relative| format!("~/{}", relative.display()))
                    .unwrap_or_else(|_| item.target.display().to_string());
                let mut spans = vec![
                    Span::styled(
                        format!("{:<10} ", change_verb(item.change)),
                        change_style(item.change),
                    ),
                    Span::raw(clean(&shown)),
                ];
                if !item.detail.is_empty() {
                    spans.push(Span::styled(format!("  ({})", clean(&item.detail)), dim()));
                }
                Line::from(spans).style(cursor_style(
                    state.focus == DotPane::Targets,
                    index == state.targets.cursor,
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), list_area);
    }

    // Palettes.
    let names = app.palette_names();
    let active = app.session.workspace.palette_name();
    let block = pane(
        format!(" Palettes {} (p cycles) ", names.len()),
        state.focus == DotPane::Palettes,
    );
    let height = (palettes_area.height as usize).saturating_sub(2);
    let range = state.palettes.window(height, names.len());
    let lines: Vec<Line> = if names.is_empty() {
        vec![Line::styled("No palettes in the repos.", dim())]
    } else {
        range
            .map(|index| {
                let name = &names[index];
                let marker = if Some(name.as_str()) == active {
                    "*"
                } else {
                    " "
                };
                Line::styled(
                    format!("{marker} {name}"),
                    cursor_style(
                        state.focus == DotPane::Palettes,
                        index == state.palettes.cursor,
                    ),
                )
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), palettes_area);

    // Variables.
    let names = app.var_names();
    let block = pane(
        format!(" Variables {} (e edits) ", names.len()),
        state.focus == DotPane::Vars,
    );
    let height = (vars_area.height as usize).saturating_sub(2);
    let range = state.vars.window(height, names.len());
    let workspace = &app.session.workspace;
    let lines: Vec<Line> = if names.is_empty() {
        vec![Line::styled("No template variables.", dim())]
    } else {
        range
            .map(|index| {
                let name = &names[index];
                let definition = &workspace.config.vars[name];
                let (value, source) = var_display(definition, workspace.host.vars.get(name));
                Line::styled(
                    format!("{name} = {value}  [{source}]"),
                    cursor_style(state.focus == DotPane::Vars, index == state.vars.cursor),
                )
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(lines).block(block), vars_area);
}

/// A variable's value as shown on screen (secrets stay hidden) and where it
/// came from.
pub fn var_display(definition: &VarDef, host_value: Option<&String>) -> (String, &'static str) {
    let (value, source) = match host_value {
        Some(value) => (Some(value.as_str()), "host"),
        None => (definition.default_value(), "default"),
    };
    match value {
        None => ("(unset)".into(), "needs a value"),
        Some(_) if definition.is_secret() => ("********".into(), source),
        Some(value) => (clean(value), source),
    }
}

// ----- sync -----------------------------------------------------------------

fn diff_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        bold()
    } else if line.starts_with('+') {
        green()
    } else if line.starts_with('-') {
        red()
    } else if line.starts_with("@@") {
        Style::default().add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
    }
}

fn draw_sync(frame: &mut Frame, area: Rect, app: &App) {
    let [list_area, diff_area] =
        Layout::horizontal([Constraint::Length(38), Constraint::Min(20)]).areas(area);
    let state = &app.sync;
    let Some((incoming, outgoing)) = &state.data else {
        frame.render_widget(
            Paragraph::new("Checking the repos... (this fetches from the remote; R retries)")
                .wrap(Wrap { trim: true })
                .block(pane(" Repos ", true)),
            area,
        );
        return;
    };
    let entries = state.entries();
    let height = (list_area.height as usize).saturating_sub(2);
    let range = state.list.window(height, entries.len());
    let lines: Vec<Line> = range
        .map(|index| {
            let text = match entries[index] {
                SyncEntry::Incoming(i) => {
                    let repo = &incoming[i];
                    let summary = if repo.problem.is_some() {
                        "problem".to_string()
                    } else if repo.commits.is_empty() {
                        "up to date".to_string()
                    } else {
                        format!("{} commit(s)", repo.commits.len())
                    };
                    format!("in  {:<10} {summary}", repo.root)
                }
                SyncEntry::Outgoing(i) => {
                    let repo = &outgoing[i];
                    let summary = if repo.problem.is_some() {
                        "problem".to_string()
                    } else if repo.status.is_empty() && repo.commits.is_empty() {
                        "clean".to_string()
                    } else {
                        format!(
                            "{} file(s), {} unpushed",
                            repo.status.len(),
                            repo.commits.len()
                        )
                    };
                    format!("out {:<10} {summary}", repo.root)
                }
            };
            Line::styled(
                clean(&text),
                if index == state.list.cursor {
                    reversed()
                } else {
                    Style::default()
                },
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(pane(" Repos (in: upstream, out: local) ", true)),
        list_area,
    );

    let block = Block::bordered().title(format!(
        " Diff {}/{} (J/K scroll) ",
        state.diff_scroll.min(state.diff_len().saturating_sub(1)) + 1,
        state.diff_len().max(1)
    ));
    let inner = block.inner(diff_area);
    frame.render_widget(block, diff_area);
    let Some((header, diff)) = state.view() else {
        frame.render_widget(Paragraph::new(Line::styled("No repos.", dim())), inner);
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    let visible = inner.height as usize;
    let scroll = state.diff_scroll;
    // Header lines, a blank line, then the diff; only the visible slice is built.
    let header_len = header.len() + 1;
    for (index, text) in header.iter().enumerate().skip(scroll).take(visible) {
        lines.push(Line::styled(
            clean(text),
            if index == 0 { bold() } else { Style::default() },
        ));
    }
    if scroll <= header.len() && lines.len() < visible {
        lines.push(Line::raw(""));
    }
    let diff_skip = scroll.saturating_sub(header_len);
    for text in diff
        .lines()
        .skip(diff_skip)
        .take(visible.saturating_sub(lines.len()))
    {
        lines.push(Line::styled(clean(text), diff_style(text)));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

// ----- settings -------------------------------------------------------------

fn draw_settings(frame: &mut Frame, area: Rect, app: &App) {
    let workspace = &app.session.workspace;
    let selection = &app.session.selection;
    let mut lines = vec![
        labeled("Host config", workspace.host_path.display().to_string()),
        labeled(
            "Platform",
            format!(
                "{} {}{}",
                workspace.platform.os,
                workspace.platform.arch,
                if workspace.platform.wsl { " (WSL)" } else { "" }
            ),
        ),
        labeled("Scope", workspace.paths.scope.to_string()),
        labeled(
            "Build mode",
            match workspace.build_mode() {
                crate::config::BuildMode::Prebuilt => "prebuilt",
                crate::config::BuildMode::Source => "source",
            },
        ),
        labeled(
            "Shells",
            if selection.shells.is_empty() {
                "(none)".into()
            } else {
                selection
                    .shells
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        ),
        labeled("Palette", workspace.palette_name().unwrap_or("(none)")),
        labeled(
            "Install prefix",
            workspace.paths.prefix.display().to_string(),
        ),
        Line::raw(""),
        Line::styled("Repos", bold()),
    ];
    for root in &workspace.roots {
        lines.push(Line::from(vec![
            Span::raw(format!(
                "  {:<10} {}  ",
                root.name,
                clean(&root.path.display().to_string())
            )),
            if root.present {
                Span::styled("present", green())
            } else {
                Span::styled("not cloned", red())
            },
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Scheduled update", bold()));
    match &app.settings.schedule {
        None => lines.push(Line::styled("  reading...", dim())),
        Some(status) if !status.installed => lines.push(Line::raw("  no scheduled update")),
        Some(status) => {
            lines.push(Line::raw(format!(
                "  schedule: {}",
                clean(status.schedule.as_deref().unwrap_or("unknown"))
            )));
            lines.push(Line::raw(format!(
                "  next run: {}",
                clean(status.next_run.as_deref().unwrap_or("unknown"))
            )));
            lines.push(Line::raw(format!(
                "  last result: {}",
                clean(status.last_result.as_deref().unwrap_or("unknown"))
            )));
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled("Config check", bold()));
    let problems = workspace.validate();
    if problems.is_empty() {
        lines.push(Line::styled(
            format!(
                "  valid: {} tools, {} presets, {} packages, {} palettes",
                workspace.config.tools.len(),
                workspace.config.presets.len(),
                workspace.config.packages.len(),
                workspace.palettes.len()
            ),
            green(),
        ));
    }
    for problem in problems {
        lines.push(Line::styled(
            format!("  problem: {}", clean(&problem)),
            red(),
        ));
    }
    for note in &workspace.notes {
        lines.push(Line::raw(format!("  note: {}", clean(note))));
    }
    let scroll = app.settings.scroll.min(lines.len().saturating_sub(1)) as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .block(Block::bordered().title(" Settings ")),
        area,
    );
}

// ----- modals ---------------------------------------------------------------

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let rows = [
        ("Everywhere", ""),
        ("  Tab, Shift-Tab, 1-5", "switch tabs"),
        ("  l", "resize the log pane"),
        ("  R", "refresh this tab"),
        ("  q, Esc", "quit (twice while a job runs)"),
        ("Tools", ""),
        ("  space, m", "mark a row, mark or clear all shown"),
        ("  a", "apply the plan (asks first)"),
        ("  r", "reinstall marked or highlighted tools"),
        ("  i", "install the highlighted unselected tool"),
        ("  u, b", "uninstall, roll back"),
        ("  n, /", "add a tool, filter by name"),
        ("Presets", ""),
        ("  space", "select or deselect on this host"),
        ("  n", "create a preset from the marked tools"),
        ("Dotfiles", ""),
        ("  d, c", "deploy, capture in-place edits"),
        ("  p, Enter", "next palette, pick the highlighted one"),
        ("  e, v", "edit a variable, show changes only"),
        ("  left, right", "switch pane"),
        ("Sync", ""),
        ("  s, U", "sync, update (asks first)"),
        ("  j/k, J/K", "pick a repo, scroll the diff"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(keys, what)| {
            if what.is_empty() {
                Line::styled(*keys, bold())
            } else {
                Line::from(vec![Span::raw(format!("{keys:<24}")), Span::raw(*what)])
            }
        })
        .collect();
    let rect = centered(area, 70, lines.len() as u16 + 3);
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Keys (any key closes) ")),
        rect,
    );
}

fn draw_confirm(frame: &mut Frame, area: Rect, confirm: &Confirm) {
    let height = (confirm.lines.len() as u16 + 4).min(area.height.saturating_sub(2));
    let rect = centered(area, area.width.saturating_sub(8).min(90), height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Thick)
        .title(format!(" {} ", clean(&confirm.title)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let lines: Vec<Line> = confirm
        .lines
        .iter()
        .map(|line| Line::styled(clean(line), diff_style(line)))
        .collect();
    frame.render_widget(
        Paragraph::new(lines).scroll((confirm.scroll as u16, 0)),
        body,
    );
    let hint = if confirm.request.is_some() {
        "y confirm   n or Esc cancel   j/k scroll"
    } else {
        "Enter or Esc closes   j/k scroll"
    };
    frame.render_widget(Paragraph::new(Line::styled(hint, bold())), footer);
}

fn input_spans(input: &Input, focused: bool, mask: bool) -> Vec<Span<'static>> {
    let chars: Vec<char> = input
        .text()
        .chars()
        .map(|character| if mask { '*' } else { character })
        .collect();
    if !focused {
        return vec![Span::raw(clean(&chars.iter().collect::<String>()))];
    }
    let cursor = input.cursor().min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    let at: String = chars.get(cursor).map_or(" ".into(), |c| c.to_string());
    let after: String = chars.iter().skip(cursor + 1).collect();
    vec![
        Span::raw(clean(&before)),
        Span::styled(clean(&at), reversed()),
        Span::raw(clean(&after)),
    ]
}

fn draw_form(frame: &mut Frame, area: Rect, form: &Form) {
    let mut lines: Vec<Line> = Vec::new();
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        let mut spans = vec![Span::styled(
            format!("{} {:<22} ", if focused { ">" } else { " " }, field.label),
            if focused { bold() } else { Style::default() },
        )];
        match &field.kind {
            FieldKind::Text => spans.extend(input_spans(&field.input, focused, false)),
            FieldKind::Secret => spans.extend(input_spans(&field.input, focused, true)),
            FieldKind::Choice { options, selected } => {
                spans.push(Span::raw(format!("< {} >", options[*selected])));
            }
            FieldKind::Check(value) => {
                spans.push(Span::raw(if *value { "[x]" } else { "[ ]" }));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    if let Some(error) = &form.error {
        lines.push(Line::styled(
            clean(error),
            red().add_modifier(Modifier::BOLD),
        ));
    } else {
        lines.push(Line::styled(clean(&form.fields[form.focus].hint), dim()));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Enter submit   Esc cancel   Tab next field",
        bold(),
    ));
    let rect = centered(
        area,
        area.width.saturating_sub(8).min(86),
        lines.len() as u16 + 2,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::bordered()
                .border_type(BorderType::Thick)
                .title(format!(" {} ", clean(&form.title))),
        ),
        rect,
    );
}

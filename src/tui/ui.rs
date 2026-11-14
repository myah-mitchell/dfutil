//! Rendering. Every draw also refills the app's hit map, so a mouse click
//! lands on whatever the last frame showed at that cell.

use super::app::{App, Button, Confirm, Modal, Tone, capture_preview, change_verb};
use super::input::{FieldKind, Form, Input};
use super::mouse::Hit;
use super::state::*;
use crate::config::VarDef;
use crate::deploy::Change;
use crate::plan::ToolRow;
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use std::collections::BTreeSet;
use std::path::Path;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ACCENT: Color = Color::Magenta;
/// Below this width the detail pane goes under the panels instead of beside them.
const SIDE_BY_SIDE: u16 = 90;

fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn fg(color: Color) -> Style {
    Style::default().fg(color)
}

fn accent() -> Style {
    fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// The highlighted row of a list: a bar across the row when its panel has
/// focus, bold text when it doesn't.
fn cursor_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        bold()
    }
}

fn pill(text: &str, color: Color) -> Span<'static> {
    Span::styled(
        format!(" {text} "),
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD),
    )
}

fn status_color(status: Status) -> Color {
    match status {
        Status::Current => Color::Green,
        Status::Upgrade => Color::Yellow,
        Status::Install | Status::Reinstall => Color::Blue,
        Status::Remove | Status::Failed => Color::Red,
        Status::Held => Color::Cyan,
        Status::Off => Color::Gray,
    }
}

fn status_style(status: Status) -> Style {
    if status == Status::Off {
        dim()
    } else {
        fg(status_color(status))
    }
}

fn width_of(spans: &[Span]) -> u16 {
    spans.iter().map(|span| span.width() as u16).sum()
}

/// Cuts `text` to `max` columns, ending with `…` when it was cut.
fn fit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Word-wraps prose to `width` columns.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = Vec::new();
    for paragraph in text.lines() {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let needed =
                line.chars().count() + usize::from(!line.is_empty()) + word.chars().count();
            if needed > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

fn tilde(home: &Path, path: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(relative) => format!("~/{}", relative.display()),
        Err(_) => path.display().to_string(),
    }
}

fn panel_block(app: &App, panel: Panel, extra: Vec<Span<'static>>) -> Block<'static> {
    let focused = app.focus == panel;
    let mut title = vec![
        Span::raw(" "),
        Span::styled(
            format!("{}", panel.index() + 1),
            if focused { accent() } else { dim() },
        ),
        Span::raw(" "),
        Span::styled(
            panel.title(),
            if focused { bold() } else { Style::default() },
        ),
        Span::raw(" "),
    ];
    if !extra.is_empty() {
        title.extend(extra);
        title.push(Span::raw(" "));
    }
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused {
            fg(ACCENT)
        } else {
            fg(Color::DarkGray)
        })
        .title(Line::from(title))
}

// ----- frame ----------------------------------------------------------------

pub fn draw(frame: &mut Frame, app: &App) {
    app.hits.clear();
    let area = frame.area();
    if area.width < 30 || area.height < 8 {
        frame.render_widget(
            Paragraph::new("dfutil needs a terminal of at least 30x8"),
            area,
        );
        return;
    }
    let [top, body, strip, hints] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(4),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    draw_top(frame, top, app);
    if body.width >= SIDE_BY_SIDE {
        let left = (body.width * 42 / 100)
            .clamp(44, 70)
            .min(body.width.saturating_sub(30));
        let [panels, detail] =
            Layout::horizontal([Constraint::Length(left), Constraint::Min(20)]).areas(body);
        draw_panels(frame, panels, app);
        draw_detail(frame, detail, app);
    } else {
        let detail_height = (body.height * 2 / 5).max(6);
        let [panels, detail] =
            Layout::vertical([Constraint::Min(6), Constraint::Length(detail_height)]).areas(body);
        draw_panels(frame, panels, app);
        draw_detail(frame, detail, app);
    }
    draw_strip(frame, strip, app);
    draw_hints(frame, hints, app);

    if let Some(modal) = &app.modal {
        // Only the dialog is clickable while one is open.
        app.hits.clear();
        app.hits.add(area, Hit::Backdrop);
        match modal {
            Modal::Help { scroll } => draw_help(frame, area, app, *scroll),
            Modal::Log { scroll } => draw_log(frame, area, app, *scroll),
            Modal::Confirm(confirm) => draw_confirm(frame, area, app, confirm),
            Modal::Form(form) => draw_form(frame, area, app, form),
        }
    }
}

/// Lays out spans left to right on one line, registering each clickable run.
struct Row<'a> {
    app: &'a App,
    area: Rect,
    spans: Vec<Span<'static>>,
    x: u16,
}

impl<'a> Row<'a> {
    fn new(app: &'a App, area: Rect) -> Self {
        Self {
            app,
            area,
            spans: Vec::new(),
            x: area.x,
        }
    }

    fn room(&self) -> u16 {
        (self.area.x + self.area.width).saturating_sub(self.x)
    }

    /// Adds spans as one unit, clickable as `hit`; false when they don't fit.
    fn push(&mut self, spans: Vec<Span<'static>>, hit: Option<Hit>) -> bool {
        let width = width_of(&spans);
        if width > self.room() {
            return false;
        }
        if let Some(hit) = hit {
            self.app
                .hits
                .add(Rect::new(self.x, self.area.y, width, 1), hit);
        }
        self.x += width;
        self.spans.extend(spans);
        true
    }

    /// Adds spans flush with the right edge, if they fit.
    fn push_right(&mut self, spans: Vec<Span<'static>>, hit: Option<Hit>) {
        let width = width_of(&spans);
        if width > self.room() {
            return;
        }
        let gap = self.room() - width;
        self.push(vec![Span::raw(" ".repeat(gap as usize))], None);
        self.push(spans, hit);
    }

    fn render(self, frame: &mut Frame) {
        frame.render_widget(Paragraph::new(Line::from(self.spans)), self.area);
    }
}

fn draw_top(frame: &mut Frame, area: Rect, app: &App) {
    let workspace = &app.session.workspace;
    let mut row = Row::new(app, area);
    row.push(vec![pill("dfutil", ACCENT), Span::raw(" ")], None);
    let presets = &workspace.host.presets;
    row.push(
        vec![
            Span::styled(" presets ", dim()),
            Span::raw(if presets.is_empty() {
                "none".to_string()
            } else {
                clean(&presets.join(" "))
            }),
            Span::raw("  "),
        ],
        Some(Hit::Focus(Panel::Presets)),
    );
    if let Some(palette) = workspace.palette_name() {
        row.push(
            vec![
                Span::styled("palette ", dim()),
                Span::styled(clean(palette), fg(ACCENT)),
                Span::styled(" ▾", dim()),
                Span::raw("  "),
            ],
            Some(Hit::Palette),
        );
    }
    let platform = &workspace.platform;
    row.push(
        vec![
            Span::styled(
                format!(
                    "{} {}{}",
                    platform.os,
                    platform.arch,
                    if platform.wsl { " (WSL)" } else { "" }
                ),
                dim(),
            ),
            Span::raw("  "),
        ],
        None,
    );
    let schedule = match &app.repos.schedule {
        Some(status) if status.installed => format!(
            "auto-update {}",
            clean(status.schedule.as_deref().unwrap_or("on"))
        ),
        Some(_) => "auto-update off".to_string(),
        None => String::new(),
    };
    if !schedule.is_empty() {
        let hit = Some(Hit::RepoRow(app.repo_rows().len() - 1));
        row.push(vec![Span::styled(schedule, dim()), Span::raw("  ")], hit);
    }
    row.push_right(
        vec![Span::styled("?", accent()), Span::styled(" help ", dim())],
        Some(Hit::Key(KeyCode::Char('?'))),
    );
    row.render(frame);
}

// ----- panels ---------------------------------------------------------------

fn draw_panels(frame: &mut Frame, area: Rect, app: &App) {
    let inner_width = area.width.saturating_sub(2);
    let half = area.height / 2;
    let size = |panel: Panel, rows: usize, collapsed: u16| -> u16 {
        let rows = rows.max(1) as u16;
        let wanted = if app.focus == panel {
            rows.min(half.saturating_sub(2).max(1))
        } else {
            rows.min(collapsed)
        };
        wanted + 2
    };
    let chips = preset_layout(app, inner_width);
    let presets = size(Panel::Presets, chips.len(), 2);
    let dotfiles = size(Panel::Dotfiles, app.dot_rows().len(), 3);
    let repos = size(Panel::Repos, app.repo_rows().len(), 3);
    let [tools_area, presets_area, dotfiles_area, repos_area] = Layout::vertical([
        Constraint::Min(5),
        Constraint::Length(presets),
        Constraint::Length(dotfiles),
        Constraint::Length(repos),
    ])
    .areas(area);
    draw_tools(frame, tools_area, app);
    draw_presets(frame, presets_area, app, &chips);
    draw_dotfiles(frame, dotfiles_area, app);
    draw_repos(frame, repos_area, app);
}

/// Pads a highlighted line so the cursor bar spans the whole row.
fn highlight(line: Line<'static>, focused: bool, width: usize) -> Line<'static> {
    let mut line = line.patch_style(cursor_style(focused));
    let used = line.width();
    if used < width {
        line.spans.push(Span::styled(
            " ".repeat(width - used),
            cursor_style(focused),
        ));
    }
    line
}

fn draw_tools(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.tools;
    let focused = app.focus == Panel::Tools;
    let rows = state.rows();
    let mut extra = Vec::new();
    if let Some(plan) = &state.plan {
        extra.push(Span::styled(format!("{}", rows.len()), dim()));
        let changes = plan.changes().count();
        if changes > 0 {
            extra.push(Span::styled(
                format!("  {changes} to apply"),
                fg(Color::Yellow),
            ));
        }
        if !state.marked.is_empty() {
            extra.push(Span::styled(
                format!("  {} marked", state.marked.len()),
                fg(ACCENT),
            ));
        }
    }
    let block = panel_block(app, Panel::Tools, extra);
    let inner = block.inner(area);
    app.hits.add(area, Hit::Focus(Panel::Tools));
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let [bar, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

    // Grouping switch and filter.
    let mut row = Row::new(app, bar);
    row.push(vec![Span::styled("group ", dim())], None);
    for grouping in Grouping::ALL {
        let style = if grouping == state.grouping {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            dim()
        };
        row.push(
            vec![Span::styled(format!(" {} ", grouping.label()), style)],
            Some(Hit::Grouping(grouping)),
        );
    }
    row.push(vec![Span::raw("  ")], None);
    let filter: Vec<Span<'static>> = if state.filtering {
        let mut spans = vec![Span::styled("/", accent())];
        spans.extend(input_spans(&state.filter, true, false));
        spans
    } else if !state.filter.is_empty() {
        vec![Span::styled(
            format!("/{}", clean(state.filter.text())),
            fg(Color::Yellow),
        )]
    } else {
        vec![Span::styled("/ filter", dim())]
    };
    row.push(filter, Some(Hit::Filter));
    row.render(frame);

    if list.height == 0 {
        return;
    }
    if state.plan.is_none() {
        let text = match &state.error {
            Some(error) => Line::styled(
                format!("Version lookup failed: {}  (R retries)", clean(error)),
                fg(Color::Red),
            ),
            None => Line::styled("Looking up tool versions...", dim()),
        };
        frame.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), list);
        return;
    }
    let items = app.tool_items();
    if items.is_empty() {
        let text = if state.filter.is_empty() {
            "This config defines no tools. Press n to add one."
        } else {
            "No tool name matches the filter. Esc clears it."
        };
        frame.render_widget(Paragraph::new(Line::styled(text, dim())), list);
        return;
    }
    let range = state.list.window(list.height as usize, items.len());
    let width = list.width as usize;
    let mut lines = Vec::new();
    for (line, index) in range.enumerate() {
        let y = list.y + line as u16;
        app.hits
            .add(Rect::new(list.x, y, list.width, 1), Hit::ToolItem(index));
        let out = match &items[index] {
            ToolItem::Header {
                label,
                count,
                attention,
                active,
                open,
                ..
            } => {
                let mut spans = vec![
                    Span::styled(if *open { "▾ " } else { "▸ " }, fg(ACCENT)),
                    Span::styled(clean(label), bold()),
                    Span::styled(format!(" {count}"), dim()),
                ];
                if *attention > 0 && state.grouping != Grouping::Status {
                    spans.push(Span::styled(
                        format!("  {attention} to apply"),
                        fg(Color::Yellow),
                    ));
                }
                match active {
                    Some(true) => spans.push(Span::styled("  on", fg(Color::Green))),
                    Some(false) => spans.push(Span::styled("  off", dim())),
                    None => {}
                }
                Line::from(spans)
            }
            ToolItem::Tool(row) => {
                let row = &rows[*row];
                app.hits
                    .add(Rect::new(list.x + 2, y, 3, 1), Hit::ToolMark(index));
                tool_line(row, state.marked.contains(&row.name), width)
            }
        };
        lines.push(if index == state.list.cursor {
            highlight(out, focused, width)
        } else {
            out
        });
    }
    frame.render_widget(Paragraph::new(lines), list);
}

fn version_text(row: &ToolRow, status: Status) -> String {
    let installed = clean(row.installed.as_deref().unwrap_or("-"));
    let available = clean(row.available.as_deref().unwrap_or("?"));
    match status {
        Status::Upgrade => format!("{installed} → {available}"),
        Status::Install => format!("→ {available}"),
        Status::Failed => "lookup failed".into(),
        Status::Held => format!("{installed}, held"),
        Status::Off => row.available.as_deref().map(clean).unwrap_or_default(),
        Status::Reinstall | Status::Remove | Status::Current => installed,
    }
}

fn tool_line(row: &ToolRow, marked: bool, width: usize) -> Line<'static> {
    let status = Status::of(row);
    let version = fit(&version_text(row, status), width / 2);
    // "  [x] ↑ " is 8 columns; keep one space before the version.
    let name_width = width.saturating_sub(8 + version.chars().count() + 1);
    let name = fit(&clean(&row.name), name_width);
    let pad = name_width.saturating_sub(name.chars().count());
    Line::from(vec![
        Span::raw("  "),
        if marked {
            Span::styled("[x]", fg(ACCENT).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("[ ]", dim())
        },
        Span::raw(" "),
        Span::styled(status.glyph(), status_style(status)),
        Span::raw(" "),
        Span::styled(
            name,
            if status == Status::Off {
                dim()
            } else {
                Style::default()
            },
        ),
        Span::raw(" ".repeat(pad + 1)),
        Span::styled(version, status_style(status)),
    ])
}

/// How a preset reads in its chip: on here, pulled in by another, or off.
fn preset_marker(app: &App, name: &str, active: &BTreeSet<String>) -> &'static str {
    if app
        .session
        .workspace
        .host
        .presets
        .iter()
        .any(|preset| preset == name)
    {
        "[x]"
    } else if active.contains(name) {
        "[+]"
    } else {
        "[ ]"
    }
}

fn chip_width(app: &App, name: &str) -> u16 {
    let tools = app.session.workspace.config.presets[name].tools.len();
    // "[x]" " name " "12 " and one space between chips.
    (3 + 1 + name.chars().count() + 1 + tools.to_string().len() + 1 + 1) as u16
}

/// Preset chips wrapped into rows that fit `width`.
fn preset_layout(app: &App, width: u16) -> Vec<Vec<usize>> {
    let mut rows: Vec<Vec<usize>> = Vec::new();
    let mut used = 0;
    for (index, name) in app.preset_names().iter().enumerate() {
        let chip = chip_width(app, name);
        if rows.is_empty() || used + chip > width {
            rows.push(Vec::new());
            used = 0;
        }
        rows.last_mut().expect("pushed above").push(index);
        used += chip;
    }
    rows
}

fn draw_presets(frame: &mut Frame, area: Rect, app: &App, chips: &[Vec<usize>]) {
    let focused = app.focus == Panel::Presets;
    let names = app.preset_names();
    let active = app.active_presets();
    let on = app.session.workspace.host.presets.len();
    let block = panel_block(
        app,
        Panel::Presets,
        vec![Span::styled(format!("{on} of {} on", names.len()), dim())],
    );
    let inner = block.inner(area);
    app.hits.add(area, Hit::Focus(Panel::Presets));
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    if names.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("This config defines no presets.", dim())),
            inner,
        );
        return;
    }
    // Scroll whole rows so the highlighted chip stays visible.
    let cursor_row = chips
        .iter()
        .position(|row| row.contains(&app.presets.list.cursor))
        .unwrap_or(0);
    let height = inner.height as usize;
    let first = cursor_row.saturating_sub(height - 1);
    for (line, chip_row) in chips.iter().skip(first).take(height).enumerate() {
        let rect = Rect::new(inner.x, inner.y + line as u16, inner.width, 1);
        let mut row = Row::new(app, rect);
        for index in chip_row {
            let name = &names[*index];
            let marker = preset_marker(app, name, &active);
            let base = if *index == app.presets.list.cursor {
                cursor_style(focused)
            } else {
                Style::default()
            };
            let marker_style = match marker {
                "[x]" => base.fg(Color::Green).add_modifier(Modifier::BOLD),
                "[+]" => base.fg(Color::Cyan),
                _ => base.add_modifier(Modifier::DIM),
            };
            let tools = app.session.workspace.config.presets[name].tools.len();
            row.push(
                vec![Span::styled(marker, marker_style)],
                Some(Hit::PresetToggle(*index)),
            );
            row.push(
                vec![
                    Span::styled(format!(" {} ", clean(name)), base),
                    Span::styled(format!("{tools} "), base.add_modifier(Modifier::DIM)),
                ],
                Some(Hit::Preset(*index)),
            );
            row.push(vec![Span::raw(" ")], None);
        }
        row.render(frame);
    }
}

fn change_style(change: Change) -> Style {
    match change {
        Change::Unchanged | Change::Skipped => dim(),
        Change::Error => fg(Color::Red).add_modifier(Modifier::BOLD),
        Change::BackedUp | Change::Pruned => fg(Color::Yellow),
        Change::Linked | Change::Rerendered => fg(Color::Blue),
    }
}

fn dot_line(app: &App, row: &DotRow, width: usize) -> Line<'static> {
    let state = &app.dotfiles;
    let workspace = &app.session.workspace;
    let home = &workspace.paths.home;
    match row {
        DotRow::Problem(index) => Line::from(vec![
            Span::styled("✗ ", fg(Color::Red)),
            Span::styled(
                fit(&clean(&state.problems[*index]), width.saturating_sub(2)),
                fg(Color::Red),
            ),
        ]),
        DotRow::Edited(index) => {
            let edit = &state.edited[*index];
            Line::from(vec![
                Span::styled("✎ ", fg(Color::Yellow)),
                Span::raw(fit(
                    &clean(&tilde(home, &edit.target)),
                    width.saturating_sub(20),
                )),
                Span::styled("  edited in place", fg(Color::Yellow)),
            ])
        }
        DotRow::Change(index) => {
            let item = &state.items.as_ref().expect("rows come from items")[*index];
            let verb = change_verb(item.change);
            Line::from(vec![
                Span::styled("↻ ", change_style(item.change)),
                Span::raw(fit(
                    &clean(&tilde(home, &item.target)),
                    width.saturating_sub(verb.len() + 4),
                )),
                Span::styled(format!("  {verb}"), change_style(item.change)),
            ])
        }
        DotRow::Current(count) => Line::from(vec![
            Span::styled("● ", fg(Color::Green)),
            Span::raw(format!("{count} file(s) deployed and current")),
        ]),
        DotRow::Palette => Line::from(vec![
            Span::styled("◆ ", fg(ACCENT)),
            Span::raw("palette  "),
            Span::styled(
                clean(workspace.palette_name().unwrap_or("(none)")),
                fg(ACCENT),
            ),
            Span::styled(format!("  of {}", workspace.palettes.len()), dim()),
        ]),
        DotRow::Var(name) => {
            let definition = &workspace.config.vars[name];
            let (value, source) = var_display(definition, workspace.host.vars.get(name));
            Line::from(vec![
                Span::styled("$ ", dim()),
                Span::raw(format!("{name} = ")),
                Span::raw(fit(&value, width.saturating_sub(name.len() + 20))),
                Span::styled(
                    format!("  {source}"),
                    if source == "needs a value" {
                        fg(Color::Yellow)
                    } else {
                        dim()
                    },
                ),
            ])
        }
    }
}

/// Draws a list panel's rows, registering a hit per row.
fn draw_rows(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    scroller: &Scroller,
    lines: Vec<Line<'static>>,
    focused: bool,
    hit: fn(usize) -> Hit,
) {
    let range = scroller.window(area.height as usize, lines.len());
    let width = area.width as usize;
    let mut shown = Vec::new();
    for (line, index) in range.enumerate() {
        app.hits.add(
            Rect::new(area.x, area.y + line as u16, area.width, 1),
            hit(index),
        );
        let out = lines[index].clone();
        shown.push(if index == scroller.cursor {
            highlight(out, focused, width)
        } else {
            out
        });
    }
    frame.render_widget(Paragraph::new(shown), area);
}

fn draw_dotfiles(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.dotfiles;
    let mut extra = Vec::new();
    match &state.items {
        None => extra.push(Span::styled("checking...", dim())),
        Some(items) => {
            let pending = items
                .iter()
                .filter(|item| !matches!(item.change, Change::Unchanged | Change::Skipped))
                .count();
            if pending > 0 {
                extra.push(Span::styled(
                    format!("{pending} to deploy"),
                    fg(Color::Blue),
                ));
            } else {
                extra.push(Span::styled("deployed", fg(Color::Green)));
            }
            if !state.edited.is_empty() {
                extra.push(Span::styled(
                    format!("  {} edited", state.edited.len()),
                    fg(Color::Yellow),
                ));
            }
        }
    }
    let block = panel_block(app, Panel::Dotfiles, extra);
    let inner = block.inner(area);
    app.hits.add(area, Hit::Focus(Panel::Dotfiles));
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let lines: Vec<Line<'static>> = app
        .dot_rows()
        .iter()
        .map(|row| dot_line(app, row, width))
        .collect();
    if lines.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("Nothing to show.", dim())),
            inner,
        );
        return;
    }
    draw_rows(
        frame,
        inner,
        app,
        &state.list,
        lines,
        app.focus == Panel::Dotfiles,
        Hit::DotRow,
    );
}

fn repo_line(app: &App, row: &RepoRow) -> Line<'static> {
    match row {
        RepoRow::Root(index) => {
            let root = &app.session.workspace.roots[*index];
            let name = Span::raw(format!("{:<10}", clean(&root.name)));
            if !root.present {
                return Line::from(vec![
                    Span::styled("○ ", dim()),
                    name,
                    Span::styled(" not cloned", fg(Color::Yellow)),
                ]);
            }
            let incoming = app.repos.incoming(&root.name);
            let outgoing = app.repos.outgoing(&root.name);
            let problem = incoming.is_some_and(|repo| repo.problem.is_some())
                || outgoing.is_some_and(|repo| repo.problem.is_some());
            let mut spans = vec![
                if problem {
                    Span::styled("✗ ", fg(Color::Red))
                } else {
                    Span::styled("● ", fg(Color::Green))
                },
                name,
            ];
            if app.repos.data.is_none() {
                spans.push(Span::styled(" checking...", dim()));
                return Line::from(spans);
            }
            let count = |glyph: &str, count: usize, color: Color| {
                Span::styled(
                    format!(" {glyph}{count}"),
                    if count > 0 { fg(color) } else { dim() },
                )
            };
            spans.push(count(
                "↓",
                incoming.map_or(0, |repo| repo.commits.len()),
                Color::Blue,
            ));
            spans.push(count(
                "↑",
                outgoing.map_or(0, |repo| repo.commits.len()),
                Color::Yellow,
            ));
            spans.push(count(
                "±",
                outgoing.map_or(0, |repo| repo.status.len()),
                Color::Yellow,
            ));
            if problem {
                spans.push(Span::styled("  problem", fg(Color::Red)));
            }
            Line::from(spans)
        }
        RepoRow::Schedule => {
            let text = match &app.repos.schedule {
                None => Span::styled("reading...", dim()),
                Some(status) if !status.installed => Span::styled("off", dim()),
                Some(status) => Span::raw(clean(status.schedule.as_deref().unwrap_or("on"))),
            };
            Line::from(vec![
                Span::styled("◷ ", fg(ACCENT)),
                Span::raw(format!("{:<10}", "schedule")),
                text,
            ])
        }
    }
}

fn draw_repos(frame: &mut Frame, area: Rect, app: &App) {
    let mut extra = Vec::new();
    if let Some((incoming, _)) = &app.repos.data {
        let down: usize = incoming.iter().map(|repo| repo.commits.len()).sum();
        if down > 0 {
            extra.push(Span::styled(format!("↓{down} incoming"), fg(Color::Blue)));
        }
    }
    let block = panel_block(app, Panel::Repos, extra);
    let inner = block.inner(area);
    app.hits.add(area, Hit::Focus(Panel::Repos));
    frame.render_widget(block, area);
    let lines = app
        .repo_rows()
        .iter()
        .map(|row| repo_line(app, row))
        .collect();
    draw_rows(
        frame,
        inner,
        app,
        &app.repos.list,
        lines,
        app.focus == Panel::Repos,
        Hit::RepoRow,
    );
}

// ----- detail pane ------------------------------------------------------------

struct Detail {
    title: String,
    pill: Option<(String, Color)>,
    lines: Vec<Line<'static>>,
}

impl Detail {
    fn new(title: impl Into<String>) -> Self {
        Self {
            title: clean(&title.into()),
            pill: None,
            lines: Vec::new(),
        }
    }

    fn pill(mut self, text: impl Into<String>, color: Color) -> Self {
        self.pill = Some((clean(&text.into()), color));
        self
    }

    fn blank(&mut self) {
        self.lines.push(Line::raw(""));
    }

    fn heading(&mut self, text: &str) {
        self.lines.push(Line::styled(text.to_string(), accent()));
    }

    fn prose(&mut self, text: &str, width: usize, style: Style) {
        for line in wrap(&clean(text), width) {
            self.lines.push(Line::styled(line, style));
        }
    }

    /// A label and value, the value wrapped under itself when long.
    fn field(&mut self, label: &str, value: impl Into<String>, width: usize, style: Style) {
        const LABEL: usize = 13;
        let value = clean(&value.into());
        let wrapped = wrap(&value, width.saturating_sub(LABEL));
        for (index, part) in wrapped.into_iter().enumerate() {
            let label = if index == 0 { label } else { "" };
            self.lines.push(Line::from(vec![
                Span::styled(format!("{label:<LABEL$}"), dim()),
                Span::styled(part, style),
            ]));
        }
    }

    fn diff(&mut self, text: &str) {
        for line in text.lines() {
            self.lines.push(Line::styled(clean(line), diff_style(line)));
        }
    }
}

fn draw_detail(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width.saturating_sub(4) as usize;
    let detail = match app.focus {
        Panel::Tools => tools_detail(app, width),
        Panel::Presets => preset_detail(app, width),
        Panel::Dotfiles => dotfiles_detail(app, width),
        Panel::Repos => repos_detail(app, width),
    };
    let mut title = vec![
        Span::raw(" "),
        Span::styled(detail.title.clone(), bold()),
        Span::raw(" "),
    ];
    if let Some((text, color)) = &detail.pill {
        title.push(pill(text, *color));
        title.push(Span::raw(" "));
    }
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(Color::DarkGray))
        .title(Line::from(title));
    let inner = block.inner(area).inner(Margin::new(1, 0));
    app.hits.add(area, Hit::Detail);
    frame.render_widget(block, area);
    let actions = app.detail_actions();
    let [body, buttons] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(if actions.is_empty() { 0 } else { 2 }),
    ])
    .areas(inner);
    let height = body.height as usize;
    let max = detail.lines.len().saturating_sub(height);
    app.detail_max.set(max);
    let scroll = app.detail_scroll.min(max);
    let mut lines: Vec<Line> = detail.lines.into_iter().skip(scroll).take(height).collect();
    if max > 0
        && let Some(last) = lines.last_mut()
    {
        // Say there's more, on the last visible line.
        *last = Line::styled(
            format!(
                "{}/{}  J/K or the wheel scrolls",
                scroll + height,
                max + height
            ),
            dim(),
        );
    }
    frame.render_widget(Paragraph::new(lines), body);
    if !actions.is_empty() {
        let row = Rect::new(buttons.x, buttons.y + 1, buttons.width, 1);
        draw_buttons(frame, row, app, &actions);
    }
}

fn button_spans(button: &Button) -> Vec<Span<'static>> {
    let style = if button.primary {
        Style::default()
            .fg(Color::Black)
            .bg(ACCENT)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::DarkGray)
    };
    let key_style = if button.primary {
        style
    } else {
        style.fg(ACCENT).add_modifier(Modifier::BOLD)
    };
    vec![
        Span::styled(format!(" {} ", button.keys), key_style),
        Span::styled(format!("{} ", button.label), style),
    ]
}

fn draw_buttons(frame: &mut Frame, area: Rect, app: &App, buttons: &[Button]) {
    let mut row = Row::new(app, area);
    for button in buttons {
        if !row.push(button_spans(button), Some(Hit::Key(button.code))) {
            break;
        }
        row.push(vec![Span::raw(" ")], None);
    }
    row.render(frame);
}

fn tools_detail(app: &App, width: usize) -> Detail {
    let state = &app.tools;
    if state.plan.is_none() {
        let mut detail = Detail::new("Tools");
        match &state.error {
            Some(error) => detail.prose(
                &format!("The version lookup failed: {error}. Press R to try again."),
                width,
                fg(Color::Red),
            ),
            None => detail.prose(
                "Looking up installed and available versions...",
                width,
                dim(),
            ),
        }
        return detail;
    }
    match app.current_item() {
        None => {
            let mut detail = Detail::new("Tools");
            detail.prose("Nothing selected.", width, dim());
            detail
        }
        Some(ToolItem::Header {
            label,
            count,
            attention,
            active,
            ..
        }) => {
            let mut detail = Detail::new(label);
            match active {
                Some(true) => detail = detail.pill("on here", Color::Green),
                Some(false) => detail = detail.pill("off here", Color::Gray),
                None => {}
            }
            detail.field("Tools", count.to_string(), width, Style::default());
            if attention > 0 {
                detail.field("To apply", attention.to_string(), width, fg(Color::Yellow));
            }
            detail.blank();
            let items = app.tool_items();
            let start = (app.tools.list.cursor + 1).min(items.len());
            let members: Vec<usize> = items[start..]
                .iter()
                .map_while(|item| match item {
                    ToolItem::Tool(index) => Some(*index),
                    ToolItem::Header { .. } => None,
                })
                .collect();
            if members.is_empty() {
                detail.prose("Space or a click opens the group.", width, dim());
            }
            for index in members {
                let row = &state.rows()[index];
                let status = Status::of(row);
                detail.lines.push(Line::from(vec![
                    Span::styled(format!("{} ", status.glyph()), status_style(status)),
                    Span::raw(format!("{:<20} ", fit(&clean(&row.name), 20))),
                    Span::styled(status.label(), dim()),
                ]));
            }
            detail
        }
        Some(ToolItem::Tool(index)) => tool_detail(app, &state.rows()[index], width),
    }
}

fn tool_detail(app: &App, row: &ToolRow, width: usize) -> Detail {
    let status = Status::of(row);
    let mut detail = Detail::new(row.name.clone()).pill(status.label(), status_color(status));
    if let Some(description) = &row.description {
        detail.prose(description, width, Style::default());
        detail.blank();
    }
    if let Some(error) = &row.error {
        detail.prose(&format!("Error: {error}"), width, fg(Color::Red));
        detail.blank();
    }
    detail.field(
        "Installed",
        match (&row.installed, row.installed_method) {
            (Some(version), Some(method)) => format!("{version} ({method})"),
            (Some(version), None) => version.clone(),
            _ => "no".into(),
        },
        width,
        Style::default(),
    );
    detail.field(
        "Available",
        match (&row.available, row.method) {
            (Some(version), Some(method)) => format!("{version} ({method})"),
            (Some(version), None) => version.clone(),
            _ => "unknown".into(),
        },
        width,
        status_style(status),
    );
    detail.field("Pin", row.pin.clone(), width, Style::default());
    detail.field(
        "Plan",
        format!("{} ({})", action_label(row.action), row.why),
        width,
        Style::default(),
    );
    detail.field(
        "Selected",
        if row.selected {
            format!("yes, by {}", row.reasons.join(", "))
        } else {
            "no".into()
        },
        width,
        Style::default(),
    );
    if let Some(category) = &row.category {
        detail.field("Category", category.clone(), width, Style::default());
    }
    if row.installed.is_some() {
        detail.field(
            "Checksum",
            if row.verified {
                "verified against a published checksum"
            } else {
                "not verified"
            },
            width,
            Style::default(),
        );
    }
    if row.has_previous {
        detail.field(
            "Rollback",
            "a previous version is kept",
            width,
            Style::default(),
        );
    }
    for note in &row.notes {
        detail.field("Note", note.clone(), width, dim());
    }
    if app.tools.marked.contains(&row.name) {
        detail.blank();
        detail.prose(
            &format!(
                "Marked, with {} other(s). u, r, and x act on every marked tool.",
                app.tools.marked.len() - 1
            ),
            width,
            fg(ACCENT),
        );
    }
    detail
}

fn preset_detail(app: &App, width: usize) -> Detail {
    let Some(name) = app.current_preset() else {
        let mut detail = Detail::new("Presets");
        detail.prose("This config defines no presets.", width, dim());
        return detail;
    };
    let config = &app.session.workspace.config;
    let preset = &config.presets[&name];
    let active = app.active_presets();
    let mut detail = match preset_marker(app, &name, &active) {
        "[x]" => Detail::new(name.clone()).pill("on here", Color::Green),
        "[+]" => Detail::new(name.clone()).pill("pulled in by another", Color::Cyan),
        _ => Detail::new(name.clone()).pill("off", Color::Gray),
    };
    if let Some(description) = &preset.description {
        detail.prose(description, width, Style::default());
        detail.blank();
    }
    if !preset.extends.is_empty() {
        detail.field(
            "Extends",
            preset.extends.join(", "),
            width,
            Style::default(),
        );
    }
    if !preset.packages.is_empty() {
        detail.field(
            "Packages",
            preset.packages.join(", "),
            width,
            Style::default(),
        );
    }
    if !preset.shells.is_empty() {
        detail.field("Shells", preset.shells.join(", "), width, Style::default());
    }
    detail.blank();
    detail.heading(&format!("Tools ({})", preset.tools.len()));
    for tool in &preset.tools {
        match app.tools.row(tool) {
            Some(row) => {
                let status = Status::of(row);
                detail.lines.push(Line::from(vec![
                    Span::styled(format!("{} ", status.glyph()), status_style(status)),
                    Span::raw(format!("{:<20} ", fit(&clean(tool), 20))),
                    Span::styled(version_text(row, status), dim()),
                ]));
            }
            None => detail
                .lines
                .push(Line::styled(format!("  {}", clean(tool)), dim())),
        }
    }
    detail
}

fn dotfiles_detail(app: &App, width: usize) -> Detail {
    let state = &app.dotfiles;
    let workspace = &app.session.workspace;
    let home = &workspace.paths.home;
    let Some(row) = app.current_dot() else {
        let mut detail = Detail::new("Dotfiles");
        detail.prose(
            if state.items.is_none() {
                "Checking what a deploy would change..."
            } else {
                "No dotfiles, palettes, or variables in this config."
            },
            width,
            dim(),
        );
        return detail;
    };
    match row {
        DotRow::Problem(index) => {
            let mut detail = Detail::new("Problem").pill("error", Color::Red);
            detail.prose(&state.problems[index], width, fg(Color::Red));
            detail
        }
        DotRow::Edited(index) => {
            let edit = &state.edited[index];
            let mut detail =
                Detail::new(tilde(home, &edit.target)).pill("edited in place", Color::Yellow);
            detail.prose(
                "This rendered file changed after it was deployed. Deploy leaves it alone until the edit is captured into its template (c).",
                width,
                dim(),
            );
            detail.blank();
            for line in capture_preview(edit).into_iter().skip(1) {
                let style = diff_style(&line);
                detail.lines.push(Line::styled(line, style));
            }
            detail
        }
        DotRow::Change(index) => {
            let item = &state.items.as_ref().expect("rows come from items")[index];
            let color = match item.change {
                Change::Error => Color::Red,
                Change::BackedUp | Change::Pruned => Color::Yellow,
                _ => Color::Blue,
            };
            let mut detail =
                Detail::new(tilde(home, &item.target)).pill(change_verb(item.change), color);
            detail.field(
                "Target",
                item.target.display().to_string(),
                width,
                Style::default(),
            );
            if !item.detail.is_empty() {
                detail.field("Detail", item.detail.clone(), width, Style::default());
            }
            detail.blank();
            detail.prose(
                match item.change {
                    Change::BackedUp => {
                        "A real file is in the way. Deploy moves it aside with a timestamped .bak suffix, then links."
                    }
                    Change::Pruned => {
                        "The link points at a file the repos no longer have. Deploy removes it."
                    }
                    Change::Rerendered => {
                        "The template or a variable changed. Deploy renders it again."
                    }
                    Change::Error => "Deploy can't handle this target; see the detail above.",
                    _ => "Deploy links this file from the repo.",
                },
                width,
                dim(),
            );
            detail
        }
        DotRow::Current(count) => {
            let mut detail =
                Detail::new("Deployed files").pill(format!("{count} current"), Color::Green);
            for item in state.unchanged() {
                detail
                    .lines
                    .push(Line::raw(fit(&clean(&tilde(home, &item.target)), width)));
            }
            detail
        }
        DotRow::Palette => {
            let active = workspace.palette_name();
            let mut detail = Detail::new("Palette");
            if let Some(active) = active {
                detail = detail.pill(active.to_string(), ACCENT);
            }
            detail.prose(
                "One setting recolors every themed dotfile. p, or a click on the palette in the top bar, switches to the next one and redeploys.",
                width,
                dim(),
            );
            detail.blank();
            for name in app.palette_names() {
                let on = Some(name.as_str()) == active;
                detail.lines.push(Line::from(vec![
                    Span::styled(if on { "● " } else { "○ " }, fg(ACCENT)),
                    Span::styled(clean(&name), if on { bold() } else { Style::default() }),
                ]));
            }
            detail
        }
        DotRow::Var(name) => {
            let definition = &workspace.config.vars[&name];
            let (value, source) = var_display(definition, workspace.host.vars.get(&name));
            let mut detail = Detail::new(format!("${name}"));
            detail = if source == "needs a value" {
                detail.pill("needs a value", Color::Yellow)
            } else {
                detail.pill(format!("from {source}"), Color::Gray)
            };
            if let Some(description) = definition.description() {
                detail.prose(description, width, Style::default());
                detail.blank();
            }
            detail.field("Value", value, width, Style::default());
            if definition.is_secret() {
                detail.field(
                    "Secret",
                    "yes, kept out of the repos",
                    width,
                    Style::default(),
                );
            } else if let Some(default) = definition.default_value() {
                detail.field("Default", default.to_string(), width, dim());
            }
            detail.blank();
            detail.prose(
                "The value is stored in this host's host.toml. e or Enter edits it; deploy afterwards to re-render.",
                width,
                dim(),
            );
            detail
        }
    }
}

fn repos_detail(app: &App, width: usize) -> Detail {
    match app.current_repo() {
        Some(RepoRow::Root(index)) => root_detail(app, index, width),
        _ => schedule_detail(app, width),
    }
}

fn root_detail(app: &App, index: usize, width: usize) -> Detail {
    let root = &app.session.workspace.roots[index];
    let incoming = app.repos.incoming(&root.name);
    let outgoing = app.repos.outgoing(&root.name);
    let mut detail = Detail::new(root.name.clone());
    detail = if !root.present {
        detail.pill("not cloned", Color::Yellow)
    } else if app.repos.data.is_none() {
        detail.pill("checking", Color::Gray)
    } else {
        let down = incoming.map_or(0, |repo| repo.commits.len());
        let local = outgoing.map_or(0, |repo| repo.commits.len() + repo.status.len());
        match (down, local) {
            (0, 0) => detail.pill("in sync", Color::Green),
            (0, _) => detail.pill("local changes", Color::Yellow),
            _ => detail.pill(format!("{down} incoming"), Color::Blue),
        }
    };
    detail.field(
        "Path",
        root.path.display().to_string(),
        width,
        Style::default(),
    );
    if let Some(repo) = incoming {
        detail.field(
            "Upstream",
            repo.upstream.clone().unwrap_or_else(|| "(none)".into()),
            width,
            Style::default(),
        );
    }
    if !root.present {
        detail.blank();
        detail.prose("Sync (s) clones it.", width, dim());
        return detail;
    }
    if let Some(repo) = incoming {
        detail.blank();
        detail.heading(&format!("Incoming ({})", repo.commits.len()));
        if let Some(problem) = &repo.problem {
            detail.prose(problem, width, fg(Color::Red));
        }
        for commit in &repo.commits {
            detail.lines.push(Line::raw(fit(&clean(commit), width)));
        }
    }
    if let Some(repo) = outgoing {
        detail.blank();
        detail.heading(&format!(
            "Local ({} changed, {} unpushed)",
            repo.status.len(),
            repo.commits.len()
        ));
        if let Some(problem) = &repo.problem {
            detail.prose(problem, width, fg(Color::Red));
        }
        for line in &repo.status {
            detail
                .lines
                .push(Line::styled(clean(line), fg(Color::Yellow)));
        }
        for commit in &repo.commits {
            detail.lines.push(Line::raw(format!(
                "unpushed {}",
                fit(&clean(commit), width.saturating_sub(9))
            )));
        }
    }
    if let Some(repo) = incoming
        && !repo.diff.is_empty()
    {
        detail.blank();
        detail.heading("Incoming diff");
        detail.diff(&repo.diff);
    }
    if let Some(repo) = outgoing
        && !repo.diff.is_empty()
    {
        detail.blank();
        detail.heading("Local diff, variables restored");
        detail.diff(&repo.diff);
    }
    detail
}

fn schedule_detail(app: &App, width: usize) -> Detail {
    let workspace = &app.session.workspace;
    let selection = &app.session.selection;
    let mut detail = Detail::new("Schedule and host");
    detail = match &app.repos.schedule {
        Some(status) if status.installed => detail.pill("auto-update on", Color::Green),
        Some(_) => detail.pill("auto-update off", Color::Gray),
        None => detail,
    };
    detail.heading("Scheduled update");
    match &app.repos.schedule {
        None => detail.prose("reading...", width, dim()),
        Some(status) if !status.installed => detail.prose(
            "No scheduled update. `dfutil schedule` sets one up.",
            width,
            Style::default(),
        ),
        Some(status) => {
            let or_unknown =
                |value: &Option<String>| value.clone().unwrap_or_else(|| "unknown".into());
            detail.field(
                "Schedule",
                or_unknown(&status.schedule),
                width,
                Style::default(),
            );
            detail.field(
                "Next run",
                or_unknown(&status.next_run),
                width,
                Style::default(),
            );
            detail.field(
                "Last result",
                or_unknown(&status.last_result),
                width,
                Style::default(),
            );
        }
    }
    detail.blank();
    detail.heading("Host");
    detail.field(
        "Host config",
        workspace.host_path.display().to_string(),
        width,
        Style::default(),
    );
    detail.field(
        "Platform",
        format!(
            "{} {}{}",
            workspace.platform.os,
            workspace.platform.arch,
            if workspace.platform.wsl { " (WSL)" } else { "" }
        ),
        width,
        Style::default(),
    );
    detail.field(
        "Scope",
        workspace.paths.scope.to_string(),
        width,
        Style::default(),
    );
    detail.field(
        "Build mode",
        match workspace.build_mode() {
            crate::config::BuildMode::Prebuilt => "prebuilt",
            crate::config::BuildMode::Source => "source",
        },
        width,
        Style::default(),
    );
    detail.field(
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
        width,
        Style::default(),
    );
    detail.field(
        "Prefix",
        workspace.paths.prefix.display().to_string(),
        width,
        Style::default(),
    );
    detail.blank();
    detail.heading("Config check");
    let problems = workspace.validate();
    if problems.is_empty() {
        detail.prose(
            &format!(
                "valid: {} tools, {} presets, {} packages, {} palettes",
                workspace.config.tools.len(),
                workspace.config.presets.len(),
                workspace.config.packages.len(),
                workspace.palettes.len()
            ),
            width,
            fg(Color::Green),
        );
    }
    for problem in problems {
        detail.prose(&format!("problem: {problem}"), width, fg(Color::Red));
    }
    for note in &workspace.notes {
        detail.prose(&format!("note: {note}"), width, dim());
    }
    detail
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

fn diff_style(line: &str) -> Style {
    let line = line.trim_start();
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        bold()
    } else if line.starts_with('+') {
        fg(Color::Green)
    } else if line.starts_with('-') {
        fg(Color::Red)
    } else if line.starts_with("@@") {
        fg(Color::Cyan)
    } else {
        Style::default()
    }
}

// ----- job strip and hints ------------------------------------------------------

fn draw_strip(frame: &mut Frame, area: Rect, app: &App) {
    app.hits.add(area, Hit::Key(KeyCode::Char('L')));
    let width = area.width as usize;
    let line = if let Some(job) = &app.job {
        let elapsed = job.started.elapsed().as_secs();
        let mut spans = vec![
            Span::styled(
                format!(" {} ", SPINNER[app.frame % SPINNER.len()]),
                accent(),
            ),
            Span::styled(clean(&job.label), bold()),
            Span::styled(format!("  {elapsed}s"), dim()),
        ];
        if let Some(last) = &job.last_line {
            let used = width_of(&spans) as usize + 2;
            spans.push(Span::styled(
                format!("  {}", fit(last, width.saturating_sub(used))),
                dim(),
            ));
        }
        Line::from(spans)
    } else if let Some(status) = &app.status {
        let style = match status.tone {
            Tone::Info => Style::default(),
            Tone::Success => fg(Color::Green),
            Tone::Error => fg(Color::Red).add_modifier(Modifier::BOLD),
        };
        Line::styled(
            format!(" {}", fit(&status.text, width.saturating_sub(1))),
            style,
        )
    } else {
        Line::styled(
            format!(" {}", fit(&idle_summary(app), width.saturating_sub(1))),
            dim(),
        )
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// What the strip says with no job running and nothing to report.
fn idle_summary(app: &App) -> String {
    let mut parts = Vec::new();
    match &app.tools.plan {
        Some(plan) => {
            let changes = plan.changes().count();
            parts.push(if changes == 0 {
                "tools up to date".to_string()
            } else {
                format!("{changes} tool change(s) planned, a applies them")
            });
        }
        None => parts.push("looking up tools".into()),
    }
    if !app.dotfiles.edited.is_empty() {
        parts.push(format!(
            "{} dotfile(s) edited in place",
            app.dotfiles.edited.len()
        ));
    }
    if let Some((incoming, _)) = &app.repos.data {
        let down: usize = incoming.iter().map(|repo| repo.commits.len()).sum();
        if down > 0 {
            parts.push(format!("{down} incoming commit(s)"));
        }
    }
    parts.push("L or a click here shows the log".into());
    parts.join(" · ")
}

fn draw_hints(frame: &mut Frame, area: Rect, app: &App) {
    let mut row = Row::new(app, area);
    row.push(vec![Span::raw(" ")], None);
    for hint in app.hints() {
        let spans = vec![
            Span::styled(hint.keys, accent()),
            Span::styled(format!(" {}", hint.label), dim()),
        ];
        if !row.push(spans, Some(Hit::Key(hint.code))) {
            break;
        }
        row.push(vec![Span::raw("  ")], None);
    }
    row.render(frame);
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

/// Draws a dialog frame with a row of buttons at the bottom, and returns the
/// body area.
fn dialog(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    title: &str,
    size: (u16, u16),
    buttons: &[Button],
) -> Rect {
    let rect = centered(area, size.0, size.1);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(fg(ACCENT))
        .title(Line::from(vec![
            Span::raw(" "),
            Span::styled(clean(title), bold()),
            Span::raw(" "),
        ]));
    let inner = block.inner(rect).inner(Margin::new(1, 0));
    frame.render_widget(block, rect);
    app.hits.add(rect, Hit::ModalBody);
    let [body, _, footer] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    draw_buttons(frame, footer, app, buttons);
    body
}

/// Renders `lines` into `body` from line `scroll`, clamped, and records how
/// far the dialog can scroll.
fn scrolled(frame: &mut Frame, body: Rect, app: &App, lines: Vec<Line<'static>>, scroll: usize) {
    let height = body.height as usize;
    let max = lines.len().saturating_sub(height);
    app.modal_max.set(max);
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(scroll.min(max))
        .take(height)
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
}

fn button(keys: &'static str, code: KeyCode, label: &str, primary: bool) -> Button {
    Button {
        keys,
        code,
        label: label.into(),
        primary,
    }
}

const HELP: &[(&str, &str)] = &[
    ("Everywhere", ""),
    ("1-4, Tab, Shift-Tab", "focus a panel (or click it)"),
    ("j/k, arrows, wheel", "move in the focused panel"),
    ("J/K, Ctrl-d/u", "scroll the detail pane"),
    ("a", "apply every planned tool change (asks first)"),
    ("U", "sync the repos, deploy, and apply (asks first)"),
    ("L", "open the log (or click the job line)"),
    ("q, Esc", "quit; twice while a job runs"),
    ("Tools", ""),
    ("space", "mark a tool, or open and close a group"),
    ("m", "mark or clear every shown tool"),
    ("h/l, left/right", "close or open a group"),
    ("g", "group by status, preset, or category"),
    ("/", "filter by name; Esc clears"),
    ("u", "install or update the marked or highlighted tools"),
    ("r, b, x", "reinstall, roll back, uninstall"),
    ("n, R", "add a tool, look versions up again"),
    ("Presets", ""),
    ("space, Enter", "turn the preset on or off for this host"),
    ("n", "create a preset from the marked tools"),
    ("Dotfiles", ""),
    ("d, c", "deploy, capture in-place edits"),
    ("p", "switch to the next palette"),
    ("e, Enter", "edit a template variable"),
    ("Repos", ""),
    ("s, R", "sync now, check again"),
    ("Mouse", ""),
    ("click", "select; a key chip or button runs it"),
    ("[ ] and [x] boxes", "mark a tool, turn a preset on or off"),
    ("wheel", "move a list, scroll the detail pane or a dialog"),
];

fn draw_help(frame: &mut Frame, area: Rect, app: &App, scroll: usize) {
    let lines: Vec<Line<'static>> = HELP
        .iter()
        .map(|(keys, what)| {
            if what.is_empty() {
                Line::styled(*keys, accent())
            } else {
                Line::from(vec![
                    Span::styled(format!("  {keys:<22}"), bold()),
                    Span::raw(*what),
                ])
            }
        })
        .collect();
    let height = lines.len() as u16 + 4;
    let close = [button("esc", KeyCode::Esc, "Close", true)];
    let body = dialog(frame, area, app, "Keys and mouse", (76, height), &close);
    scrolled(frame, body, app, lines, scroll);
}

fn draw_log(frame: &mut Frame, area: Rect, app: &App, scroll: Option<usize>) {
    let lines: Vec<Line<'static>> = if app.log.is_empty() {
        vec![Line::styled("Nothing logged yet.", dim())]
    } else {
        app.log
            .iter()
            .map(|line| {
                Line::styled(
                    line.text.clone(),
                    if line.error {
                        fg(Color::Red)
                    } else {
                        Style::default()
                    },
                )
            })
            .collect()
    };
    let size = (area.width.saturating_sub(6), area.height.saturating_sub(4));
    let title = if scroll.is_none() {
        "Log, following new lines"
    } else {
        "Log"
    };
    let close = [button("esc", KeyCode::Esc, "Close", true)];
    let body = dialog(frame, area, app, title, size, &close);
    // Following shows the newest lines.
    scrolled(frame, body, app, lines, scroll.unwrap_or(usize::MAX));
}

fn draw_confirm(frame: &mut Frame, area: Rect, app: &App, confirm: &Confirm) {
    let lines: Vec<Line<'static>> = confirm
        .lines
        .iter()
        .map(|line| Line::styled(clean(line), diff_style(line)))
        .collect();
    let buttons = if confirm.request.is_some() {
        vec![
            button("y", KeyCode::Char('y'), "Confirm", true),
            button("n", KeyCode::Char('n'), "Cancel", false),
        ]
    } else {
        vec![button("esc", KeyCode::Esc, "Close", true)]
    };
    let height = lines.len() as u16 + 5;
    let width = area.width.saturating_sub(8).min(90);
    let body = dialog(frame, area, app, &confirm.title, (width, height), &buttons);
    scrolled(frame, body, app, lines, confirm.scroll);
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
        Span::styled(
            clean(&at),
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(clean(&after)),
    ]
}

fn draw_form(frame: &mut Frame, area: Rect, app: &App, form: &Form) {
    let width = area.width.saturating_sub(8).min(86);
    let height = form.fields.len() as u16 + 8;
    let buttons = [
        button("enter", KeyCode::Enter, "Save", true),
        button("esc", KeyCode::Esc, "Cancel", false),
        button("tab", KeyCode::Tab, "Next field", false),
    ];
    let body = dialog(frame, area, app, &form.title, (width, height), &buttons);
    let mut lines: Vec<Line> = Vec::new();
    for (index, field) in form.fields.iter().enumerate() {
        let focused = index == form.focus;
        if (index as u16) < body.height {
            app.hits.add(
                Rect::new(body.x, body.y + index as u16, body.width, 1),
                Hit::FormField(index),
            );
        }
        let mut spans = vec![
            Span::styled(if focused { "› " } else { "  " }, fg(ACCENT)),
            Span::styled(
                format!("{:<22} ", field.label),
                if focused { bold() } else { dim() },
            ),
        ];
        match &field.kind {
            FieldKind::Text => spans.extend(input_spans(&field.input, focused, false)),
            FieldKind::Secret => spans.extend(input_spans(&field.input, focused, true)),
            FieldKind::Choice { options, selected } => {
                spans.push(Span::styled("‹ ", dim()));
                spans.push(Span::styled(options[*selected].clone(), fg(ACCENT)));
                spans.push(Span::styled(" ›", dim()));
            }
            FieldKind::Check(value) => spans.push(if *value {
                Span::styled("[x]", fg(ACCENT).add_modifier(Modifier::BOLD))
            } else {
                Span::styled("[ ]", dim())
            }),
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));
    match &form.error {
        Some(error) => lines.push(Line::styled(
            clean(error),
            fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        None => lines.push(Line::styled(clean(&form.fields[form.focus].hint), dim())),
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
}

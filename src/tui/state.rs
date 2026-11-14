//! Per-panel state: cursors, scroll offsets, grouping, and the data jobs
//! delivered. Everything here is plain data with no terminal attached.

use super::input::Input;
use crate::config::PresetDef;
use crate::deploy::{Change, Edited, Item};
use crate::gitops::{Incoming, Outgoing};
use crate::plan::{Action, Plan, ToolRow};
use crate::schedule;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};

/// The four stacked panels on the left. The detail pane on the right always
/// describes the focused panel's selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Panel {
    Tools,
    Presets,
    Dotfiles,
    Repos,
}

impl Panel {
    pub const ALL: [Panel; 4] = [Panel::Tools, Panel::Presets, Panel::Dotfiles, Panel::Repos];

    pub fn title(self) -> &'static str {
        match self {
            Panel::Tools => "Tools",
            Panel::Presets => "Presets",
            Panel::Dotfiles => "Dotfiles",
            Panel::Repos => "Repos",
        }
    }

    pub fn index(self) -> usize {
        Panel::ALL
            .iter()
            .position(|panel| *panel == self)
            .expect("every panel is listed")
    }

    pub fn next(self) -> Panel {
        Panel::ALL[(self.index() + 1) % Panel::ALL.len()]
    }

    pub fn prev(self) -> Panel {
        Panel::ALL[(self.index() + Panel::ALL.len() - 1) % Panel::ALL.len()]
    }
}

/// A cursor over a list, with a scroll offset the renderer keeps in step.
#[derive(Clone, Debug, Default)]
pub struct Scroller {
    pub cursor: usize,
    offset: Cell<usize>,
}

impl Scroller {
    pub fn clamp(&mut self, len: usize) {
        self.cursor = self.cursor.min(len.saturating_sub(1));
    }

    pub fn down(&mut self, by: usize, len: usize) {
        self.cursor = (self.cursor + by).min(len.saturating_sub(1));
    }

    pub fn up(&mut self, by: usize) {
        self.cursor = self.cursor.saturating_sub(by);
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self, len: usize) {
        self.cursor = len.saturating_sub(1);
    }

    /// The range of rows to draw in `height` lines, scrolled just enough to
    /// keep the cursor visible.
    pub fn window(&self, height: usize, len: usize) -> std::ops::Range<usize> {
        let height = height.max(1);
        let mut offset = self.offset.get().min(len.saturating_sub(1));
        if self.cursor < offset {
            offset = self.cursor;
        } else if self.cursor >= offset + height {
            offset = self.cursor + 1 - height;
        }
        self.offset.set(offset);
        offset..(offset + height).min(len)
    }
}

// ----- tools ----------------------------------------------------------------

/// How a tool reads at a glance: one glyph and one color in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Failed,
    Install,
    Upgrade,
    Reinstall,
    Remove,
    Held,
    Current,
    Off,
}

impl Status {
    pub fn of(row: &ToolRow) -> Status {
        match row.action {
            Action::Error => Status::Failed,
            Action::Install => Status::Install,
            Action::Upgrade => Status::Upgrade,
            Action::Reinstall => Status::Reinstall,
            Action::Uninstall => Status::Remove,
            Action::Keep if row.held => Status::Held,
            Action::Keep => Status::Current,
            Action::Available => Status::Off,
        }
    }

    /// Whether the row wants a decision or an apply.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Status::Failed | Status::Install | Status::Upgrade | Status::Reinstall | Status::Remove
        )
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Status::Failed => "✗",
            Status::Install => "+",
            Status::Upgrade => "↑",
            Status::Reinstall => "↻",
            Status::Remove => "−",
            Status::Held => "‖",
            Status::Current => "●",
            Status::Off => "○",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Failed => "lookup failed",
            Status::Install => "will install",
            Status::Upgrade => "update available",
            Status::Reinstall => "will reinstall",
            Status::Remove => "will be removed",
            Status::Held => "held after a rollback",
            Status::Current => "up to date",
            Status::Off => "not selected",
        }
    }
}

/// What the tools list is grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    Status,
    Preset,
    Category,
}

impl Grouping {
    pub const ALL: [Grouping; 3] = [Grouping::Status, Grouping::Preset, Grouping::Category];

    pub fn label(self) -> &'static str {
        match self {
            Grouping::Status => "status",
            Grouping::Preset => "preset",
            Grouping::Category => "category",
        }
    }

    pub fn next(self) -> Grouping {
        match self {
            Grouping::Status => Grouping::Preset,
            Grouping::Preset => Grouping::Category,
            Grouping::Category => Grouping::Status,
        }
    }
}

/// One line of the tools list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolItem {
    Header {
        /// Grouping-qualified key, such as `status:current`, for open state.
        key: String,
        label: String,
        count: usize,
        attention: usize,
        /// For preset groups: whether the preset is active on this host.
        active: Option<bool>,
        open: bool,
    },
    /// Index into the plan's rows.
    Tool(usize),
}

/// What the cursor sits on, so it can follow its item when the list changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToolAnchor {
    Header(String),
    Tool(String),
}

#[derive(Debug)]
pub struct ToolsState {
    pub plan: Option<Plan>,
    /// Why the last lookup failed, if it did.
    pub error: Option<String>,
    pub marked: BTreeSet<String>,
    pub filter: Input,
    pub filtering: bool,
    pub grouping: Grouping,
    /// Groups opened or closed by hand; the rest use their default.
    pub open: BTreeMap<String, bool>,
    pub list: Scroller,
}

impl Default for ToolsState {
    fn default() -> Self {
        Self {
            plan: None,
            error: None,
            marked: BTreeSet::new(),
            filter: Input::default(),
            filtering: false,
            grouping: Grouping::Status,
            open: BTreeMap::new(),
            list: Scroller::default(),
        }
    }
}

impl ToolsState {
    pub fn rows(&self) -> &[ToolRow] {
        self.plan.as_ref().map_or(&[], |plan| plan.rows.as_slice())
    }

    pub fn row(&self, name: &str) -> Option<&ToolRow> {
        self.rows().iter().find(|row| row.name == name)
    }

    /// The grouped, filtered list. `presets` are the config's presets and
    /// `active` the ones in effect on this host (extends included).
    pub fn items(
        &self,
        presets: &BTreeMap<String, PresetDef>,
        active: &BTreeSet<String>,
    ) -> Vec<ToolItem> {
        let rows = self.rows();
        let needle = self.filter.text().to_lowercase();
        let shown: Vec<usize> = (0..rows.len())
            .filter(|index| needle.is_empty() || rows[*index].name.to_lowercase().contains(&needle))
            .collect();
        let status = |index: &usize| Status::of(&rows[*index]);

        // (key, label, active, members)
        let mut groups: Vec<(String, String, Option<bool>, Vec<usize>)> = Vec::new();
        match self.grouping {
            Grouping::Status => {
                type Bucket = (&'static str, &'static str, fn(Status) -> bool);
                let buckets: [Bucket; 4] = [
                    ("attention", "Needs attention", Status::needs_attention),
                    ("held", "Held back", |status| status == Status::Held),
                    ("current", "Up to date", |status| status == Status::Current),
                    ("off", "Not selected", |status| status == Status::Off),
                ];
                for (key, label, test) in buckets {
                    let mut members: Vec<usize> = shown
                        .iter()
                        .copied()
                        .filter(|index| test(status(index)))
                        .collect();
                    // Failures first, then by name.
                    members.sort_by_key(|index| (status(index), rows[*index].name.clone()));
                    groups.push((key.into(), label.into(), None, members));
                }
            }
            Grouping::Preset => {
                let mut placed = BTreeSet::new();
                for (name, preset) in presets {
                    let members: Vec<usize> = shown
                        .iter()
                        .copied()
                        .filter(|index| preset.tools.contains(&rows[*index].name))
                        .collect();
                    placed.extend(members.iter().copied());
                    groups.push((
                        name.clone(),
                        name.clone(),
                        Some(active.contains(name)),
                        members,
                    ));
                }
                let rest = shown
                    .iter()
                    .copied()
                    .filter(|index| !placed.contains(index))
                    .collect();
                groups.push(("-".into(), "In no preset".into(), None, rest));
            }
            Grouping::Category => {
                let mut by: BTreeMap<String, Vec<usize>> = BTreeMap::new();
                let mut rest = Vec::new();
                for index in &shown {
                    match &rows[*index].category {
                        Some(category) => by.entry(category.clone()).or_default().push(*index),
                        None => rest.push(*index),
                    }
                }
                for (category, members) in by {
                    groups.push((category.clone(), category, None, members));
                }
                groups.push(("-".into(), "Uncategorized".into(), None, rest));
            }
        }
        groups.retain(|(_, _, _, members)| !members.is_empty());

        let any_flagged = shown.iter().any(|index| {
            let status = status(index);
            status.needs_attention() || status == Status::Held
        });
        let mut items = Vec::new();
        for (key, label, active, members) in groups {
            let key = format!("{}:{key}", self.grouping.label());
            let attention = members
                .iter()
                .filter(|index| status(index).needs_attention())
                .count();
            let flagged =
                attention > 0 || members.iter().any(|index| status(index) == Status::Held);
            // Open what wants a look; with nothing flagged, open the
            // up-to-date group so the list isn't all headers.
            let default = flagged || (!any_flagged && key == "status:current");
            let open = !needle.is_empty() || self.open.get(&key).copied().unwrap_or(default);
            items.push(ToolItem::Header {
                key,
                label,
                count: members.len(),
                attention,
                active,
                open,
            });
            if open {
                items.extend(members.into_iter().map(ToolItem::Tool));
            }
        }
        items
    }

    pub fn anchor(item: &ToolItem, rows: &[ToolRow]) -> ToolAnchor {
        match item {
            ToolItem::Header { key, .. } => ToolAnchor::Header(key.clone()),
            ToolItem::Tool(index) => ToolAnchor::Tool(rows[*index].name.clone()),
        }
    }

    /// Where `anchor` is in `items`, if it's still there.
    pub fn find(items: &[ToolItem], rows: &[ToolRow], anchor: &ToolAnchor) -> Option<usize> {
        items
            .iter()
            .position(|item| &ToolsState::anchor(item, rows) == anchor)
    }
}

// ----- presets --------------------------------------------------------------

#[derive(Debug, Default)]
pub struct PresetsState {
    pub list: Scroller,
}

// ----- dotfiles -------------------------------------------------------------

/// One line of the dotfiles panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DotRow {
    /// A problem from the last preview.
    Problem(usize),
    /// A rendered file edited in place (index into `edited`).
    Edited(usize),
    /// A target a deploy would change (index into `items`).
    Change(usize),
    /// Every target a deploy would leave alone, as one line.
    Current(usize),
    Palette,
    Var(String),
}

#[derive(Debug, Default)]
pub struct DotfilesState {
    /// None until the first dry run lands.
    pub items: Option<Vec<Item>>,
    pub edited: Vec<Edited>,
    pub problems: Vec<String>,
    pub list: Scroller,
}

impl DotfilesState {
    pub fn rows(&self, has_palettes: bool, vars: &[String]) -> Vec<DotRow> {
        let mut rows: Vec<DotRow> = (0..self.problems.len()).map(DotRow::Problem).collect();
        rows.extend((0..self.edited.len()).map(DotRow::Edited));
        let edited: BTreeSet<_> = self.edited.iter().map(|edit| &edit.target).collect();
        let items = self.items.as_deref().unwrap_or_default();
        let mut current = 0;
        for (index, item) in items.iter().enumerate() {
            if item.change == Change::Unchanged {
                current += 1;
            } else if !(item.change == Change::Skipped && edited.contains(&item.target)) {
                rows.push(DotRow::Change(index));
            }
        }
        if current > 0 {
            rows.push(DotRow::Current(current));
        }
        if has_palettes {
            rows.push(DotRow::Palette);
        }
        rows.extend(vars.iter().cloned().map(DotRow::Var));
        rows
    }

    /// Targets a deploy would leave alone.
    pub fn unchanged(&self) -> impl Iterator<Item = &Item> {
        self.items
            .iter()
            .flatten()
            .filter(|item| item.change == Change::Unchanged)
    }
}

// ----- repos ----------------------------------------------------------------

/// One line of the repos panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RepoRow {
    /// Index into the workspace's roots.
    Root(usize),
    Schedule,
}

#[derive(Debug, Default)]
pub struct ReposState {
    /// None until the first check lands.
    pub data: Option<(Vec<Incoming>, Vec<Outgoing>)>,
    pub schedule: Option<schedule::Status>,
    pub list: Scroller,
}

impl ReposState {
    pub fn incoming(&self, root: &str) -> Option<&Incoming> {
        self.data.as_ref()?.0.iter().find(|repo| repo.root == root)
    }

    pub fn outgoing(&self, root: &str) -> Option<&Outgoing> {
        self.data.as_ref()?.1.iter().find(|repo| repo.root == root)
    }
}

// ----- helpers --------------------------------------------------------------

/// Strips control characters from text that came from outside (log output,
/// diffs, config descriptions), so it can't move the cursor or inject escape
/// sequences into the terminal. Tabs become four spaces.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\t' => out.push_str("    "),
            character if character.is_control() => out.push('?'),
            character => out.push(character),
        }
    }
    out
}

/// What a planned action is called in the UI.
pub fn action_label(action: Action) -> &'static str {
    match action {
        Action::Install => "install",
        Action::Upgrade => "upgrade",
        Action::Reinstall => "reinstall",
        Action::Uninstall => "uninstall",
        Action::Keep => "keep",
        Action::Available => "available",
        Action::Error => "error",
    }
}

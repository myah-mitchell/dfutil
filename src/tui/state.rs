//! Per-tab state: cursors, scroll offsets, and the data jobs delivered.

use super::input::Input;
use crate::deploy::{Edited, Item};
use crate::gitops::{Incoming, Outgoing};
use crate::plan::{Plan, ToolRow};
use crate::schedule;
use std::cell::Cell;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Tools,
    Presets,
    Dotfiles,
    Sync,
    Settings,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Tools,
        Tab::Presets,
        Tab::Dotfiles,
        Tab::Sync,
        Tab::Settings,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Tools => "Tools",
            Tab::Presets => "Presets",
            Tab::Dotfiles => "Dotfiles",
            Tab::Sync => "Sync",
            Tab::Settings => "Settings",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL
            .iter()
            .position(|tab| *tab == self)
            .expect("every tab is listed")
    }

    pub fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
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

#[derive(Debug, Default)]
pub struct ToolsState {
    pub plan: Option<Plan>,
    /// Why the last lookup failed, if it did.
    pub error: Option<String>,
    pub marked: BTreeSet<String>,
    pub filter: Input,
    pub filtering: bool,
    pub list: Scroller,
}

impl ToolsState {
    /// Rows matching the filter, in name order.
    pub fn visible(&self) -> Vec<&ToolRow> {
        let needle = self.filter.text().to_lowercase();
        self.plan
            .iter()
            .flat_map(|plan| plan.rows.iter())
            .filter(|row| needle.is_empty() || row.name.to_lowercase().contains(&needle))
            .collect()
    }

    pub fn current(&self) -> Option<&ToolRow> {
        self.visible().get(self.list.cursor).copied()
    }

    /// Replaces the plan, keeping the cursor on the same tool and dropping
    /// marks for tools that no longer exist.
    pub fn set_plan(&mut self, plan: Plan) {
        let name = self.current().map(|row| row.name.clone());
        self.marked
            .retain(|marked| plan.rows.iter().any(|row| &row.name == marked));
        self.plan = Some(plan);
        self.error = None;
        let visible = self.visible();
        let len = visible.len();
        let index = name.and_then(|name| visible.iter().position(|row| row.name == name));
        if let Some(index) = index {
            self.list.cursor = index;
        }
        self.list.clamp(len);
    }

    /// Marked tool names, or the highlighted one when nothing is marked.
    pub fn targets(&self) -> Vec<String> {
        if self.marked.is_empty() {
            self.current()
                .map(|row| vec![row.name.clone()])
                .unwrap_or_default()
        } else {
            self.marked.iter().cloned().collect()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DotPane {
    Targets,
    Palettes,
    Vars,
}

impl DotPane {
    pub fn right(self) -> DotPane {
        match self {
            DotPane::Targets => DotPane::Palettes,
            _ => DotPane::Vars,
        }
    }

    pub fn left(self) -> DotPane {
        match self {
            DotPane::Vars => DotPane::Palettes,
            _ => DotPane::Targets,
        }
    }
}

#[derive(Debug)]
pub struct DotfilesState {
    /// None until the first dry run lands.
    pub items: Option<Vec<Item>>,
    pub edited: Vec<Edited>,
    pub problems: Vec<String>,
    pub focus: DotPane,
    pub changes_only: bool,
    pub targets: Scroller,
    pub palettes: Scroller,
    pub vars: Scroller,
}

impl Default for DotfilesState {
    fn default() -> Self {
        Self {
            items: None,
            edited: Vec::new(),
            problems: Vec::new(),
            focus: DotPane::Targets,
            changes_only: false,
            targets: Scroller::default(),
            palettes: Scroller::default(),
            vars: Scroller::default(),
        }
    }
}

impl DotfilesState {
    pub fn visible_items(&self) -> Vec<&Item> {
        self.items
            .iter()
            .flatten()
            .filter(|item| !self.changes_only || item.change != crate::deploy::Change::Unchanged)
            .collect()
    }
}

#[derive(Debug, Default)]
pub struct SyncState {
    /// None until the first check lands.
    pub data: Option<(Vec<Incoming>, Vec<Outgoing>)>,
    pub list: Scroller,
    pub diff_scroll: usize,
    /// Set once the first check was queued, so visiting the tab fetches once.
    pub requested: bool,
}

/// One row of the sync list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncEntry {
    Incoming(usize),
    Outgoing(usize),
}

impl SyncState {
    pub fn entries(&self) -> Vec<SyncEntry> {
        let Some((incoming, outgoing)) = &self.data else {
            return Vec::new();
        };
        let mut entries = Vec::new();
        for index in 0..incoming.len() {
            entries.push(SyncEntry::Incoming(index));
        }
        for index in 0..outgoing.len() {
            entries.push(SyncEntry::Outgoing(index));
        }
        entries
    }

    pub fn current(&self) -> Option<SyncEntry> {
        self.entries().get(self.list.cursor).copied()
    }

    /// The viewer's content for the highlighted entry: header lines (title,
    /// problem, commits, status) and the raw diff text.
    pub fn view(&self) -> Option<(Vec<String>, &str)> {
        let (incoming, outgoing) = self.data.as_ref()?;
        match self.current()? {
            SyncEntry::Incoming(index) => {
                let repo = &incoming[index];
                let mut header = vec![format!(
                    "{}: incoming from {}",
                    repo.root,
                    repo.upstream.as_deref().unwrap_or("(no upstream)")
                )];
                if let Some(problem) = &repo.problem {
                    header.push(format!("problem: {problem}"));
                } else if repo.commits.is_empty() {
                    header.push("up to date".into());
                }
                header.extend(repo.commits.iter().map(|commit| format!("  {commit}")));
                Some((header, &repo.diff))
            }
            SyncEntry::Outgoing(index) => {
                let repo = &outgoing[index];
                let mut header = vec![format!("{}: local changes", repo.root)];
                if let Some(problem) = &repo.problem {
                    header.push(format!("problem: {problem}"));
                } else if repo.status.is_empty() && repo.commits.is_empty() {
                    header.push("nothing to push".into());
                }
                header.extend(repo.status.iter().map(|line| format!("  {line}")));
                header.extend(
                    repo.commits
                        .iter()
                        .map(|commit| format!("  unpushed: {commit}")),
                );
                Some((header, &repo.diff))
            }
        }
    }

    /// Number of lines in the viewer for the highlighted entry.
    pub fn diff_len(&self) -> usize {
        self.view()
            .map_or(0, |(header, diff)| header.len() + 1 + diff.lines().count())
    }
}

#[derive(Debug, Default)]
pub struct SettingsState {
    pub schedule: Option<schedule::Status>,
    pub scroll: usize,
    pub requested: bool,
}

#[derive(Debug, Default)]
pub struct PresetsState {
    pub list: Scroller,
}

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
pub fn action_label(action: crate::plan::Action) -> &'static str {
    use crate::plan::Action;
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

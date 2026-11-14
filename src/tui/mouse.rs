//! Mouse support. The renderer records every clickable region of the frame
//! it draws in a [`HitMap`]; the app looks a click or wheel event up there
//! and turns it into the same state change a key would make.

use super::state::{Grouping, Panel};
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Position, Rect};
use std::cell::RefCell;

/// What a region of the screen does when clicked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hit {
    /// A panel's frame or title: focuses it.
    Focus(Panel),
    /// A line of the tools list (item index): selects it, and opens or
    /// closes a group header.
    ToolItem(usize),
    /// The mark box at the start of a tool line (item index).
    ToolMark(usize),
    Grouping(Grouping),
    Filter,
    /// The palette in the top bar: switches to the next one.
    Palette,
    /// A preset chip's name: selects it.
    Preset(usize),
    /// A preset chip's check box: turns it on or off.
    PresetToggle(usize),
    DotRow(usize),
    RepoRow(usize),
    /// The detail pane, for the scroll wheel.
    Detail,
    /// A hint chip or a button: acts as if this key was pressed.
    Key(KeyCode),
    /// Everything behind an open dialog.
    Backdrop,
    /// A dialog's scrollable body, for the scroll wheel.
    ModalBody,
    /// A field of an open form.
    FormField(usize),
}

/// Clickable regions of the last frame, topmost last.
#[derive(Debug, Default)]
pub struct HitMap {
    regions: RefCell<Vec<(Rect, Hit)>>,
}

impl HitMap {
    pub fn clear(&self) {
        self.regions.borrow_mut().clear();
    }

    pub fn add(&self, rect: Rect, hit: Hit) {
        if rect.width > 0 && rect.height > 0 {
            self.regions.borrow_mut().push((rect, hit));
        }
    }

    /// The topmost region under a cell.
    pub fn at(&self, column: u16, row: u16) -> Option<Hit> {
        let position = Position::new(column, row);
        self.regions
            .borrow()
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains(position))
            .map(|(_, hit)| hit.clone())
    }

    /// Where `hit` was drawn, if it was.
    pub fn rect_of(&self, hit: &Hit) -> Option<Rect> {
        self.regions
            .borrow()
            .iter()
            .rev()
            .find(|(_, candidate)| candidate == hit)
            .map(|(rect, _)| *rect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_region_added_wins_where_regions_overlap() {
        let map = HitMap::default();
        map.add(Rect::new(0, 0, 10, 10), Hit::Focus(Panel::Tools));
        map.add(Rect::new(2, 2, 3, 1), Hit::ToolItem(4));
        map.add(Rect::new(0, 0, 0, 5), Hit::Detail);
        assert_eq!(map.at(3, 2), Some(Hit::ToolItem(4)));
        assert_eq!(map.at(3, 3), Some(Hit::Focus(Panel::Tools)));
        assert_eq!(map.at(20, 20), None);
        assert_eq!(map.rect_of(&Hit::Detail), None);
        map.clear();
        assert_eq!(map.at(3, 2), None);
    }
}

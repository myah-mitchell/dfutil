//! Single-line text input and the small multi-field forms built from it.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// An editable line of text with a cursor counted in characters.
#[derive(Clone, Debug, Default)]
pub struct Input {
    text: String,
    cursor: usize,
}

impl Input {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            cursor: text.chars().count(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(index, _)| index)
    }

    /// Applies an editing key. Returns whether the key was an edit or a
    /// cursor move, so the caller can pass other keys on.
    pub fn handle(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if control => {
                let end = self.byte_at(self.cursor);
                self.text.replace_range(..end, "");
                self.cursor = 0;
            }
            KeyCode::Char('w') if control => {
                let mut start = self.cursor;
                let chars: Vec<char> = self.text.chars().collect();
                while start > 0 && chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                while start > 0 && !chars[start - 1].is_whitespace() {
                    start -= 1;
                }
                let (from, to) = (self.byte_at(start), self.byte_at(self.cursor));
                self.text.replace_range(from..to, "");
                self.cursor = start;
            }
            KeyCode::Char('a') if control => self.cursor = 0,
            KeyCode::Char('e') if control => self.cursor = self.len(),
            KeyCode::Char(character) if !control && !alt && !character.is_control() => {
                let at = self.byte_at(self.cursor);
                self.text.insert(at, character);
                self.cursor += 1;
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let (from, to) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
                    self.text.replace_range(from..to, "");
                    self.cursor -= 1;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.len() {
                    let (from, to) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
                    self.text.replace_range(from..to, "");
                }
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.len(),
            _ => return false,
        }
        true
    }
}

/// How a form field takes input.
#[derive(Clone, Debug)]
pub enum FieldKind {
    Text,
    /// Typed text shown as asterisks.
    Secret,
    Choice {
        options: Vec<String>,
        selected: usize,
    },
    Check(bool),
}

#[derive(Clone, Debug)]
pub struct Field {
    pub label: String,
    /// One line of help shown while the field has focus.
    pub hint: String,
    pub input: Input,
    pub kind: FieldKind,
}

impl Field {
    pub fn text(label: &str, hint: &str, initial: &str) -> Self {
        Self {
            label: label.into(),
            hint: hint.into(),
            input: Input::new(initial),
            kind: FieldKind::Text,
        }
    }

    pub fn secret(label: &str, hint: &str) -> Self {
        Self {
            kind: FieldKind::Secret,
            ..Self::text(label, hint, "")
        }
    }

    pub fn choice(label: &str, hint: &str, options: &[&str], selected: usize) -> Self {
        Self {
            kind: FieldKind::Choice {
                options: options.iter().map(|option| option.to_string()).collect(),
                selected,
            },
            ..Self::text(label, hint, "")
        }
    }

    pub fn check(label: &str, hint: &str, value: bool) -> Self {
        Self {
            kind: FieldKind::Check(value),
            ..Self::text(label, hint, "")
        }
    }

    /// The text of a text field, or the selected option of a choice.
    pub fn value(&self) -> &str {
        match &self.kind {
            FieldKind::Choice { options, selected } => &options[*selected],
            _ => self.input.text(),
        }
    }

    pub fn checked(&self) -> bool {
        matches!(self.kind, FieldKind::Check(true))
    }
}

/// What a form is for, so the app knows what to do on submit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FormKind {
    AddTool,
    CreatePreset,
    EditVar(String),
}

#[derive(Clone, Debug)]
pub struct Form {
    pub kind: FormKind,
    pub title: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    /// Why the last submit was refused; cleared by the next edit.
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormEvent {
    Pending,
    Cancel,
    Submit,
}

impl Form {
    pub fn new(kind: FormKind, title: &str, fields: Vec<Field>) -> Self {
        Self {
            kind,
            title: title.into(),
            fields,
            focus: 0,
            error: None,
        }
    }

    pub fn value(&self, index: usize) -> &str {
        self.fields[index].value().trim()
    }

    pub fn checked(&self, index: usize) -> bool {
        self.fields[index].checked()
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> FormEvent {
        match key.code {
            KeyCode::Esc => return FormEvent::Cancel,
            KeyCode::Enter => return FormEvent::Submit,
            KeyCode::Tab | KeyCode::Down => {
                self.focus = (self.focus + 1) % self.fields.len();
                return FormEvent::Pending;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focus = (self.focus + self.fields.len() - 1) % self.fields.len();
                return FormEvent::Pending;
            }
            _ => {}
        }
        let field = &mut self.fields[self.focus];
        let edited = match &mut field.kind {
            FieldKind::Text | FieldKind::Secret => field.input.handle(key),
            FieldKind::Choice { options, selected } => match key.code {
                KeyCode::Left => {
                    *selected = (*selected + options.len() - 1) % options.len();
                    true
                }
                KeyCode::Right | KeyCode::Char(' ') => {
                    *selected = (*selected + 1) % options.len();
                    true
                }
                _ => false,
            },
            FieldKind::Check(value) => match key.code {
                KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
                    *value = !*value;
                    true
                }
                _ => false,
            },
        };
        if edited {
            self.error = None;
        }
        FormEvent::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Press)
    }

    #[test]
    fn input_edits_by_character_not_byte() {
        let mut input = Input::new("caf\u{e9}");
        assert_eq!(input.cursor(), 4);
        input.handle(press(KeyCode::Backspace));
        assert_eq!(input.text(), "caf");
        input.handle(press(KeyCode::Home));
        input.handle(press(KeyCode::Char('\u{e9}')));
        assert_eq!(input.text(), "\u{e9}caf");
        input.handle(press(KeyCode::Delete));
        assert_eq!(input.text(), "\u{e9}af");
        input.handle(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input.text(), "af");
        input.handle(press(KeyCode::End));
        input.handle(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert!(input.is_empty());
        assert!(!input.handle(press(KeyCode::F(5))));
    }

    #[test]
    fn form_moves_focus_and_cycles_choices() {
        let mut form = Form::new(
            FormKind::AddTool,
            "t",
            vec![
                Field::text("a", "", ""),
                Field::choice("k", "", &["x", "y"], 0),
                Field::check("c", "", false),
            ],
        );
        form.handle_key(press(KeyCode::Char('h')));
        form.handle_key(press(KeyCode::Tab));
        form.handle_key(press(KeyCode::Right));
        form.handle_key(press(KeyCode::Tab));
        form.handle_key(press(KeyCode::Char(' ')));
        assert_eq!(form.value(0), "h");
        assert_eq!(form.value(1), "y");
        assert!(form.checked(2));
        form.handle_key(press(KeyCode::Tab));
        assert_eq!(form.focus, 0);
        assert_eq!(form.handle_key(press(KeyCode::Enter)), FormEvent::Submit);
        assert_eq!(form.handle_key(press(KeyCode::Esc)), FormEvent::Cancel);
    }
}

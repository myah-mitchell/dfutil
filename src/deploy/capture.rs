//! Writing edits made to a rendered file back into its template.
//!
//! Rendering keeps lines one to one (values have no newlines), so the
//! rendered base and the template line up. The base render is diffed against
//! the edited file: unchanged lines keep their template text, so variables
//! survive; a changed line whose template line used variables gets those
//! variables put back wherever their values still appear; anything else is
//! copied in verbatim, with `{{` escaped so it renders as written.

use super::template::{self, Vars};
use anyhow::{Result, bail};
use similar::{DiffOp, TextDiff};

/// Result of a capture: the new template text and lines worth a look.
#[derive(Debug, PartialEq)]
pub struct Captured {
    pub template: String,
    /// Template line numbers (1-based, in the new template) where a variable
    /// was put back by matching its value, which can be ambiguous.
    pub restored_lines: Vec<usize>,
}

/// Maps `edited` (a changed copy of `template` rendered with `vars`) back
/// onto the template.
pub fn capture(template_text: &str, vars: &Vars, edited: &str, file: &str) -> Result<Captured> {
    let base = template::render(template_text, vars, file)?;
    let template_lines: Vec<&str> = template_text.split_inclusive('\n').collect();
    let base_lines: Vec<&str> = base.split_inclusive('\n').collect();
    if template_lines.len() != base_lines.len() {
        bail!("{file}: rendering changed the line count, so edits can't be mapped back");
    }
    let edited_lines: Vec<&str> = edited.split_inclusive('\n').collect();
    let diff = TextDiff::configure().diff_slices(&base_lines, &edited_lines);

    let mut out = String::with_capacity(edited.len() + 64);
    let mut restored_lines = Vec::new();
    let mut push = |out: &mut String, line: String, restored: bool| {
        out.push_str(&line);
        if restored {
            restored_lines.push(out.matches('\n').count() + usize::from(!line.ends_with('\n')));
        }
    };
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, len, .. } => {
                for line in &template_lines[old_index..old_index + len] {
                    push(&mut out, (*line).to_string(), false);
                }
            }
            DiffOp::Delete { .. } => {}
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for line in &edited_lines[new_index..new_index + new_len] {
                    push(&mut out, template::escape(line), false);
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                for offset in 0..new_len {
                    let line = edited_lines[new_index + offset];
                    let names = if offset < old_len {
                        template::line_vars(template_lines[old_index + offset])
                    } else {
                        Vec::new()
                    };
                    let (text, restored) = restore(line, &names, vars);
                    push(&mut out, text, restored);
                }
            }
        }
    }
    Ok(Captured {
        template: out,
        restored_lines,
    })
}

/// Escapes `line` and puts `{{ name }}` back where each named variable's
/// value appears, longest values first so a short value inside a longer one
/// doesn't win.
fn restore(line: &str, names: &[String], vars: &Vars) -> (String, bool) {
    let mut escaped = template::escape(line);
    let mut pairs: Vec<(&String, &String)> = names
        .iter()
        .filter_map(|name| vars.get(name).map(|value| (name, value)))
        .filter(|(_, value)| !value.is_empty())
        .collect();
    pairs.sort_by_key(|(_, value)| std::cmp::Reverse(value.len()));
    pairs.dedup();
    let mut restored = false;
    // Replace with sentinels first so a later, shorter value can't match
    // inside an already restored reference.
    let mut placeholders = Vec::new();
    for (index, (name, value)) in pairs.iter().enumerate() {
        let sentinel = format!("\u{0}{index}\u{0}");
        let escaped_value = template::escape(value);
        if escaped.contains(&escaped_value) {
            escaped = escaped.replace(&escaped_value, &sentinel);
            placeholders.push((sentinel, format!("{{{{ {name} }}}}")));
            restored = true;
        }
    }
    for (sentinel, reference) in placeholders {
        escaped = escaped.replace(&sentinel, &reference);
    }
    (escaped, restored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        [
            ("email", "me@example.com"),
            ("user", "me"),
            ("palette.mauve", "#cba6f7"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    const TEMPLATE: &str = "[user]\n  email = {{ email }}\n  name = {{ user }}\n[color]\n  accent = {{ palette.mauve }}\n";

    #[test]
    fn unchanged_lines_keep_their_variables() {
        let edited = "[user]\n  email = me@example.com\n  name = me\n[color]\n  accent = #cba6f7\n[core]\n  pager = delta {{x}}\n";
        let captured = capture(TEMPLATE, &vars(), edited, "f").unwrap();
        assert_eq!(
            captured.template,
            format!("{TEMPLATE}[core]\n  pager = delta {{{{{{{{x}}}}\n")
        );
        assert!(captured.restored_lines.is_empty());
        assert_eq!(
            template::render(&captured.template, &vars(), "f").unwrap(),
            edited
        );
    }

    #[test]
    fn changed_lines_get_variables_restored() {
        let edited =
            "[user]\n  email = me@example.com # work\n  name = me\n[color]\n  accent = #cba6f7\n";
        let captured = capture(TEMPLATE, &vars(), edited, "f").unwrap();
        assert!(captured.template.contains("  email = {{ email }} # work\n"));
        assert_eq!(captured.restored_lines, [2]);
        assert_eq!(
            template::render(&captured.template, &vars(), "f").unwrap(),
            edited
        );
    }

    #[test]
    fn longest_value_wins_and_deletions_apply() {
        let template_text = "a = {{ user }} {{ email }}\nremove me\n";
        let edited = "a = me me@example.com!\n";
        let captured = capture(template_text, &vars(), edited, "f").unwrap();
        assert_eq!(captured.template, "a = {{ user }} {{ email }}!\n");
    }
}

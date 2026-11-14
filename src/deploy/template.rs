//! A deliberately small template syntax for dotfiles.
//!
//! `{{ name }}` is replaced by a variable's value, where names are dotted
//! (`palette.base`, `editor`). `{{{{` writes a literal `{{`. Nothing else is
//! special, so a template stays readable as the file it produces, and
//! [`super::capture`] can map edits back onto it line by line. Values can't
//! contain newlines for the same reason.

use crate::config::Workspace;
use anyhow::{Result, bail};
use std::collections::BTreeMap;

/// Variables available to templates, by name.
pub type Vars = BTreeMap<String, String>;

/// Built-in variable names, which `[vars]` can't reuse.
pub const BUILTINS: [&str; 10] = [
    "home",
    "user",
    "hostname",
    "os",
    "arch",
    "wsl",
    "prefix",
    "bin_dir",
    "repo",
    "palette_name",
];

/// A piece of a parsed template line.
#[derive(Debug, PartialEq)]
pub enum Piece<'a> {
    Text(&'a str),
    /// A literal `{{` written as `{{{{`.
    OpenBraces,
    Var(&'a str),
}

/// Splits one line into text and variable references.
pub fn parse_line(line: &str) -> Result<Vec<Piece<'_>>> {
    let mut pieces = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("{{") {
        if start > 0 {
            pieces.push(Piece::Text(&rest[..start]));
        }
        let after = &rest[start + 2..];
        if let Some(remaining) = after.strip_prefix("{{") {
            pieces.push(Piece::OpenBraces);
            rest = remaining;
            continue;
        }
        let Some(end) = after.find("}}") else {
            bail!("`{{{{` without a closing `}}}}` (write `{{{{{{{{` for a literal `{{{{`)");
        };
        let name = after[..end].trim();
        if name.is_empty()
            || !name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-')
            })
        {
            bail!(
                "`{{{{{}}}}}` isn't a variable reference (write `{{{{{{{{` for a literal `{{{{`)",
                &after[..end]
            );
        }
        pieces.push(Piece::Var(name));
        rest = &after[end + 2..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    Ok(pieces)
}

/// Renders a template. Errors name the file and line.
pub fn render(text: &str, vars: &Vars, file: &str) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    for (number, line) in text.split_inclusive('\n').enumerate() {
        let pieces =
            parse_line(line).map_err(|error| anyhow::anyhow!("{file}:{}: {error}", number + 1))?;
        for piece in pieces {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::OpenBraces => out.push_str("{{"),
                Piece::Var(name) => match vars.get(name) {
                    Some(value) => out.push_str(value),
                    None => bail!("{file}:{}: unknown variable `{name}`", number + 1),
                },
            }
        }
    }
    Ok(out)
}

/// Variable names a template line uses.
pub fn line_vars(line: &str) -> Vec<String> {
    parse_line(line)
        .map(|pieces| {
            pieces
                .into_iter()
                .filter_map(|piece| match piece {
                    Piece::Var(name) => Some(name.to_string()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Escapes text so it renders as itself.
pub fn escape(text: &str) -> String {
    text.replace("{{", "{{{{")
}

/// Builds the variable set for a workspace: built-ins, `palette.*` from the
/// active palette, then `[vars]` defaults overridden by the host's values.
/// A variable with no value anywhere is left out, so a template using it
/// fails with "unknown variable" and names it.
pub fn workspace_vars(workspace: &Workspace) -> Result<Vars> {
    let paths = &workspace.paths;
    let mut vars = Vars::new();
    let user = std::env::var("USER")
        .ok()
        .or_else(|| {
            paths
                .home
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_default();
    let hostname = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    let builtins = [
        ("home", paths.home.display().to_string()),
        ("user", user),
        ("hostname", hostname),
        ("os", workspace.platform.os.clone()),
        ("arch", workspace.platform.arch.clone()),
        ("wsl", workspace.platform.wsl.to_string()),
        ("prefix", paths.prefix.display().to_string()),
        ("bin_dir", paths.bin_dir.display().to_string()),
        ("repo", workspace.main_root().path.display().to_string()),
        (
            "palette_name",
            workspace.palette_name().unwrap_or_default().to_string(),
        ),
    ];
    for (name, value) in builtins {
        vars.insert(name.to_string(), value);
    }
    if let Some(palette) = workspace
        .palette_name()
        .and_then(|name| workspace.palettes.get(name))
    {
        for (key, value) in &palette.values {
            if value.contains('\n') {
                bail!(
                    "palette {}: {key}: values can't contain newlines",
                    palette_label(workspace)
                );
            }
            vars.insert(format!("palette.{key}"), value.clone());
        }
    }
    for (name, definition) in &workspace.config.vars {
        if BUILTINS.contains(&name.as_str()) || name.starts_with("palette.") {
            bail!("vars.{name}: `{name}` is a built-in variable name; pick another");
        }
        let value = workspace
            .host
            .vars
            .get(name)
            .map(String::as_str)
            .or(definition.default_value());
        if let Some(value) = value {
            // Covers `[vars]` defaults; host values are checked below with
            // their own file name.
            if value.contains('\n') && !workspace.host.vars.contains_key(name) {
                bail!("dfutil.toml: vars.{name}: the default can't contain newlines");
            }
            vars.insert(name.clone(), value.to_string());
        }
    }
    for (name, value) in &workspace.host.vars {
        if !workspace.config.vars.contains_key(name) {
            bail!(
                "{}: vars.{name}: not declared in [vars] of dfutil.toml",
                workspace.host_path.display()
            );
        }
        if value.contains('\n') {
            bail!(
                "{}: vars.{name}: values can't contain newlines",
                workspace.host_path.display()
            );
        }
    }
    Ok(vars)
}

fn palette_label(workspace: &Workspace) -> String {
    workspace.palette_name().unwrap_or_default().to_string()
}

/// Values of secret variables that are long enough to search for.
pub fn secret_values(workspace: &Workspace) -> Vec<(String, String)> {
    workspace
        .config
        .vars
        .iter()
        .filter(|(_, definition)| definition.is_secret())
        .filter_map(|(name, definition)| {
            let value = workspace
                .host
                .vars
                .get(name)
                .map(String::as_str)
                .or(definition.default_value())?;
            (value.len() >= 4).then(|| (name.clone(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vars {
        [("editor", "nvim"), ("palette.base", "#1e1e2e")]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn renders_variables_and_escapes() {
        let text = "editor = {{ editor }}\nbg={{palette.base}}\nliteral {{{{ x }}\n";
        let out = render(text, &vars(), "f").unwrap();
        assert_eq!(out, "editor = nvim\nbg=#1e1e2e\nliteral {{ x }}\n");
    }

    #[test]
    fn errors_name_file_and_line() {
        let error = render("ok\n{{ nope }}\n", &vars(), "git/.gitconfig.tmpl")
            .unwrap_err()
            .to_string();
        assert_eq!(error, "git/.gitconfig.tmpl:2: unknown variable `nope`");
        let error = render("a {{ b", &vars(), "f").unwrap_err().to_string();
        assert!(error.starts_with("f:1: `{{` without"), "{error}");
        assert!(render("{{ not valid }}", &vars(), "f").is_err());
    }

    #[test]
    fn escape_round_trips() {
        let text = "a {{ b }} {{{{";
        assert_eq!(render(&escape(text), &vars(), "f").unwrap(), text);
        assert_eq!(
            line_vars("x {{ editor }} {{{{ y {{palette.base}}"),
            ["editor", "palette.base"]
        );
    }
}

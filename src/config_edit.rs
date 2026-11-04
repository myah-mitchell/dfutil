//! Edits to `dfapp.toml` and `host.toml` that keep the user's comments and
//! formatting, through `toml_edit`.

use crate::config::{HostConfig, PresetDef, ToolDef, is_valid_name};
use anyhow::{Context, Result, bail};
use std::path::Path;
use toml_edit::{Array, DocumentMut, Item, Table, value};

fn load(path: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

fn save(path: &Path, document: &DocumentMut) -> Result<()> {
    crate::util::write_atomic(path, document.to_string().as_bytes())
}

/// Converts a serializable value into a `toml_edit` table.
fn to_table<T: serde::Serialize>(value: &T) -> Result<Table> {
    let text = toml::to_string(value)?;
    let document: DocumentMut = text.parse()?;
    Ok(document.as_table().clone())
}

fn section<'a>(document: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    let item = document.entry(name).or_insert_with(|| {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    });
    item.as_table_mut().expect("section is a table")
}

/// Adds `[tools.<name>]` to a repo config file. Fails if it already exists.
pub fn add_tool(path: &Path, name: &str, tool: &ToolDef) -> Result<()> {
    if !is_valid_name(name) {
        bail!("`{name}` isn't a valid tool name (letters, digits, `.`, `_`, `-`)");
    }
    let mut document = load(path)?;
    let tools = section(&mut document, "tools");
    if tools.contains_key(name) {
        bail!("{}: tools.{name} already exists", path.display());
    }
    tools.insert(name, Item::Table(to_table(tool)?));
    save(path, &document)
}

/// Adds `[presets.<name>]` to a repo config file. Fails if it already exists.
pub fn add_preset(path: &Path, name: &str, preset: &PresetDef) -> Result<()> {
    if !is_valid_name(name) {
        bail!("`{name}` isn't a valid preset name (letters, digits, `.`, `_`, `-`)");
    }
    let mut document = load(path)?;
    let presets = section(&mut document, "presets");
    if presets.contains_key(name) {
        bail!("{}: presets.{name} already exists", path.display());
    }
    presets.insert(name, Item::Table(to_table(preset)?));
    save(path, &document)
}

/// Writes a new `host.toml`. Fails if one exists.
pub fn create_host(path: &Path, host: &HostConfig) -> Result<()> {
    if path.exists() {
        bail!(
            "{} already exists; edit it, or remove it to start over",
            path.display()
        );
    }
    let mut text =
        String::from("# This host's dfapp choices. See docs/config.md in the dfapp repo.\n");
    text.push_str(&toml::to_string(host)?);
    crate::util::write_atomic(path, text.as_bytes())?;
    // May hold secret variable values.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Adds or removes a string in a top-level array of `host.toml`. Returns
/// whether anything changed.
pub fn host_list(path: &Path, key: &str, item: &str, present: bool) -> Result<bool> {
    let mut document = load(path)?;
    let entry = document.entry(key).or_insert_with(|| value(Array::new()));
    let array = entry
        .as_array_mut()
        .with_context(|| format!("{}: `{key}` must be an array", path.display()))?;
    let position = array
        .iter()
        .position(|existing| existing.as_str() == Some(item));
    let changed = match (present, position) {
        (true, None) => {
            array.push(item);
            true
        }
        (false, Some(index)) => {
            array.remove(index);
            true
        }
        _ => false,
    };
    if changed {
        save(path, &document)?;
    }
    Ok(changed)
}

/// Sets (or with `None`, removes) a top-level string in `host.toml`.
pub fn host_set(path: &Path, key: &str, new_value: Option<&str>) -> Result<()> {
    let mut document = load(path)?;
    match new_value {
        Some(text) => {
            document[key] = value(text);
        }
        None => {
            document.remove(key);
        }
    }
    save(path, &document)
}

/// Sets a template variable's value in `host.toml`'s `[vars]`.
pub fn host_var(path: &Path, name: &str, new_value: &str) -> Result<()> {
    if new_value.contains('\n') {
        bail!("variable values can't contain newlines");
    }
    let mut document = load(path)?;
    section(&mut document, "vars").insert(name, value(new_value));
    save(path, &document)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_comments_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dfapp.toml");
        std::fs::write(
            &path,
            "# my tools\n[tools.rg]\nrepo = \"BurntSushi/ripgrep\" # fast grep\n",
        )
        .unwrap();
        let tool = ToolDef {
            repo: Some("sharkdp/fd".into()),
            pin: Some("10".into()),
            bins: vec!["fd".into()],
            assets: [(
                "linux-x86_64".to_string(),
                "fd-*-x86_64-unknown-linux-musl.tar.gz".to_string(),
            )]
            .into(),
            ..ToolDef::default()
        };
        add_tool(&path, "fd", &tool).unwrap();
        assert!(add_tool(&path, "fd", &tool).is_err());
        add_preset(
            &path,
            "search",
            &PresetDef {
                tools: vec!["rg".into(), "fd".into()],
                ..PresetDef::default()
            },
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my tools") && text.contains("# fast grep"));
        let parsed: crate::config::RepoConfig = toml::from_str(&text).unwrap();
        assert_eq!(parsed.tools["fd"], tool);
        assert_eq!(parsed.presets["search"].tools, ["rg", "fd"]);
    }

    #[test]
    fn host_edits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("host.toml");
        create_host(
            &path,
            &HostConfig {
                repo: "~/dotfiles".into(),
                presets: vec!["base".into()],
                ..HostConfig::default()
            },
        )
        .unwrap();
        assert!(host_list(&path, "tools", "jq", true).unwrap());
        assert!(!host_list(&path, "tools", "jq", true).unwrap());
        assert!(host_list(&path, "presets", "base", false).unwrap());
        host_set(&path, "palette", Some("light")).unwrap();
        host_var(&path, "email", "me@example.com").unwrap();
        let host: HostConfig = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(host.tools, ["jq"]);
        assert!(host.presets.is_empty());
        assert_eq!(host.palette.as_deref(), Some("light"));
        assert_eq!(host.vars["email"], "me@example.com");
        assert!(create_host(&path, &host).is_err());
    }
}

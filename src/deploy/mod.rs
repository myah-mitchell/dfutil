//! Deploying dotfiles packages into `$HOME` as symlinks, one file at a time.
//!
//! Each selected package is a directory in a source repo whose layout mirrors
//! `$HOME`. Secondary sources layer over the main repo: when two roots have
//! the same file in the same package, the later root wins. A `.tmpl` file is
//! rendered into dfutil's rendered directory and the link points there.

pub mod capture;
pub mod template;

use crate::config::{SourceRoot, Workspace};
use crate::selection::Selection;
use crate::state::{DeployState, LinkRecord, RenderRecord};
use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use template::Vars;

/// File name suffix marking a template.
pub const TEMPLATE_SUFFIX: &str = ".tmpl";
/// Per-package file listing globs not to deploy.
pub const LINKIGNORE: &str = ".linkignore";

/// One file a deploy wants in `$HOME`.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// Where the link goes.
    pub target: PathBuf,
    /// The repo file (a template for rendered entries).
    pub source: PathBuf,
    pub package: String,
    pub root: String,
    /// Path inside the package, without `.tmpl`.
    pub relative: PathBuf,
    pub template: bool,
}

/// What happened (or would happen) to one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Change {
    Unchanged,
    Linked,
    /// A rendered file was rewritten (a var or palette change); its link
    /// was already in place.
    Rerendered,
    /// A foreign file was moved aside to `<target>.bak.<timestamp>`.
    BackedUp,
    /// A link dfutil made earlier was removed.
    Pruned,
    Skipped,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct Item {
    pub target: PathBuf,
    pub change: Change,
    pub detail: String,
}

/// Options for a deploy run.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Report what would change without touching anything.
    pub dry_run: bool,
    /// Overwrite rendered files that were edited in place (losing the edits).
    pub force: bool,
}

/// Collects the files the selected packages deploy.
pub fn collect(
    workspace: &Workspace,
    packages: &BTreeSet<String>,
) -> Result<BTreeMap<PathBuf, Entry>> {
    let mut entries = BTreeMap::new();
    for package in packages {
        let roots: Vec<&SourceRoot> = workspace
            .roots
            .iter()
            .filter(|root| root.present && root.path.join(package).is_dir())
            .collect();
        let ignore = linkignore(&roots, package)?;
        for root in roots {
            let package_dir = root.path.join(package);
            for file in walk(&package_dir) {
                let relative = file
                    .strip_prefix(&package_dir)
                    .unwrap_or(&file)
                    .to_path_buf();
                if relative == Path::new(LINKIGNORE) || ignore.is_match(&relative) {
                    continue;
                }
                let name = relative.to_string_lossy();
                let (relative, template) = match name.strip_suffix(TEMPLATE_SUFFIX) {
                    Some(stripped) => (PathBuf::from(stripped), true),
                    None => (relative.clone(), false),
                };
                let target = workspace.paths.home.join(&relative);
                entries.insert(
                    target.clone(),
                    Entry {
                        target,
                        source: file,
                        package: package.clone(),
                        root: root.name.clone(),
                        relative,
                        template,
                    },
                );
            }
        }
    }
    Ok(entries)
}

fn linkignore(roots: &[&SourceRoot], package: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for root in roots {
        let file = root.path.join(package).join(LINKIGNORE);
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            builder.add(
                Glob::new(line)
                    .with_context(|| format!("{}: bad pattern `{line}`", file.display()))?,
            );
        }
    }
    Ok(builder.build()?)
}

/// Regular files and symlinks under `directory`, skipping `.git`.
fn walk(directory: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![directory.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {
                    if entry.file_name() != ".git" {
                        stack.push(path);
                    }
                }
                Ok(_) => files.push(path),
                Err(_) => {}
            }
        }
    }
    files.sort();
    files
}

/// Where a template's rendered output lives.
pub fn rendered_path(workspace: &Workspace, entry: &Entry) -> PathBuf {
    workspace
        .paths
        .rendered_dir
        .join(&entry.root)
        .join(&entry.package)
        .join(&entry.relative)
}

/// Deploys the selection's packages: renders templates, links files,
/// backs up foreign files, and prunes links from packages no longer deployed.
pub fn deploy(
    workspace: &Workspace,
    selection: &Selection,
    state: &mut DeployState,
    options: &Options,
) -> Result<Vec<Item>> {
    let vars = template::workspace_vars(workspace)?;
    let entries = collect(workspace, &selection.packages)?;
    let mut items = Vec::new();
    let roots: Vec<PathBuf> = workspace
        .roots
        .iter()
        .filter(|root| root.present)
        .filter_map(|root| root.path.canonicalize().ok())
        .collect();
    let timestamp = crate::util::timestamp();

    for entry in entries.values() {
        let item = deploy_one(workspace, entry, &vars, &roots, state, options, &timestamp)
            .unwrap_or_else(|error| Item {
                target: entry.target.clone(),
                change: Change::Error,
                detail: format!("{error:#}"),
            });
        items.push(item);
    }

    let stale: Vec<PathBuf> = state
        .links
        .keys()
        .filter(|target| !entries.contains_key(*target))
        .cloned()
        .collect();
    for target in stale {
        let record = state.links[&target].clone();
        let still_ours =
            std::fs::read_link(&target).is_ok_and(|points_at| points_at == record.source);
        if !options.dry_run {
            if still_ours {
                std::fs::remove_file(&target)
                    .with_context(|| format!("removing {}", target.display()))?;
            }
            if let Some(rendered) = &record.rendered {
                std::fs::remove_file(&rendered.output).ok();
            }
            state.links.remove(&target);
        }
        items.push(Item {
            target,
            change: Change::Pruned,
            detail: if still_ours {
                "no longer deployed".into()
            } else {
                "no longer deployed; the file there now isn't dfutil's, left alone".into()
            },
        });
    }
    Ok(items)
}

fn deploy_one(
    workspace: &Workspace,
    entry: &Entry,
    vars: &Vars,
    roots: &[PathBuf],
    state: &mut DeployState,
    options: &Options,
    timestamp: &str,
) -> Result<Item> {
    let item = |change, detail: &str| Item {
        target: entry.target.clone(),
        change,
        detail: detail.to_string(),
    };
    // A parent directory that's a symlink into a repo (a folded stow link)
    // would make the target the repo file itself.
    if let Some(parent) = entry.target.parent()
        && let Ok(resolved) = parent.canonicalize()
        && roots.iter().any(|root| resolved.starts_with(root))
    {
        return Ok(item(
            Change::Skipped,
            "a parent directory is a symlink into a repo; remove that link and deploy again",
        ));
    }

    let mut rerendered = false;
    let (link_to, rendered) = if entry.template {
        let output = rendered_path(workspace, entry);
        let text = std::fs::read_to_string(&entry.source)
            .with_context(|| format!("reading {}", entry.source.display()))?;
        let label = entry
            .source
            .strip_prefix(&workspace.paths.home)
            .unwrap_or(&entry.source)
            .display()
            .to_string();
        let rendered_text = template::render(&text, vars, &label)?;
        let sha256 = crate::util::sha256_hex(rendered_text.as_bytes());
        let on_disk = std::fs::read(&output)
            .ok()
            .map(|bytes| crate::util::sha256_hex(&bytes));
        let recorded = state
            .links
            .get(&entry.target)
            .and_then(|record| record.rendered.as_ref())
            .map(|rendered| rendered.sha256.clone());
        if let (Some(on_disk), Some(recorded)) = (&on_disk, &recorded)
            && on_disk != recorded
            && *on_disk != sha256
            && !options.force
        {
            return Ok(item(
                Change::Skipped,
                "edited in place since the last deploy; run `dfutil capture` to keep the edits, or deploy with --force to discard them",
            ));
        }
        if on_disk.as_deref() != Some(sha256.as_str()) {
            rerendered = true;
            if !options.dry_run {
                // Rendered secrets stay readable by the owner only.
                let private = text.lines().flat_map(template::line_vars).any(|name| {
                    workspace
                        .config
                        .vars
                        .get(&name)
                        .is_some_and(|var| var.is_secret())
                });
                write_preserving_mode(&output, rendered_text.as_bytes(), &entry.source, private)?;
            }
        }
        (
            output.clone(),
            Some(RenderRecord {
                template: entry.source.clone(),
                output,
                sha256,
            }),
        )
    } else {
        (entry.source.clone(), None)
    };

    let record = LinkRecord {
        source: link_to.clone(),
        package: entry.package.clone(),
        root: entry.root.clone(),
        rendered,
    };
    let current = std::fs::read_link(&entry.target).ok();
    let mut change = if rerendered {
        Change::Rerendered
    } else {
        Change::Unchanged
    };
    let mut detail = String::new();
    if current.as_deref() != Some(link_to.as_path()) {
        let exists = std::fs::symlink_metadata(&entry.target).is_ok();
        let ours = state
            .links
            .get(&entry.target)
            .is_some_and(|previous| Some(&previous.source) == current.as_ref());
        change = Change::Linked;
        if exists && !ours {
            let backup = PathBuf::from(format!("{}.bak.{timestamp}", entry.target.display()));
            detail = format!("moved the existing file to {}", backup.display());
            change = Change::BackedUp;
            if !options.dry_run {
                std::fs::rename(&entry.target, &backup)
                    .with_context(|| format!("backing up {}", entry.target.display()))?;
            }
        }
        if !options.dry_run {
            if exists && ours {
                std::fs::remove_file(&entry.target)?;
            }
            if let Some(parent) = entry.target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            std::os::unix::fs::symlink(&link_to, &entry.target).with_context(|| {
                format!(
                    "linking {} -> {}",
                    entry.target.display(),
                    link_to.display()
                )
            })?;
        }
    }
    if !options.dry_run {
        state.links.insert(entry.target.clone(), record);
    }
    Ok(Item {
        target: entry.target.clone(),
        change,
        detail,
    })
}

/// Writes a rendered file with the template's permission bits, so an
/// executable script template renders executable.
/// With `private`, group and other permissions are dropped.
fn write_preserving_mode(
    output: &Path,
    contents: &[u8],
    template: &Path,
    private: bool,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(template)
        .map(|metadata| metadata.permissions().mode() & 0o777)
        .unwrap_or(0o644);
    let mode = if private { mode & 0o700 } else { mode };
    crate::util::write_atomic_with_mode(output, contents, Some(mode))
}

/// A rendered file edited in place, with the template it came from.
#[derive(Clone, Debug, Serialize)]
pub struct Edited {
    pub target: PathBuf,
    pub template: PathBuf,
    pub output: PathBuf,
    /// The template text with the edits mapped back, when mapping worked.
    #[serde(skip)]
    pub captured: Option<String>,
    /// Template lines where a variable was restored by value match.
    pub restored_lines: Vec<usize>,
    pub problem: Option<String>,
}

/// Finds rendered files edited in place and maps each edit onto its
/// template. With `write`, saves the new templates and records the edited
/// files as the current render.
pub fn capture_all(
    workspace: &Workspace,
    state: &mut DeployState,
    write: bool,
) -> Result<Vec<Edited>> {
    let vars = template::workspace_vars(workspace)?;
    let mut results = Vec::new();
    for (target, record) in state.links.iter_mut() {
        let Some(rendered) = &mut record.rendered else {
            continue;
        };
        let Ok(edited) = std::fs::read_to_string(&rendered.output) else {
            continue;
        };
        if crate::util::sha256_hex(edited.as_bytes()) == rendered.sha256 {
            continue;
        }
        let mut result = Edited {
            target: target.clone(),
            template: rendered.template.clone(),
            output: rendered.output.clone(),
            captured: None,
            restored_lines: Vec::new(),
            problem: None,
        };
        let outcome = (|| -> Result<capture::Captured> {
            let template_text = std::fs::read_to_string(&rendered.template)
                .with_context(|| format!("reading {}", rendered.template.display()))?;
            let label = rendered.template.display().to_string();
            let base = template::render(&template_text, &vars, &label)?;
            if crate::util::sha256_hex(base.as_bytes()) != rendered.sha256 {
                anyhow::bail!(
                    "the template or a variable changed since this file was rendered, so the edit can't be mapped back safely; copy the edit into {} by hand",
                    rendered.template.display()
                );
            }
            capture::capture(&template_text, &vars, &edited, &label)
        })();
        match outcome {
            Ok(captured) => {
                if write {
                    crate::util::write_atomic(&rendered.template, captured.template.as_bytes())?;
                    rendered.sha256 = crate::util::sha256_hex(edited.as_bytes());
                }
                result.restored_lines = captured.restored_lines;
                result.captured = Some(captured.template);
            }
            Err(error) => result.problem = Some(format!("{error:#}")),
        }
        results.push(result);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostConfig;
    use crate::config::tests::{SAMPLE, workspace_with};

    fn setup() -> (tempfile::TempDir, Workspace, Selection) {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("dotfiles");
        let write = |relative: &str, text: &str| {
            let path = repo.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("zsh/.zshrc", "plain\n");
        write("zsh/.config/zsh/aliases.zsh", "alias x=y\n");
        write(
            "zsh/.config/zsh/theme.zsh.tmpl",
            "bg={{ palette.base }} ed={{ editor }}\n",
        );
        write("zsh/notes/skip.md", "no\n");
        write("zsh/.linkignore", "# docs\nnotes/*\n");
        let host = HostConfig {
            presets: vec!["base".into()],
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        let selection = Selection::compute(&workspace).unwrap();
        (home, workspace, selection)
    }

    fn changes(items: &[Item]) -> Vec<(String, Change)> {
        items
            .iter()
            .map(|item| {
                (
                    item.target
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    item.change,
                )
            })
            .collect()
    }

    #[test]
    fn renders_holding_secrets_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("dotfiles/zsh");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".netrc.tmpl"), "password {{ token }}\n").unwrap();
        std::fs::write(repo.join(".plain.tmpl"), "editor {{ editor }}\n").unwrap();
        for file in [".netrc.tmpl", ".plain.tmpl"] {
            std::fs::set_permissions(repo.join(file), std::fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        let host = HostConfig {
            presets: vec!["base".into()],
            vars: [("token".to_string(), "hunter22".to_string())].into(),
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        let selection = Selection::compute(&workspace).unwrap();
        let mut state = DeployState::default();
        deploy(&workspace, &selection, &mut state, &Options::default()).unwrap();
        let mode = |name: &str| {
            std::fs::metadata(home.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(".netrc"), 0o600);
        assert_eq!(mode(".plain"), 0o644);
        assert_eq!(
            std::fs::read_to_string(home.path().join(".netrc")).unwrap(),
            "password hunter22\n"
        );
    }

    #[test]
    fn links_renders_backs_up_and_prunes() {
        let (home, workspace, selection) = setup();
        let home = home.path();
        std::fs::write(home.join(".zshrc"), "user's own\n").unwrap();
        let mut state = DeployState::default();

        let preview = deploy(
            &workspace,
            &selection,
            &mut state,
            &Options {
                dry_run: true,
                force: false,
            },
        )
        .unwrap();
        assert!(state.links.is_empty() && std::fs::read_link(home.join(".zshrc")).is_err());
        assert_eq!(preview.len(), 3);

        let items = deploy(&workspace, &selection, &mut state, &Options::default()).unwrap();
        let summary = changes(&items);
        assert!(summary.contains(&(".zshrc".into(), Change::BackedUp)));
        assert!(summary.contains(&("theme.zsh".into(), Change::Linked)));
        assert!(!home.join("notes").exists(), ".linkignore applies");
        assert_eq!(
            std::fs::read_to_string(home.join(".config/zsh/theme.zsh")).unwrap(),
            "bg=#000000 ed=nvim\n"
        );
        let backups = std::fs::read_dir(home)
            .unwrap()
            .flatten()
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".zshrc.bak.")
            })
            .count();
        assert_eq!(backups, 1);

        let again = deploy(&workspace, &selection, &mut state, &Options::default()).unwrap();
        assert!(
            again.iter().all(|item| item.change == Change::Unchanged),
            "{again:?}"
        );

        // Switching palettes re-renders in place.
        let light = Workspace {
            host: HostConfig {
                palette: Some("light".into()),
                ..workspace.host.clone()
            },
            ..workspace.clone()
        };
        let switched = deploy(&light, &selection, &mut state, &Options::default()).unwrap();
        assert!(changes(&switched).contains(&("theme.zsh".into(), Change::Rerendered)));
        assert_eq!(
            std::fs::read_to_string(home.join(".config/zsh/theme.zsh")).unwrap(),
            "bg=#ffffff ed=nvim\n"
        );

        let none = Selection::default();
        let pruned = deploy(&workspace, &none, &mut state, &Options::default()).unwrap();
        assert!(pruned.iter().all(|item| item.change == Change::Pruned));
        assert!(std::fs::symlink_metadata(home.join(".zshrc")).is_err());
        assert!(state.links.is_empty());
    }

    #[test]
    fn edits_to_rendered_files_are_protected_and_captured() {
        let (home, workspace, selection) = setup();
        let mut state = DeployState::default();
        deploy(&workspace, &selection, &mut state, &Options::default()).unwrap();
        let target = home.path().join(".config/zsh/theme.zsh");
        // Editing through the link writes the rendered file.
        std::fs::write(&target, "bg=#000000 ed=nvim\nexport EXTRA=1\n").unwrap();

        let items = deploy(&workspace, &selection, &mut state, &Options::default()).unwrap();
        assert!(changes(&items).contains(&("theme.zsh".into(), Change::Skipped)));

        let preview = capture_all(&workspace, &mut state, false).unwrap();
        assert_eq!(preview.len(), 1);
        let template_path = home.path().join("dotfiles/zsh/.config/zsh/theme.zsh.tmpl");
        assert_eq!(
            std::fs::read_to_string(&template_path).unwrap(),
            "bg={{ palette.base }} ed={{ editor }}\n"
        );

        capture_all(&workspace, &mut state, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(&template_path).unwrap(),
            "bg={{ palette.base }} ed={{ editor }}\nexport EXTRA=1\n"
        );
        let after = deploy(&workspace, &selection, &mut state, &Options::default()).unwrap();
        assert!(
            after.iter().all(|item| item.change == Change::Unchanged),
            "{after:?}"
        );
    }

    #[test]
    fn overlay_sources_win_for_the_same_file() {
        let (home, mut workspace, selection) = setup();
        let overlay = home.path().join("private");
        std::fs::create_dir_all(overlay.join("zsh")).unwrap();
        std::fs::write(overlay.join("zsh/.zshrc"), "private\n").unwrap();
        workspace.roots.push(SourceRoot {
            name: "private".into(),
            path: overlay.clone(),
            def: None,
            present: true,
        });
        let entries = collect(&workspace, &selection.packages).unwrap();
        let zshrc = &entries[&home.path().join(".zshrc")];
        assert_eq!(zshrc.root, "private");
        assert_eq!(zshrc.source, overlay.join("zsh/.zshrc"));
    }
}

//! Turns a host's presets, extra tools, and exclusions into the effective set
//! of tools, packages, and shells for this platform.

use crate::config::{BuildMode, PresetDef, Workspace};
use anyhow::Result;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// What this host gets.
#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Selection {
    pub tools: BTreeSet<String>,
    pub packages: BTreeSet<String>,
    pub shells: BTreeSet<String>,
    /// Why each tool is selected: preset names, `host`, or `required by <tool>`.
    pub reasons: BTreeMap<String, Vec<String>>,
    /// Selected tools and packages left out because their `when` doesn't match.
    pub skipped_for_platform: Vec<String>,
}

impl Selection {
    pub fn compute(workspace: &Workspace) -> Result<Selection> {
        let config = &workspace.config;
        let host = &workspace.host;
        let mut selection = Selection::default();

        let presets = expand_presets(&host.presets, &config.presets);
        for preset_name in &presets {
            let Some(preset) = config.presets.get(preset_name) else {
                continue;
            };
            for tool in &preset.tools {
                selection.add_tool(tool, preset_name);
            }
            selection.packages.extend(preset.packages.iter().cloned());
            selection.shells.extend(preset.shells.iter().cloned());
        }
        for tool in &host.tools {
            selection.add_tool(tool, "host");
        }
        selection.packages.extend(host.packages.iter().cloned());
        for tool in &host.exclude_tools {
            selection.tools.remove(tool);
            selection.reasons.remove(tool);
        }
        for package in &host.exclude_packages {
            selection.packages.remove(package);
        }

        // Pull in requirements transitively (an npm tool needs node).
        let mut queue: Vec<String> = selection.tools.iter().cloned().collect();
        while let Some(tool_name) = queue.pop() {
            let Some(tool) = config.tools.get(&tool_name) else {
                continue;
            };
            for required in &tool.requires {
                if selection.tools.insert(required.clone()) {
                    queue.push(required.clone());
                }
                selection
                    .reasons
                    .entry(required.clone())
                    .or_default()
                    .push(format!("required by {tool_name}"));
            }
        }

        let platform = &workspace.platform;
        let mut skipped = Vec::new();
        selection.tools.retain(|name| {
            let keep = config
                .tools
                .get(name)
                .is_some_and(|tool| tool.when.matches(platform));
            if !keep {
                skipped.push(format!("tool {name}"));
            }
            keep
        });
        selection.packages.retain(|name| {
            let keep = config
                .packages
                .get(name)
                .is_some_and(|package| package.when.matches(platform));
            if !keep {
                skipped.push(format!("package {name}"));
            }
            keep
        });
        selection
            .reasons
            .retain(|name, _| selection.tools.contains(name));
        selection.skipped_for_platform = skipped;

        selection
            .shells
            .extend(config.settings.shells.iter().cloned());
        if selection.shells.is_empty() {
            selection.shells.insert("zsh".into());
        }
        Ok(selection)
    }

    fn add_tool(&mut self, tool: &str, reason: &str) {
        self.tools.insert(tool.to_string());
        self.reasons
            .entry(tool.to_string())
            .or_default()
            .push(reason.to_string());
    }
}

/// The host's presets plus everything they extend, each once, parents first.
pub fn expand_presets(roots: &[String], presets: &BTreeMap<String, PresetDef>) -> Vec<String> {
    fn visit(
        name: &str,
        presets: &BTreeMap<String, PresetDef>,
        seen: &mut BTreeSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(name.to_string()) {
            return;
        }
        if let Some(preset) = presets.get(name) {
            for parent in &preset.extends {
                visit(parent, presets, seen, out);
            }
        }
        out.push(name.to_string());
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for root in roots {
        visit(root, presets, &mut seen, &mut out);
    }
    out
}

/// Finds a cycle in preset `extends`, returning its path when there is one.
pub fn find_preset_cycle(presets: &BTreeMap<String, PresetDef>) -> Option<Vec<String>> {
    fn walk(
        name: &str,
        presets: &BTreeMap<String, PresetDef>,
        stack: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if let Some(position) = stack.iter().position(|entry| entry == name) {
            let mut cycle = stack[position..].to_vec();
            cycle.push(name.to_string());
            return Some(cycle);
        }
        if done.contains(name) {
            return None;
        }
        stack.push(name.to_string());
        if let Some(preset) = presets.get(name) {
            for parent in &preset.extends {
                if let Some(cycle) = walk(parent, presets, stack, done) {
                    return Some(cycle);
                }
            }
        }
        stack.pop();
        done.insert(name.to_string());
        None
    }
    let mut done = BTreeSet::new();
    presets
        .keys()
        .find_map(|name| walk(name, presets, &mut Vec::new(), &mut done))
}

/// Build mode: the host's choice, else `source` if any selected preset asks
/// for it, else the repo default, else prebuilt.
pub fn effective_build_mode(workspace: &Workspace) -> BuildMode {
    if let Some(build) = workspace.host.build {
        return build;
    }
    let presets = expand_presets(&workspace.host.presets, &workspace.config.presets);
    let preset_wants_source = presets.iter().any(|name| {
        workspace
            .config
            .presets
            .get(name)
            .is_some_and(|preset| preset.build == Some(BuildMode::Source))
    });
    if preset_wants_source {
        return BuildMode::Source;
    }
    workspace.config.settings.build.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::HostConfig;
    use crate::config::tests::{SAMPLE, workspace_with};

    #[test]
    fn unions_presets_extras_and_requirements() {
        let home = tempfile::tempdir().unwrap();
        let host = HostConfig {
            presets: vec!["claude".into()],
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        let selection = Selection::compute(&workspace).unwrap();
        let tools: Vec<&str> = selection.tools.iter().map(String::as_str).collect();
        assert_eq!(tools, ["ccstatusline", "node", "ripgrep"]);
        assert_eq!(selection.reasons["node"], ["required by ccstatusline"]);
        assert_eq!(selection.reasons["ripgrep"], ["base"]);
        assert!(selection.packages.contains("zsh"));
        assert!(selection.shells.contains("zsh"));
    }

    #[test]
    fn host_excludes_and_platform_conditions_remove_entries() {
        let home = tempfile::tempdir().unwrap();
        let host = HostConfig {
            presets: vec!["base".into()],
            exclude_tools: vec!["ripgrep".into()],
            packages: vec!["wsl-only".into()],
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        let selection = Selection::compute(&workspace).unwrap();
        assert!(selection.tools.is_empty());
        assert!(!selection.packages.contains("wsl-only"));
        assert_eq!(selection.skipped_for_platform, ["package wsl-only"]);
    }

    #[test]
    fn one_off_selection_without_presets() {
        let home = tempfile::tempdir().unwrap();
        let host = HostConfig {
            tools: vec!["ripgrep".into()],
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        let selection = Selection::compute(&workspace).unwrap();
        assert_eq!(selection.tools.iter().collect::<Vec<_>>(), ["ripgrep"]);
        assert!(selection.packages.is_empty());
    }

    #[test]
    fn preset_build_mode_applies_unless_the_host_overrides() {
        let home = tempfile::tempdir().unwrap();
        let repo = format!("{SAMPLE}\n[presets.cargo]\nbuild = \"source\"\n");
        let host = HostConfig {
            presets: vec!["cargo".into()],
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), &repo, host.clone()).unwrap();
        assert_eq!(effective_build_mode(&workspace), BuildMode::Source);
        let host = HostConfig {
            build: Some(BuildMode::Prebuilt),
            ..host
        };
        let workspace = workspace_with(home.path(), &repo, host).unwrap();
        assert_eq!(effective_build_mode(&workspace), BuildMode::Prebuilt);
    }
}

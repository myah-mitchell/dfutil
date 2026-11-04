//! Configuration files: the content repo's `dfapp.toml`, palettes, and the
//! machine-local `host.toml`, plus loading them into one validated
//! [`Workspace`].

use crate::paths::{Paths, Scope};
use crate::platform::{Platform, When};
use crate::version::Pin;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// File name of the content repo config.
pub const REPO_CONFIG_FILE: &str = "dfapp.toml";

/// Whether tools with a crate are built from source or downloaded prebuilt.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BuildMode {
    #[default]
    Prebuilt,
    Source,
}

/// How a tool is installed.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    /// A GitHub release asset (archive, single binary, or file such as a `.wasm`).
    Github,
    /// Built with `cargo install`.
    Cargo,
    /// A global npm package.
    Npm,
    /// A Python tool installed with `uv tool install`.
    Uv,
    /// Arbitrary install and version commands.
    Script,
}

/// How `dfapp sync` integrates upstream commits.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PullMode {
    /// Rebase local commits on top of upstream.
    #[default]
    Rebase,
    /// Only fast-forward; fail when local commits diverge.
    FfOnly,
}

/// The `[settings]` table of `dfapp.toml`: defaults a host can override.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Default palette name (a file in `palettes/`).
    pub palette: Option<String>,
    pub scope: Option<Scope>,
    pub build: Option<BuildMode>,
    /// Presets `dfapp init` selects when none are given.
    #[serde(default)]
    pub presets: Vec<String>,
    /// Shells to generate init scripts for (`zsh`, `bash`, `nushell`).
    #[serde(default)]
    pub shells: Vec<String>,
    #[serde(default)]
    pub pull: PullMode,
    /// How long cached release listings stay fresh, in seconds.
    pub cache_ttl: Option<u64>,
}

/// A template variable as written in `[vars]`: either a plain default value
/// or a table with more detail.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum VarDef {
    Value(String),
    Detailed(VarDetail),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VarDetail {
    /// Default value; a host must supply one when this is absent.
    pub value: Option<String>,
    /// Shown by `dfapp init` when asking for a value.
    pub description: Option<String>,
    /// Secret values are never printed and are blocked from commits by the
    /// pre-commit hook.
    #[serde(default)]
    pub secret: bool,
}

impl VarDef {
    pub fn default_value(&self) -> Option<&str> {
        match self {
            VarDef::Value(value) => Some(value),
            VarDef::Detailed(detail) => detail.value.as_deref(),
        }
    }

    pub fn is_secret(&self) -> bool {
        matches!(self, VarDef::Detailed(detail) if detail.secret)
    }

    pub fn description(&self) -> Option<&str> {
        match self {
            VarDef::Value(_) => None,
            VarDef::Detailed(detail) => detail.description.as_deref(),
        }
    }
}

/// A secondary content repo layered over the main one.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceDef {
    /// Clone URL. Private repos use the user's own git credentials (SSH agent
    /// or credential helper).
    pub url: String,
    pub branch: Option<String>,
    /// Local checkout to use instead of `~/.local/share/dfapp/sources/<name>`.
    pub path: Option<String>,
}

/// A dotfiles package: a directory in a source repo mirrored into `$HOME`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PackageDef {
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "When::is_empty")]
    pub when: When,
}

/// One installable tool.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ToolDef {
    /// Defaults to `github` when `repo` is set, otherwise must be given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// GitHub `owner/name`, for release downloads and version lookups.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Version pin; see [`Pin`]. Empty means the newest stable release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// crates.io crate name, enabling source builds.
    #[serde(rename = "crate", skip_serializing_if = "Option::is_none")]
    pub crate_name: Option<String>,
    /// npm or PyPI package name (defaults to the tool name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Release asset glob per platform key (`linux-x86_64`, `linux`, `any`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, String>,
    /// Binaries to link into the bin directory: `name`, or `name=path/in/archive`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bins: Vec<String>,
    /// Other files to link, as prefix-relative destination = path in the archive.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    /// Shell init generators: shell name = command whose stdout is the script.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub init: BTreeMap<String, String>,
    /// Script kind: install command (run with `sh -c`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install: Option<String>,
    /// Script kind: command printing the installed version.
    #[serde(rename = "version", skip_serializing_if = "Option::is_none")]
    pub version_command: Option<String>,
    /// Script kind: command removing the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uninstall: Option<String>,
    /// Command run after a successful install of any kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post_install: Option<String>,
    /// Other tools this one needs installed first (an npm package needs `node`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    #[serde(default, skip_serializing_if = "When::is_empty")]
    pub when: When,
}

impl ToolDef {
    pub fn kind(&self) -> Option<ToolKind> {
        self.kind
            .or_else(|| self.repo.as_ref().map(|_| ToolKind::Github))
            .or_else(|| self.crate_name.as_ref().map(|_| ToolKind::Cargo))
    }

    pub fn pin(&self) -> Result<Pin> {
        Pin::parse(self.pin.as_deref().unwrap_or(""))
    }

    /// The asset glob for this platform, trying the most specific key first.
    pub fn asset_for(&self, platform: &Platform) -> Option<&str> {
        platform
            .asset_keys()
            .iter()
            .find_map(|key| self.assets.get(key))
            .map(String::as_str)
    }

    /// A short hash of the recipe, so a change to how a tool is installed
    /// triggers a reinstall even when the version didn't change.
    pub fn fingerprint(&self) -> String {
        // The description and pin don't change how a version is installed;
        // a pin change is caught by comparing resolved versions instead.
        let recipe = ToolDef {
            description: None,
            pin: None,
            ..self.clone()
        };
        let serialized = serde_json::to_vec(&recipe).unwrap_or_default();
        crate::util::sha256_hex(&serialized)[..16].to_string()
    }

    /// Parsed `bins` entries as (link name, optional path inside the install).
    pub fn bin_specs(&self, tool_name: &str) -> Vec<(String, Option<String>)> {
        if self.bins.is_empty() {
            return vec![(tool_name.to_string(), None)];
        }
        self.bins
            .iter()
            .map(|entry| match entry.split_once('=') {
                Some((name, path)) => (name.trim().to_string(), Some(path.trim().to_string())),
                None => (entry.trim().to_string(), None),
            })
            .collect()
    }
}

/// A named group of tools and packages.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PresetDef {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Other presets included in this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extends: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// Extra shells to generate init scripts for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<String>,
    /// Build mode this preset asks for (a `cargo` preset sets `source`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildMode>,
}

/// Contents of a `dfapp.toml`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub vars: BTreeMap<String, VarDef>,
    #[serde(default)]
    pub sources: BTreeMap<String, SourceDef>,
    #[serde(default)]
    pub packages: BTreeMap<String, PackageDef>,
    #[serde(default)]
    pub tools: BTreeMap<String, ToolDef>,
    #[serde(default)]
    pub presets: BTreeMap<String, PresetDef>,
}

impl RepoConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

/// Contents of the machine-local `host.toml`: this host's choices.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Path to the main content repo checkout.
    pub repo: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<String>,
    /// Tools added on top of the presets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Tools removed from what the presets select.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_packages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<BuildMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub palette: Option<String>,
    /// Values for template variables, overriding repo defaults.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub vars: BTreeMap<String, String>,
    /// Extra secondary sources for this host only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, SourceDef>,
}

impl HostConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| {
            format!(
                "reading {} (run `dfapp init` first on a new host)",
                path.display()
            )
        })?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn repo_path(&self, home: &Path) -> PathBuf {
        crate::util::expand_tilde(&self.repo, home)
    }
}

/// A palette file: flat or nested string values, exposed to templates as
/// `palette.<dotted.key>`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Palette {
    pub name: String,
    pub values: BTreeMap<String, String>,
}

impl Palette {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let mut values = BTreeMap::new();
        flatten("", &toml::Value::Table(table), &mut values)
            .with_context(|| format!("in {}", path.display()))?;
        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_default();
        Ok(Self { name, values })
    }
}

fn flatten(prefix: &str, value: &toml::Value, out: &mut BTreeMap<String, String>) -> Result<()> {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let joined = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                flatten(&joined, child, out)?;
            }
        }
        toml::Value::String(text) => {
            out.insert(prefix.to_string(), text.clone());
        }
        toml::Value::Integer(number) => {
            out.insert(prefix.to_string(), number.to_string());
        }
        toml::Value::Float(number) => {
            out.insert(prefix.to_string(), number.to_string());
        }
        toml::Value::Boolean(flag) => {
            out.insert(prefix.to_string(), flag.to_string());
        }
        _ => bail!("palette key `{prefix}` must be a string, number, or boolean"),
    }
    Ok(())
}

/// One content repo on disk: the main repo or a secondary source.
#[derive(Clone, Debug, Serialize)]
pub struct SourceRoot {
    /// `main` for the main repo, otherwise the source's name.
    pub name: String,
    pub path: PathBuf,
    /// The source definition (absent for the main repo).
    pub def: Option<SourceDef>,
    /// False when a secondary source hasn't been cloned yet.
    pub present: bool,
}

/// Everything loaded and merged for one host: paths, platform, the merged
/// repo config, the host config, and palettes.
#[derive(Clone, Debug)]
pub struct Workspace {
    pub paths: Paths,
    pub platform: Platform,
    pub host_path: PathBuf,
    pub host: HostConfig,
    /// Main repo config with secondary sources layered on top.
    pub config: RepoConfig,
    /// Main repo first, then secondary sources in name order.
    pub roots: Vec<SourceRoot>,
    pub palettes: BTreeMap<String, Palette>,
    /// Which file defined each tool, for error messages and edits.
    pub tool_origins: BTreeMap<String, PathBuf>,
    /// Which file defined each preset.
    pub preset_origins: BTreeMap<String, PathBuf>,
    /// Non-fatal problems found while loading (an uncloned source, an overlay
    /// replacing a definition).
    pub notes: Vec<String>,
}

impl Workspace {
    /// Loads the host config at `host_path` and everything it points at.
    /// `home` and `scope` default to the environment and the host's choice.
    pub fn load(host_path: &Path, home_override: Option<&Path>) -> Result<Self> {
        let host = HostConfig::load(host_path)?;
        let probe = match home_override {
            Some(home) => Paths::for_home(home, Scope::User),
            None => Paths::from_env(Scope::User),
        };
        let main_path = host.repo_path(&probe.home);
        let main_file = main_path.join(REPO_CONFIG_FILE);
        let main_config = RepoConfig::load(&main_file)?;
        let scope = host
            .scope
            .or(main_config.settings.scope)
            .unwrap_or_default();
        let paths = match home_override {
            Some(home) => Paths::for_home(home, scope),
            None => Paths::from_env(scope),
        };
        Self::assemble(
            paths,
            Platform::detect(),
            host_path.to_path_buf(),
            host,
            main_config,
        )
    }

    /// Builds a workspace from already-parsed parts. Separate from [`load`]
    /// so tests can supply a platform.
    pub fn assemble(
        paths: Paths,
        platform: Platform,
        host_path: PathBuf,
        host: HostConfig,
        main_config: RepoConfig,
    ) -> Result<Self> {
        let main_path = host.repo_path(&paths.home);
        let main_file = main_path.join(REPO_CONFIG_FILE);
        let mut notes = Vec::new();
        let mut tool_origins = BTreeMap::new();
        let mut preset_origins = BTreeMap::new();
        for name in main_config.tools.keys() {
            tool_origins.insert(name.clone(), main_file.clone());
        }
        for name in main_config.presets.keys() {
            preset_origins.insert(name.clone(), main_file.clone());
        }

        let mut roots = vec![SourceRoot {
            name: "main".into(),
            path: main_path.clone(),
            def: None,
            present: true,
        }];
        let mut source_defs = main_config.sources.clone();
        source_defs.extend(host.sources.clone());
        for (name, def) in &source_defs {
            if name == "main" {
                bail!(
                    "{}: sources.main: `main` is reserved for the main repo",
                    main_file.display()
                );
            }
            let path = match &def.path {
                Some(path) => crate::util::expand_tilde(path, &paths.home),
                None => paths.sources_dir.join(name),
            };
            let present = path.join(".git").exists() || path.join(REPO_CONFIG_FILE).exists();
            if !present {
                notes.push(format!(
                    "source `{name}` isn't cloned at {} yet; run `dfapp sync` to fetch it",
                    path.display()
                ));
            }
            roots.push(SourceRoot {
                name: name.clone(),
                path,
                def: Some(def.clone()),
                present,
            });
        }

        let mut config = main_config;
        for root in roots.iter().skip(1).filter(|root| root.present) {
            let file = root.path.join(REPO_CONFIG_FILE);
            if !file.exists() {
                continue;
            }
            let overlay = RepoConfig::load(&file)?;
            merge_overlay(
                &mut config,
                overlay,
                &file,
                &root.name,
                &mut tool_origins,
                &mut preset_origins,
                &mut notes,
            );
        }

        let mut palettes = BTreeMap::new();
        for root in roots.iter().filter(|root| root.present) {
            let directory = root.path.join("palettes");
            let Ok(entries) = std::fs::read_dir(&directory) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.extension()
                        .is_some_and(|extension| extension == "toml")
                })
                .collect();
            files.sort();
            for file in files {
                let palette = Palette::load(&file)?;
                palettes.insert(palette.name.clone(), palette);
            }
        }

        let workspace = Self {
            paths,
            platform,
            host_path,
            host,
            config,
            roots,
            palettes,
            tool_origins,
            preset_origins,
            notes,
        };
        let problems = workspace.validate();
        if !problems.is_empty() {
            bail!("configuration problems:\n  {}", problems.join("\n  "));
        }
        Ok(workspace)
    }

    pub fn main_root(&self) -> &SourceRoot {
        &self.roots[0]
    }

    pub fn main_config_file(&self) -> PathBuf {
        self.main_root().path.join(REPO_CONFIG_FILE)
    }

    pub fn build_mode(&self) -> BuildMode {
        crate::selection::effective_build_mode(self)
    }

    pub fn palette_name(&self) -> Option<&str> {
        self.host
            .palette
            .as_deref()
            .or(self.config.settings.palette.as_deref())
    }

    pub fn origin_of_tool(&self, name: &str) -> String {
        self.tool_origins
            .get(name)
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| REPO_CONFIG_FILE.to_string())
    }

    /// Checks references between presets, tools, packages, pins, palettes,
    /// and the host selection. Returns one message per problem.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        let host_file = self.host_path.display().to_string();
        let config = &self.config;

        for (name, tool) in &config.tools {
            let origin = self.origin_of_tool(name);
            if let Err(error) = tool.pin() {
                problems.push(format!("{origin}: tools.{name}.pin: {error}"));
            }
            match tool.kind() {
                None => problems.push(format!(
                    "{origin}: tools.{name}: set `kind`, or `repo` for a GitHub release tool"
                )),
                Some(ToolKind::Github) => {
                    if tool.repo.is_none() {
                        problems.push(format!("{origin}: tools.{name}: github tools need `repo`"));
                    }
                    if tool.assets.is_empty() {
                        problems.push(format!(
                            "{origin}: tools.{name}: github tools need an `assets` table"
                        ));
                    }
                }
                Some(ToolKind::Cargo) => {
                    if tool.crate_name.is_none() {
                        problems.push(format!("{origin}: tools.{name}: cargo tools need `crate`"));
                    }
                }
                Some(ToolKind::Script) => {
                    if tool.install.is_none() {
                        problems.push(format!(
                            "{origin}: tools.{name}: script tools need `install`"
                        ));
                    }
                }
                Some(ToolKind::Npm | ToolKind::Uv) => {}
            }
            if let Some(repo) = &tool.repo
                && !is_valid_repo_slug(repo)
            {
                problems.push(format!(
                    "{origin}: tools.{name}.repo: `{repo}` isn't an owner/name slug"
                ));
            }
            for required in &tool.requires {
                if !config.tools.contains_key(required) {
                    problems.push(format!(
                        "{origin}: tools.{name}.requires: unknown tool `{required}`"
                    ));
                }
            }
            for shell in tool.init.keys() {
                if !crate::install::init::SHELLS.contains(&shell.as_str()) {
                    problems.push(format!("{origin}: tools.{name}.init.{shell}: unknown shell (use zsh, bash, or nushell)"));
                }
            }
            if !is_valid_name(name) {
                problems.push(format!(
                    "{origin}: tools.{name}: names may only use letters, digits, `.`, `_`, and `-`"
                ));
            }
            // Link names and paths come from a repo that may be shared; keep
            // every link inside the prefix and every source inside the install.
            for (link_name, path) in tool.bin_specs(name) {
                if !is_valid_name(&link_name) {
                    problems.push(format!(
                        "{origin}: tools.{name}.bins: `{link_name}` isn't a plain file name"
                    ));
                }
                if let Some(path) = path
                    && !is_contained_relative(&path)
                {
                    problems.push(format!(
                        "{origin}: tools.{name}.bins: `{path}` must be a relative path without `..`"
                    ));
                }
            }
            for (destination, path) in &tool.links {
                for (key, value) in [("destination", destination), ("path", path)] {
                    if !is_contained_relative(value) {
                        problems.push(format!(
                            "{origin}: tools.{name}.links: {key} `{value}` must be a relative path without `..`"
                        ));
                    }
                }
            }
        }

        for (name, preset) in &config.presets {
            let origin = self
                .preset_origins
                .get(name)
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            for parent in &preset.extends {
                if !config.presets.contains_key(parent) {
                    problems.push(format!(
                        "{origin}: presets.{name}.extends: unknown preset `{parent}`"
                    ));
                }
            }
            for tool in &preset.tools {
                if !config.tools.contains_key(tool) {
                    problems.push(format!(
                        "{origin}: presets.{name}.tools: unknown tool `{tool}`"
                    ));
                }
            }
            for package in &preset.packages {
                if !config.packages.contains_key(package) {
                    problems.push(format!(
                        "{origin}: presets.{name}.packages: unknown package `{package}`"
                    ));
                }
            }
            for shell in &preset.shells {
                if !crate::install::init::SHELLS.contains(&shell.as_str()) {
                    problems.push(format!("{origin}: presets.{name}.shells: unknown shell `{shell}` (use zsh, bash, or nushell)"));
                }
            }
        }
        for shell in &config.settings.shells {
            if !crate::install::init::SHELLS.contains(&shell.as_str()) {
                problems.push(format!(
                    "settings.shells: unknown shell `{shell}` (use zsh, bash, or nushell)"
                ));
            }
        }
        if let Some(cycle) = crate::selection::find_preset_cycle(&config.presets) {
            problems.push(format!("presets: `extends` cycle: {}", cycle.join(" -> ")));
        }

        for preset in self.host.presets.iter().chain(&config.settings.presets) {
            if !config.presets.contains_key(preset) {
                problems.push(format!("{host_file}: presets: unknown preset `{preset}`"));
            }
        }
        for tool in self.host.tools.iter().chain(&self.host.exclude_tools) {
            if !config.tools.contains_key(tool) {
                problems.push(format!("{host_file}: tools: unknown tool `{tool}`"));
            }
        }
        for package in self.host.packages.iter().chain(&self.host.exclude_packages) {
            if !config.packages.contains_key(package) {
                problems.push(format!(
                    "{host_file}: packages: unknown package `{package}`"
                ));
            }
        }
        for name in config.packages.keys() {
            if !is_valid_name(name) {
                problems.push(format!(
                    "packages.{name}: names may only use letters, digits, `.`, `_`, and `-`"
                ));
            }
        }
        if let Some(palette) = self.palette_name()
            && !self.palettes.is_empty()
            && !self.palettes.contains_key(palette)
        {
            problems.push(format!(
                "palette `{palette}` has no palettes/{palette}.toml (available: {})",
                self.palettes.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }

        // Selected tools must have an asset for this platform when installed prebuilt.
        if problems.is_empty()
            && let Ok(selection) = crate::selection::Selection::compute(self)
        {
            let build = self.build_mode();
            for name in &selection.tools {
                let tool = &config.tools[name];
                let builds_from_source = build == BuildMode::Source && tool.crate_name.is_some();
                if tool.kind() == Some(ToolKind::Github)
                    && !builds_from_source
                    && tool.asset_for(&self.platform).is_none()
                {
                    problems.push(format!(
                        "{}: tools.{name}.assets: no entry for {} (tried {})",
                        self.origin_of_tool(name),
                        self.platform.asset_keys()[0],
                        self.platform.asset_keys().join(", ")
                    ));
                }
            }
        }
        problems
    }
}

/// Applies a secondary source's config over the merged config. Whole entries
/// are replaced by name; a replaced definition is noted.
fn merge_overlay(
    config: &mut RepoConfig,
    overlay: RepoConfig,
    file: &Path,
    source: &str,
    tool_origins: &mut BTreeMap<String, PathBuf>,
    preset_origins: &mut BTreeMap<String, PathBuf>,
    notes: &mut Vec<String>,
) {
    for (name, tool) in overlay.tools {
        if config.tools.contains_key(&name) {
            notes.push(format!("source `{source}` overrides tool `{name}`"));
        }
        tool_origins.insert(name.clone(), file.to_path_buf());
        config.tools.insert(name, tool);
    }
    for (name, preset) in overlay.presets {
        if config.presets.contains_key(&name) {
            notes.push(format!("source `{source}` overrides preset `{name}`"));
        }
        preset_origins.insert(name.clone(), file.to_path_buf());
        config.presets.insert(name, preset);
    }
    config.vars.extend(overlay.vars);
    config.packages.extend(overlay.packages);
    let settings = overlay.settings;
    if settings.palette.is_some() {
        config.settings.palette = settings.palette;
    }
    for shell in settings.shells {
        if !config.settings.shells.contains(&shell) {
            config.settings.shells.push(shell);
        }
    }
    if !overlay.sources.is_empty() {
        notes.push(format!(
            "source `{source}` declares its own [sources]; nested sources are ignored"
        ));
    }
}

/// A non-empty relative path with no `..`, `.`, or root components.
pub fn is_contained_relative(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

/// Tool, preset, and package names end up in paths and file names, so they
/// stay to a safe character set.
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

fn is_valid_repo_slug(repo: &str) -> bool {
    match repo.split_once('/') {
        Some((owner, name)) => is_valid_name(owner) && is_valid_name(name),
        None => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn linux() -> Platform {
        Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            wsl: false,
        }
    }

    pub const SAMPLE: &str = r#"
[settings]
palette = "dark"
presets = ["base"]

[vars]
editor = "nvim"
email = { description = "Git email" }
token = { value = "", secret = true }

[packages]
zsh = {}
wsl-only = { when = { wsl = true } }

[tools.ripgrep]
repo = "BurntSushi/ripgrep"
pin = "14"
crate = "ripgrep"
bins = ["rg"]
assets.linux-x86_64 = "ripgrep-*-x86_64-unknown-linux-musl.tar.gz"

[tools.node]
repo = "nodejs/node"
assets.linux = "node-*-linux-x64.tar.xz"
bins = ["node=bin/node", "npm=bin/npm"]

[tools.ccstatusline]
kind = "npm"
pin = "2.0.0"
requires = ["node"]

[presets.base]
tools = ["ripgrep"]
packages = ["zsh"]

[presets.claude]
extends = ["base"]
tools = ["ccstatusline"]
"#;

    pub fn workspace_with(home: &Path, repo_toml: &str, host: HostConfig) -> Result<Workspace> {
        let repo = home.join("dotfiles");
        std::fs::create_dir_all(repo.join("palettes")).unwrap();
        std::fs::write(repo.join(REPO_CONFIG_FILE), repo_toml).unwrap();
        std::fs::write(
            repo.join("palettes/dark.toml"),
            "base = \"#000000\"\n[theme]\nbat = \"Dark\"\n",
        )
        .unwrap();
        std::fs::write(
            repo.join("palettes/light.toml"),
            "base = \"#ffffff\"\n[theme]\nbat = \"Light\"\n",
        )
        .unwrap();
        let host = HostConfig {
            repo: repo.display().to_string(),
            ..host
        };
        let config = RepoConfig::load(&repo.join(REPO_CONFIG_FILE))?;
        Workspace::assemble(
            Paths::for_home(home, Scope::User),
            linux(),
            home.join(".config/dfapp/host.toml"),
            host,
            config,
        )
    }

    #[test]
    fn rejects_links_and_bins_that_escape() {
        let home = tempfile::tempdir().unwrap();
        let broken = SAMPLE
            .replace("bins = [\"rg\"]", "bins = [\"rg=../../etc/rg\", \"../x\"]")
            .replace(
                "[tools.ccstatusline]",
                "[tools.plugin]\nkind = \"script\"\ninstall = \"true\"\nlinks.\"../.bashrc\" = \"x\"\nlinks.\"share/ok\" = \"/etc/passwd\"\n\n[tools.ccstatusline]",
            );
        let error = workspace_with(home.path(), &broken, HostConfig::default())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("tools.ripgrep.bins: `../../etc/rg`"),
            "{error}"
        );
        assert!(
            error.contains("tools.ripgrep.bins: `../x` isn't a plain file name"),
            "{error}"
        );
        assert!(error.contains("links: destination `../.bashrc`"), "{error}");
        assert!(error.contains("links: path `/etc/passwd`"), "{error}");
        assert!(is_contained_relative("share/zellij/plugins/a.wasm"));
        assert!(!is_contained_relative("./a") && !is_contained_relative(""));
    }

    #[test]
    fn loads_a_valid_config() {
        let home = tempfile::tempdir().unwrap();
        let host = HostConfig {
            presets: vec!["claude".into()],
            ..HostConfig::default()
        };
        let workspace = workspace_with(home.path(), SAMPLE, host).unwrap();
        assert_eq!(workspace.config.tools.len(), 3);
        assert_eq!(workspace.palettes["dark"].values["theme.bat"], "Dark");
        assert!(workspace.config.vars["token"].is_secret());
    }

    #[test]
    fn reports_unknown_references_with_file_and_key() {
        let home = tempfile::tempdir().unwrap();
        let broken = SAMPLE.replace("tools = [\"ccstatusline\"]", "tools = [\"nope\"]");
        let host = HostConfig {
            presets: vec!["missing".into()],
            ..HostConfig::default()
        };
        let error = workspace_with(home.path(), &broken, host)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("dfapp.toml: presets.claude.tools: unknown tool `nope`"),
            "{error}"
        );
        assert!(
            error.contains("host.toml: presets: unknown preset `missing`"),
            "{error}"
        );
    }

    #[test]
    fn reports_bad_pins_and_missing_assets() {
        let home = tempfile::tempdir().unwrap();
        let broken = SAMPLE
            .replace("pin = \"14\"", "pin = \"1.*.3\"")
            .replace("assets.linux-x86_64", "assets.linux-aarch64");
        let host = HostConfig {
            presets: vec!["base".into()],
            ..HostConfig::default()
        };
        let error = workspace_with(home.path(), &broken, host)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("tools.ripgrep.pin: `*` is only allowed at the end"),
            "{error}"
        );
        // Missing-asset checks run once references are clean.
        let only_asset = SAMPLE.replace("assets.linux-x86_64", "assets.linux-aarch64");
        let host = HostConfig {
            presets: vec!["base".into()],
            ..HostConfig::default()
        };
        let error = workspace_with(home.path(), &only_asset, host)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("tools.ripgrep.assets: no entry for linux-x86_64"),
            "{error}"
        );
    }

    #[test]
    fn unknown_fields_are_rejected_with_the_file_name() {
        let home = tempfile::tempdir().unwrap();
        let broken = SAMPLE.replace("pin = \"14\"", "pinn = \"14\"");
        let error = format!(
            "{:#}",
            workspace_with(home.path(), &broken, HostConfig::default()).unwrap_err()
        );
        assert!(error.contains("dfapp.toml"), "{error}");
        assert!(error.contains("pinn"), "{error}");
    }

    #[test]
    fn detects_preset_cycles() {
        let home = tempfile::tempdir().unwrap();
        let cyclic =
            format!("{SAMPLE}\n[presets.a]\nextends = [\"b\"]\n[presets.b]\nextends = [\"a\"]\n");
        let error = workspace_with(home.path(), &cyclic, HostConfig::default())
            .unwrap_err()
            .to_string();
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn fingerprint_changes_with_the_recipe() {
        let config: RepoConfig = toml::from_str(SAMPLE).unwrap();
        let mut tool = config.tools["ripgrep"].clone();
        let before = tool.fingerprint();
        tool.bins.push("rga".into());
        assert_ne!(before, tool.fingerprint());
    }
}

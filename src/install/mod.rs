//! Resolving which version of a tool to install and installing it.
//!
//! Every method except `uv` and `script` stages the install in a temporary
//! directory beside the final one and renames it into place, so a failed
//! install never leaves a half-written version behind. Versions live in
//! `<tools_dir>/<tool>/<version>/` and binaries are symlinked into the bin
//! directory; the previous version is kept for `dfapp rollback`.

pub mod archive;
pub mod init;

use crate::config::{BuildMode, ToolDef, ToolKind};
use crate::paths::Paths;
use crate::platform::Platform;
use crate::remote::{
    Asset, ChecksumSource, Remote, compile_glob, find_asset, find_checksum_source, parse_checksum,
};
use crate::state::{InstalledVersion, Method, ToolRecord};
use crate::version::{Pin, select, version_from_output};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The concrete thing to install for a tool.
#[derive(Clone, Debug, Serialize)]
pub struct Resolved {
    pub method: Method,
    /// Version or tag to install; `None` when it can't be known up front
    /// (a script tool with no repo to check).
    pub version: Option<String>,
    /// Where it comes from (`owner/name`, package, or crate).
    pub source: Option<String>,
    #[serde(skip)]
    pub asset: Option<Asset>,
    #[serde(skip)]
    pub checksum: Option<ChecksumSource>,
    /// Things worth telling the user (a fallback from source to prebuilt).
    pub notes: Vec<String>,
}

/// Works out the version and download for a tool under the given build mode.
pub fn resolve(
    remote: &Remote,
    platform: &Platform,
    name: &str,
    tool: &ToolDef,
    build: BuildMode,
) -> Result<Resolved> {
    let pin = tool.pin()?;
    let kind = tool
        .kind()
        .with_context(|| format!("tool {name} has no kind"))?;
    let package = tool.package.clone().unwrap_or_else(|| name.to_string());
    let wants_source =
        kind == ToolKind::Cargo || (build == BuildMode::Source && tool.crate_name.is_some());
    let mut notes = Vec::new();

    if wants_source {
        let crate_name = tool.crate_name.clone().unwrap_or_else(|| name.to_string());
        match resolve_crate(remote, &crate_name, &pin) {
            Ok(version) => {
                return Ok(Resolved {
                    method: Method::Cargo,
                    version: Some(version),
                    source: Some(crate_name),
                    asset: None,
                    checksum: None,
                    notes,
                });
            }
            Err(error) if kind == ToolKind::Github => {
                notes.push(format!(
                    "building from source isn't possible ({error:#}); using the prebuilt release"
                ));
            }
            Err(error) => return Err(error),
        }
    }

    match kind {
        ToolKind::Github | ToolKind::Cargo => {
            let repo = tool
                .repo
                .as_deref()
                .with_context(|| format!("tool {name} has no repo"))?;
            let pattern = tool.asset_for(platform).with_context(|| {
                format!(
                    "tool {name} has no asset for {}-{}",
                    platform.os, platform.arch
                )
            })?;
            let matcher = compile_glob(pattern)?;
            let releases = remote.github_releases(repo)?;
            let release = select(
                &pin,
                &releases,
                |release| &release.candidate,
                |release| find_asset(release, &matcher).is_some(),
            )
            .with_context(|| {
                format!(
                    "no release of {repo} matches pin `{pin}` with an asset matching `{pattern}`"
                )
            })?;
            let asset = find_asset(release, &matcher).cloned();
            let checksum = asset
                .as_ref()
                .and_then(|asset| find_checksum_source(release, &asset.name));
            Ok(Resolved {
                method: Method::Prebuilt,
                version: Some(release.candidate.tag.clone()),
                source: Some(repo.to_string()),
                asset,
                checksum,
                notes,
            })
        }
        ToolKind::Npm => {
            let candidates = remote.npm_versions(&package)?;
            let chosen = select(&pin, &candidates, |candidate| candidate, |_| true)
                .with_context(|| format!("no npm version of {package} matches pin `{pin}`"))?;
            Ok(Resolved {
                method: Method::Npm,
                version: Some(chosen.tag.clone()),
                source: Some(package),
                asset: None,
                checksum: None,
                notes,
            })
        }
        ToolKind::Uv => {
            let candidates = remote.pypi_versions(&package)?;
            let chosen = select(&pin, &candidates, |candidate| candidate, |_| true)
                .with_context(|| format!("no PyPI version of {package} matches pin `{pin}`"))?;
            Ok(Resolved {
                method: Method::Uv,
                version: Some(chosen.tag.clone()),
                source: Some(package),
                asset: None,
                checksum: None,
                notes,
            })
        }
        ToolKind::Script => {
            let version = match &tool.repo {
                Some(repo) => {
                    let releases = remote.github_releases(repo)?;
                    select(&pin, &releases, |release| &release.candidate, |_| true)
                        .map(|release| release.candidate.tag.clone())
                }
                None => None,
            };
            Ok(Resolved {
                method: Method::Script,
                version,
                source: tool.repo.clone(),
                asset: None,
                checksum: None,
                notes,
            })
        }
    }
}

fn resolve_crate(remote: &Remote, crate_name: &str, pin: &Pin) -> Result<String> {
    let candidates = remote.crate_versions(crate_name)?;
    // Tag and prefix pins name GitHub tags; map them onto the crate version they contain.
    let crate_pin = match pin {
        Pin::Tag(tag) | Pin::Prefix(tag) => match crate::version::Version::parse(tag) {
            Some(version) if version.pre.is_none() => Pin::Version(version.parts),
            _ => bail!("pin `{pin}` doesn't name a crates.io version"),
        },
        other => other.clone(),
    };
    select(&crate_pin, &candidates, |candidate| candidate, |_| true)
        .map(|candidate| candidate.tag.clone())
        .with_context(|| format!("no crates.io version of {crate_name} matches pin `{pin}`"))
}

/// Shared inputs for installing tools.
pub struct InstallContext<'a> {
    pub paths: &'a Paths,
    pub remote: &'a Remote,
    pub platform: &'a Platform,
    /// Main content repo, the working directory for script and post-install commands.
    pub repo_root: &'a Path,
    /// Receives progress lines.
    pub log: &'a (dyn Fn(String) + Sync),
}

/// Installs one tool and returns its new record. `existing` is the current
/// record, used to keep the previous version and clean up stale links.
pub fn install(
    context: &InstallContext,
    name: &str,
    tool: &ToolDef,
    resolved: &Resolved,
    existing: Option<&ToolRecord>,
) -> Result<ToolRecord> {
    let tool_dir = context.paths.tools_dir.join(name);
    std::fs::create_dir_all(&tool_dir)
        .with_context(|| format!("creating {}", tool_dir.display()))?;
    let mut sha256 = None;
    let mut verified = false;

    let current = match resolved.method {
        Method::Prebuilt => {
            let (installed, hash, checked) =
                install_prebuilt(context, name, tool, resolved, &tool_dir)?;
            sha256 = Some(hash);
            verified = checked;
            installed
        }
        Method::Cargo => install_cargo(context, name, tool, resolved, &tool_dir)?,
        Method::Npm => install_npm(context, name, tool, resolved, &tool_dir)?,
        Method::Uv => install_uv(context, name, tool, resolved, &tool_dir)?,
        Method::Script => install_script(context, name, tool, resolved, &tool_dir)?,
    };

    // Drop links the old version had that the new one doesn't.
    if let Some(existing) = existing {
        for stale in existing
            .current
            .links
            .iter()
            .filter(|link| !current.links.contains(link))
        {
            remove_owned_link(stale, &tool_dir);
        }
    }

    let previous = match existing {
        Some(existing) if existing.current.version != current.version => {
            // Keep exactly one previous version; delete the one before it.
            if let Some(older) = &existing.previous
                && let Some(directory) = &older.install_dir
                && Some(directory) != existing.current.install_dir.as_ref()
                && Some(directory) != current.install_dir.as_ref()
            {
                std::fs::remove_dir_all(directory).ok();
            }
            Some(existing.current.clone())
        }
        Some(existing) => existing.previous.clone(),
        None => None,
    };

    if let Some(command) = &tool.post_install {
        (context.log)(format!("{name}: running post_install"));
        run_shell(context, name, command, resolved, &tool_dir)
            .with_context(|| format!("{name}: post_install failed"))?;
    }

    Ok(ToolRecord {
        current,
        previous,
        method: resolved.method,
        source: resolved.source.clone(),
        fingerprint: tool.fingerprint(),
        sha256,
        verified,
        installed_at: crate::util::unix_now(),
    })
}

/// Directory name for a version, safe as a single path component.
fn version_dir_name(version: &str) -> String {
    let cleaned: String = version
        .chars()
        .map(|character| {
            if character == '/' || character == '\\' {
                '_'
            } else {
                character
            }
        })
        .collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "unknown".to_string()
    } else {
        cleaned
    }
}

fn install_prebuilt(
    context: &InstallContext,
    name: &str,
    tool: &ToolDef,
    resolved: &Resolved,
    tool_dir: &Path,
) -> Result<(InstalledVersion, String, bool)> {
    let asset = resolved
        .asset
        .as_ref()
        .with_context(|| format!("{name}: no asset resolved"))?;
    let version = resolved.version.clone().unwrap_or_else(|| "unknown".into());
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(tool_dir)?;
    let download = staging.path().join("download");
    (context.log)(format!("{name}: downloading {}", asset.name));
    context.remote.download(&asset.url, &download)?;
    let hash = crate::util::sha256_file(&download)?;
    let verified = verify_checksum(context, name, resolved, &asset.name, &hash)?;

    let unpacked = staging.path().join("unpacked");
    archive::unpack(&download, &asset.name, &unpacked)
        .with_context(|| format!("{name}: unpacking {}", asset.name))?;
    let root = archive::content_root(&unpacked)?;
    let files = locate_files(name, tool, &root)?;
    let relative_files: Vec<(PathBuf, PathBuf)> = files
        .into_iter()
        .map(|(link, file)| {
            (
                link,
                file.strip_prefix(&root)
                    .map(Path::to_path_buf)
                    .unwrap_or(file),
            )
        })
        .collect();

    let final_dir = tool_dir.join(version_dir_name(&version));
    commit_directory(&root, &final_dir)?;
    let links = create_links(context.paths, tool_dir, &final_dir, &relative_files)?;
    Ok((
        InstalledVersion {
            version,
            install_dir: Some(final_dir),
            links,
        },
        hash,
        verified,
    ))
}

/// Checks the download against the release's published checksum. Returns
/// whether a checksum was found and matched; a mismatch is an error.
fn verify_checksum(
    context: &InstallContext,
    name: &str,
    resolved: &Resolved,
    asset_name: &str,
    actual: &str,
) -> Result<bool> {
    let Some(source) = &resolved.checksum else {
        return Ok(false);
    };
    let (checksum_asset, sidecar) = match source {
        ChecksumSource::Sidecar(asset) => (asset, true),
        ChecksumSource::List(asset) => (asset, false),
    };
    let text = context.remote.download_text(&checksum_asset.url)?;
    match parse_checksum(&text, asset_name, sidecar) {
        Some(expected) if expected == actual => {
            (context.log)(format!(
                "{name}: checksum verified against {}",
                checksum_asset.name
            ));
            Ok(true)
        }
        Some(expected) => bail!(
            "{name}: checksum mismatch for {asset_name}: {} says {expected}, download is {actual}",
            checksum_asset.name
        ),
        None => {
            (context.log)(format!(
                "{name}: {} has no entry for {asset_name}; not verified",
                checksum_asset.name
            ));
            Ok(false)
        }
    }
}

/// Maps each `bins` and `links` entry to a file under `root`, as
/// (link path relative to the prefix, file path).
fn locate_files(name: &str, tool: &ToolDef, root: &Path) -> Result<Vec<(PathBuf, PathBuf)>> {
    let mut files = Vec::new();
    let all_files = archive::list_files(root);
    for (link_name, path) in bin_specs(name, tool) {
        let file = match &path {
            Some(path) => {
                let candidate = root.join(path);
                if !candidate.is_file() {
                    bail!("{name}: `{path}` isn't in the release (bins entry `{link_name}`)");
                }
                candidate
            }
            None => match archive::find_file(root, &link_name) {
                Some(found) => found,
                // A bare binary asset (`jq-linux-amd64`) is the only file present.
                None if all_files.len() == 1 => all_files[0].clone(),
                None => bail!(
                    "{name}: no file named `{link_name}` in the release; set `bins = [\"{link_name}=path/in/archive\"]`"
                ),
            },
        };
        make_executable(&file)?;
        files.push((PathBuf::from("bin").join(&link_name), file));
    }
    for (destination, path) in &tool.links {
        let candidate = root.join(path);
        let file = if candidate.exists() {
            candidate
        } else if all_files.len() == 1 {
            all_files[0].clone()
        } else {
            bail!("{name}: `{path}` isn't in the release (links entry `{destination}`)");
        };
        files.push((PathBuf::from(destination), file));
    }
    Ok(files)
}

/// `bins` entries, defaulting to one binary named after the tool unless the
/// tool only links other files (a Zellij plugin).
fn bin_specs(name: &str, tool: &ToolDef) -> Vec<(String, Option<String>)> {
    if tool.bins.is_empty() && !tool.links.is_empty() {
        return Vec::new();
    }
    tool.bin_specs(name)
}

fn make_executable(file: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(file)?.permissions();
    permissions.set_mode(permissions.mode() | 0o755);
    std::fs::set_permissions(file, permissions)?;
    Ok(())
}

/// Moves a staged directory to its final place, replacing an existing
/// install of the same version.
fn commit_directory(staged: &Path, final_dir: &Path) -> Result<()> {
    if final_dir.exists() {
        let parked = final_dir.with_extension(format!("old-{}", std::process::id()));
        std::fs::rename(final_dir, &parked)?;
        std::fs::rename(staged, final_dir)
            .with_context(|| format!("moving the install into {}", final_dir.display()))?;
        std::fs::remove_dir_all(&parked).ok();
    } else {
        std::fs::rename(staged, final_dir)
            .with_context(|| format!("moving the install into {}", final_dir.display()))?;
    }
    Ok(())
}

/// Creates `<prefix>/<link>` -> `<final_dir>/<file>` for each entry.
fn create_links(
    paths: &Paths,
    tool_dir: &Path,
    final_dir: &Path,
    files: &[(PathBuf, PathBuf)],
) -> Result<Vec<PathBuf>> {
    let mut links = Vec::new();
    for (link, file) in files {
        let link_path = paths.prefix.join(link);
        let target = final_dir.join(file);
        place_link(&link_path, &target, tool_dir)?;
        links.push(link_path);
    }
    Ok(links)
}

/// Points `link_path` at `target`, replacing only a link dfapp owns (one
/// pointing into `tool_dir`). Anything else in the way is an error, so a
/// system binary or a user's own file is never overwritten.
pub fn place_link(link_path: &Path, target: &Path, tool_dir: &Path) -> Result<()> {
    if let Some(parent) = link_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::read_link(link_path) {
        Ok(existing) if existing == target => return Ok(()),
        Ok(existing) if existing.starts_with(tool_dir) => {
            std::fs::remove_file(link_path)?;
        }
        Ok(existing) => bail!(
            "{} already exists and points at {}, which dfapp doesn't manage; remove it to let dfapp install here",
            link_path.display(),
            existing.display()
        ),
        Err(_) if link_path.exists() => bail!(
            "{} already exists and isn't managed by dfapp; remove it to let dfapp install here",
            link_path.display()
        ),
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, link_path)
        .with_context(|| format!("linking {} -> {}", link_path.display(), target.display()))
}

/// Removes `link` if it's a symlink into `tool_dir`.
pub fn remove_owned_link(link: &Path, tool_dir: &Path) {
    if let Ok(target) = std::fs::read_link(link)
        && target.starts_with(tool_dir)
    {
        std::fs::remove_file(link).ok();
    }
}

fn install_cargo(
    context: &InstallContext,
    name: &str,
    tool: &ToolDef,
    resolved: &Resolved,
    tool_dir: &Path,
) -> Result<InstalledVersion> {
    let cargo = crate::util::find_executable("cargo", &[context.paths.home.join(".cargo/bin")])
        .context("building from source needs cargo; select the `rustup` tool or install a Rust toolchain")?;
    let crate_name = resolved.source.clone().unwrap_or_else(|| name.to_string());
    let version = resolved.version.clone().unwrap_or_else(|| "latest".into());
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(tool_dir)?;
    (context.log)(format!(
        "{name}: building {crate_name} {version} with cargo (this can take a while)"
    ));
    let mut command = Command::new(cargo);
    command
        .args(["install", "--locked", "--root"])
        .arg(staging.path())
        .arg(&crate_name);
    if let Some(version) = &resolved.version {
        command.args(["--version", version]);
    }
    command.env("PATH", path_with_bin(context.paths));
    crate::util::run_checked(&mut command)
        .with_context(|| format!("{name}: cargo install failed"))?;
    let root = staging.path().to_path_buf();
    let mut files = Vec::new();
    for (link_name, _) in bin_specs(name, tool) {
        let file = PathBuf::from("bin").join(&link_name);
        if !root.join(&file).is_file() {
            bail!("{name}: cargo didn't produce a binary named `{link_name}`");
        }
        files.push((PathBuf::from("bin").join(&link_name), file));
    }
    let final_dir = tool_dir.join(version_dir_name(&format!("cargo-{version}")));
    let staged = staging.keep();
    commit_directory(&staged, &final_dir)?;
    let links = create_links(context.paths, tool_dir, &final_dir, &files)?;
    Ok(InstalledVersion {
        version,
        install_dir: Some(final_dir),
        links,
    })
}

fn install_npm(
    context: &InstallContext,
    name: &str,
    tool: &ToolDef,
    resolved: &Resolved,
    tool_dir: &Path,
) -> Result<InstalledVersion> {
    let npm = crate::util::find_executable("npm", std::slice::from_ref(&context.paths.bin_dir))
        .context("npm tools need npm; add the `node` tool to `requires`")?;
    let package = resolved.source.clone().unwrap_or_else(|| name.to_string());
    let version = resolved.version.clone().unwrap_or_else(|| "latest".into());
    let staging = tempfile::Builder::new()
        .prefix(".staging-")
        .tempdir_in(tool_dir)?;
    (context.log)(format!("{name}: npm install {package}@{version}"));
    let mut command = Command::new(npm);
    command
        .args(["install", "--global", "--no-fund", "--no-audit", "--prefix"])
        .arg(staging.path())
        .arg(format!("{package}@{version}"))
        .env("PATH", path_with_bin(context.paths));
    crate::util::run_checked(&mut command)
        .with_context(|| format!("{name}: npm install failed"))?;
    let staged = staging.keep();
    let mut files = Vec::new();
    for (link_name, _) in bin_specs(name, tool) {
        let file = PathBuf::from("bin").join(&link_name);
        if !staged.join(&file).exists() {
            std::fs::remove_dir_all(&staged).ok();
            bail!("{name}: the npm package has no `{link_name}` binary; set `bins`");
        }
        files.push((file.clone(), file));
    }
    let final_dir = tool_dir.join(version_dir_name(&version));
    commit_directory(&staged, &final_dir)?;
    let links = create_links(context.paths, tool_dir, &final_dir, &files)?;
    Ok(InstalledVersion {
        version,
        install_dir: Some(final_dir),
        links,
    })
}

fn install_uv(
    context: &InstallContext,
    name: &str,
    tool: &ToolDef,
    resolved: &Resolved,
    tool_dir: &Path,
) -> Result<InstalledVersion> {
    let uv = crate::util::find_executable("uv", std::slice::from_ref(&context.paths.bin_dir))
        .context("uv tools need uv; add the `uv` tool to `requires`")?;
    let package = resolved.source.clone().unwrap_or_else(|| name.to_string());
    let version = resolved.version.clone().unwrap_or_else(|| "latest".into());
    // uv writes absolute paths into the environments it creates, so it installs
    // straight into the final directory instead of a staging one.
    let final_dir = tool_dir.join(version_dir_name(&version));
    if final_dir.exists() {
        std::fs::remove_dir_all(&final_dir)?;
    }
    (context.log)(format!("{name}: uv tool install {package}=={version}"));
    let mut command = Command::new(uv);
    command
        .args(["tool", "install", "--force"])
        .arg(if resolved.version.is_some() {
            format!("{package}=={version}")
        } else {
            package.clone()
        })
        .env("UV_TOOL_DIR", final_dir.join("tools"))
        .env("UV_TOOL_BIN_DIR", final_dir.join("bin"))
        .env("PATH", path_with_bin(context.paths));
    if let Err(error) = crate::util::run_checked(&mut command) {
        std::fs::remove_dir_all(&final_dir).ok();
        return Err(error.context(format!("{name}: uv tool install failed")));
    }
    let mut files = Vec::new();
    for (link_name, _) in bin_specs(name, tool) {
        let file = PathBuf::from("bin").join(&link_name);
        if !final_dir.join(&file).exists() {
            bail!("{name}: uv didn't create a `{link_name}` binary; set `bins`");
        }
        files.push((file.clone(), file));
    }
    let links = create_links(context.paths, tool_dir, &final_dir, &files)?;
    Ok(InstalledVersion {
        version,
        install_dir: Some(final_dir),
        links,
    })
}

fn install_script(
    context: &InstallContext,
    name: &str,
    tool: &ToolDef,
    resolved: &Resolved,
    tool_dir: &Path,
) -> Result<InstalledVersion> {
    let command = tool
        .install
        .as_deref()
        .with_context(|| format!("{name}: script tools need `install`"))?;
    (context.log)(format!("{name}: running install script"));
    run_shell(context, name, command, resolved, tool_dir)
        .with_context(|| format!("{name}: install script failed"))?;
    let version = match &tool.version_command {
        Some(version_command) => {
            let output = run_shell(context, name, version_command, resolved, tool_dir)
                .with_context(|| format!("{name}: version command failed"))?;
            version_from_output(&output).unwrap_or_else(|| output.trim().to_string())
        }
        None => resolved
            .version
            .clone()
            .unwrap_or_else(|| "installed".into()),
    };
    // Script tools may name files they placed under DFAPP_TOOL_DIR to link.
    let mut links = Vec::new();
    for (link_name, path) in tool.bin_specs(name) {
        if let Some(path) = path {
            let target = tool_dir.join(path);
            let link_path = context.paths.bin_dir.join(&link_name);
            place_link(&link_path, &target, tool_dir)?;
            links.push(link_path);
        }
    }
    Ok(InstalledVersion {
        version,
        install_dir: None,
        links,
    })
}

/// `PATH` with the bin directory first, so tools installed earlier in the run
/// (node for npm, uv) are found.
pub fn path_with_bin(paths: &Paths) -> std::ffi::OsString {
    let mut entries = vec![paths.bin_dir.clone()];
    if let Some(existing) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(entries).unwrap_or_default()
}

/// A `sh -c <command>` with the bin directory on `PATH` and `HOME` set to the
/// home dfapp deploys into. They differ from the caller's only under
/// `DFAPP_HOME` or `--home`, where a script writing under `$HOME` must land in
/// that home, not the real one.
pub fn shell(paths: &Paths, command: &str) -> Command {
    let mut process = Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .env("PATH", path_with_bin(paths))
        .env("HOME", &paths.home);
    process
}

/// Runs a script-tool or post-install command with `sh -c` in the repo root,
/// passing the install context as `DFAPP_*` variables. Returns stdout.
fn run_shell(
    context: &InstallContext,
    name: &str,
    command: &str,
    resolved: &Resolved,
    tool_dir: &Path,
) -> Result<String> {
    let mut process = shell(context.paths, command);
    process
        .current_dir(context.repo_root)
        .env("DFAPP_TOOL", name)
        .env(
            "DFAPP_VERSION",
            resolved.version.clone().unwrap_or_default(),
        )
        .env("DFAPP_PREFIX", &context.paths.prefix)
        .env("DFAPP_BIN_DIR", &context.paths.bin_dir)
        .env("DFAPP_TOOL_DIR", tool_dir)
        .env("DFAPP_REPO", context.repo_root)
        .env("DFAPP_SCOPE", context.paths.scope.to_string())
        .env("DFAPP_OS", &context.platform.os)
        .env("DFAPP_ARCH", &context.platform.arch)
        .env("DFAPP_WSL", if context.platform.wsl { "1" } else { "0" });
    crate::util::run_stdout(&mut process)
}

/// Removes an installed tool: its links, its install directories, and for
/// script tools, runs the `uninstall` command when there is one.
pub fn uninstall(
    paths: &Paths,
    name: &str,
    tool: Option<&ToolDef>,
    record: &ToolRecord,
    repo_root: &Path,
) -> Result<()> {
    let tool_dir = paths.tools_dir.join(name);
    for link in &record.current.links {
        remove_owned_link(link, &tool_dir);
    }
    if record.method == Method::Script
        && let Some(command) = tool.and_then(|tool| tool.uninstall.as_deref())
    {
        let mut process = shell(paths, command);
        process
            .current_dir(repo_root)
            .env("DFAPP_TOOL", name)
            .env("DFAPP_PREFIX", &paths.prefix)
            .env("DFAPP_BIN_DIR", &paths.bin_dir)
            .env("DFAPP_TOOL_DIR", &tool_dir);
        crate::util::run_checked(&mut process)
            .with_context(|| format!("{name}: uninstall command failed"))?;
    }
    if tool_dir.exists() {
        std::fs::remove_dir_all(&tool_dir)
            .with_context(|| format!("removing {}", tool_dir.display()))?;
    }
    Ok(())
}

/// Swaps a tool back to its previous version: re-points its links at the
/// previous install directory and swaps the records.
pub fn rollback(paths: &Paths, name: &str, record: &ToolRecord) -> Result<ToolRecord> {
    let previous = record
        .previous
        .clone()
        .with_context(|| format!("{name} has no previous version to roll back to"))?;
    let tool_dir = paths.tools_dir.join(name);
    let (Some(previous_dir), Some(current_dir)) =
        (&previous.install_dir, &record.current.install_dir)
    else {
        bail!("{name} wasn't installed into a dfapp-managed directory, so it can't be rolled back");
    };
    if !previous_dir.exists() {
        bail!(
            "{name}'s previous version directory {} is gone",
            previous_dir.display()
        );
    }
    let mut links = Vec::new();
    for link in &record.current.links {
        let Ok(target) = std::fs::read_link(link) else {
            continue;
        };
        let Ok(relative) = target.strip_prefix(current_dir) else {
            continue;
        };
        let previous_target = previous_dir.join(relative);
        if previous_target.exists() {
            place_link(link, &previous_target, &tool_dir)?;
            links.push(link.clone());
        } else {
            remove_owned_link(link, &tool_dir);
        }
    }
    Ok(ToolRecord {
        current: InstalledVersion { links, ..previous },
        previous: Some(record.current.clone()),
        installed_at: crate::util::unix_now(),
        ..record.clone()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Scope;

    fn tool(toml_text: &str) -> ToolDef {
        toml::from_str(toml_text).unwrap()
    }

    fn context_parts(home: &Path) -> (Paths, Remote) {
        let paths = Paths::for_home(home, Scope::User);
        let remote = Remote::new(&paths.cache_dir, std::time::Duration::from_secs(3600), true);
        (paths, remote)
    }

    #[test]
    fn shell_commands_see_the_deploy_home() {
        let home = tempfile::tempdir().unwrap();
        let (paths, _) = context_parts(home.path());
        let output = shell(&paths, "printf %s \"$HOME\"").output().unwrap();
        assert_eq!(output.stdout, home.path().as_os_str().as_encoded_bytes());
    }

    #[test]
    fn locates_bins_by_name_by_path_and_for_bare_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/node"), "").unwrap();
        std::fs::write(root.join("README"), "").unwrap();
        let by_path = tool("repo = \"a/b\"\nbins = [\"node=bin/node\"]");
        let found = locate_files("node", &by_path, root).unwrap();
        assert_eq!(found[0].0, PathBuf::from("bin/node"));
        let by_name = tool("repo = \"a/b\"\nbins = [\"node\"]");
        assert!(
            locate_files("node", &by_name, root).unwrap()[0]
                .1
                .ends_with("bin/node")
        );
        let missing = tool("repo = \"a/b\"\nbins = [\"npm\"]");
        assert!(locate_files("node", &missing, root).is_err());

        let bare = tempfile::tempdir().unwrap();
        std::fs::write(bare.path().join("jq-linux-amd64"), "").unwrap();
        let jq = tool("repo = \"jqlang/jq\"\nbins = [\"jq\"]");
        let found = locate_files("jq", &jq, bare.path()).unwrap();
        assert!(found[0].1.ends_with("jq-linux-amd64"));
        assert!(crate::util::is_executable(&found[0].1));
    }

    #[test]
    fn plugin_tools_link_files_without_bins() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zjstatus.wasm"), "").unwrap();
        let plugin = tool(
            "repo = \"dj95/zjstatus\"\nlinks = { \"share/zellij/plugins/zjstatus.wasm\" = \"zjstatus.wasm\" }",
        );
        let found = locate_files("zjstatus", &plugin, dir.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].0,
            PathBuf::from("share/zellij/plugins/zjstatus.wasm")
        );
    }

    #[test]
    fn links_never_replace_unmanaged_files() {
        let dir = tempfile::tempdir().unwrap();
        let tool_dir = dir.path().join("tools/rg");
        let target = tool_dir.join("1/rg");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "").unwrap();
        let link = dir.path().join("bin/rg");
        place_link(&link, &target, &tool_dir).unwrap();
        // Re-pointing an owned link works.
        let newer = tool_dir.join("2/rg");
        std::fs::create_dir_all(newer.parent().unwrap()).unwrap();
        std::fs::write(&newer, "").unwrap();
        place_link(&link, &newer, &tool_dir).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), newer);
        // A real file in the way is left alone.
        let foreign = dir.path().join("bin/fd");
        std::fs::write(&foreign, "user's").unwrap();
        assert!(place_link(&foreign, &newer, &tool_dir).is_err());
        assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "user's");
    }

    #[test]
    fn prebuilt_install_verifies_checksums_keeps_previous_and_rolls_back() {
        let home = tempfile::tempdir().unwrap();
        let (paths, remote) = context_parts(home.path());
        let fixtures = tempfile::tempdir().unwrap();
        let log = |_: String| {};
        let repo_root = home.path().to_path_buf();
        let platform = Platform::detect();
        let context = InstallContext {
            paths: &paths,
            remote: &remote,
            platform: &platform,
            repo_root: &repo_root,
            log: &log,
        };
        let definition =
            tool("repo = \"o/tool\"\nbins = [\"tool\"]\nassets.any = \"tool-*.tar.gz\"");

        let install_version =
            |version: &str, checksum_text: Option<String>, existing: Option<&ToolRecord>| {
                let archive_path = archive::tests::make_tar_gz(
                    fixtures.path(),
                    &format!("tool-{version}.tar.gz"),
                    &[(&format!("tool-{version}/tool"), version, 0o755)],
                );
                let hash = crate::util::sha256_file(&archive_path).unwrap();
                let checksum = checksum_text.map(|text| {
                    let text = text.replace("HASH", &hash);
                    let path = fixtures
                        .path()
                        .join(format!("tool-{version}.tar.gz.sha256"));
                    std::fs::write(&path, text).unwrap();
                    ChecksumSource::Sidecar(Asset {
                        name: format!("tool-{version}.tar.gz.sha256"),
                        url: format!("file://{}", path.display()),
                    })
                });
                let resolved = Resolved {
                    method: Method::Prebuilt,
                    version: Some(format!("v{version}")),
                    source: Some("o/tool".into()),
                    asset: Some(Asset {
                        name: format!("tool-{version}.tar.gz"),
                        url: format!("file://{}", archive_path.display()),
                    }),
                    checksum,
                    notes: vec![],
                };
                install(&context, "tool", &definition, &resolved, existing)
            };

        let first =
            install_version("1.0.0", Some("HASH  tool-1.0.0.tar.gz\n".into()), None).unwrap();
        assert!(first.verified);
        let link = paths.bin_dir.join("tool");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "1.0.0");

        let bad = install_version("1.1.0", Some(format!("{}\n", "0".repeat(64))), Some(&first));
        assert!(bad.unwrap_err().to_string().contains("checksum mismatch"));
        assert_eq!(
            std::fs::read_to_string(&link).unwrap(),
            "1.0.0",
            "a failed install leaves the old version"
        );

        let second = install_version("2.0.0", None, Some(&first)).unwrap();
        assert!(!second.verified);
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "2.0.0");
        assert_eq!(second.previous.as_ref().unwrap().version, "v1.0.0");

        let rolled_back = rollback(&paths, "tool", &second).unwrap();
        assert_eq!(rolled_back.current.version, "v1.0.0");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "1.0.0");

        uninstall(&paths, "tool", Some(&definition), &rolled_back, &repo_root).unwrap();
        assert!(!link.exists() && std::fs::symlink_metadata(&link).is_err());
        assert!(!paths.tools_dir.join("tool").exists());
    }

    #[test]
    fn version_directory_names_are_single_components() {
        assert_eq!(version_dir_name("v1.2.3"), "v1.2.3");
        assert_eq!(version_dir_name("release/1.0"), "release_1.0");
        assert_eq!(version_dir_name(".."), "unknown");
    }
}

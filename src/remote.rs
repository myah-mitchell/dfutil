//! Version and download lookups: GitHub releases, npm, PyPI, and crates.io,
//! with an on-disk response cache, plus release asset and checksum matching.

use crate::version::Candidate;
use anyhow::{Context, Result, bail};
use globset::{Glob, GlobMatcher};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A GitHub release with its assets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Release {
    #[serde(flatten)]
    pub candidate: Candidate,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Asset {
    pub name: String,
    pub url: String,
}

/// Fields read from the GitHub releases API.
#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    published_at: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
}

/// HTTP client with caching. Cheap to clone and safe to share across threads.
#[derive(Clone)]
pub struct Remote {
    agent: ureq::Agent,
    github_token: Option<String>,
    cache_dir: PathBuf,
    ttl: Duration,
    /// Use cached responses regardless of age and never hit the network.
    pub offline: bool,
}

impl Remote {
    pub fn new(cache_dir: &Path, ttl: Duration, offline: bool) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent(concat!("dfutil/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self {
            agent,
            github_token: github_token(),
            cache_dir: cache_dir.to_path_buf(),
            ttl,
            offline,
        }
    }

    /// Releases of `owner/name`, newest first, drafts excluded.
    pub fn github_releases(&self, repo: &str) -> Result<Vec<Release>> {
        let url = format!("https://api.github.com/repos/{repo}/releases?per_page=100");
        let body = self.get_cached(
            &url,
            &format!("github/{}.json", repo.replace('/', "__")),
            true,
        )?;
        let releases: Vec<ApiRelease> = serde_json::from_str(&body)
            .with_context(|| format!("parsing the GitHub releases of {repo}"))?;
        Ok(releases
            .into_iter()
            .filter(|release| !release.draft)
            .map(|release| Release {
                candidate: Candidate {
                    tag: release.tag_name,
                    prerelease: release.prerelease,
                    published_at: release.published_at,
                },
                assets: release
                    .assets
                    .into_iter()
                    .map(|asset| Asset {
                        name: asset.name,
                        url: asset.browser_download_url,
                    })
                    .collect(),
            })
            .collect())
    }

    /// Published versions of an npm package.
    pub fn npm_versions(&self, package: &str) -> Result<Vec<Candidate>> {
        #[derive(Deserialize)]
        struct Packument {
            #[serde(default)]
            time: std::collections::BTreeMap<String, String>,
            #[serde(default)]
            versions: std::collections::BTreeMap<String, serde_json::Value>,
        }
        let url = format!("https://registry.npmjs.org/{}", package.replace('/', "%2F"));
        let body = self.get_cached(
            &url,
            &format!("npm/{}.json", package.replace('/', "__")),
            false,
        )?;
        let packument: Packument = serde_json::from_str(&body)
            .with_context(|| format!("parsing npm metadata for {package}"))?;
        Ok(packument
            .versions
            .keys()
            .map(|version| Candidate {
                tag: version.clone(),
                prerelease: version.contains('-'),
                published_at: packument.time.get(version).cloned(),
            })
            .collect())
    }

    /// Published versions of a PyPI package, yanked releases excluded.
    pub fn pypi_versions(&self, package: &str) -> Result<Vec<Candidate>> {
        #[derive(Deserialize)]
        struct Project {
            releases: std::collections::BTreeMap<String, Vec<File>>,
        }
        #[derive(Deserialize)]
        struct File {
            #[serde(default)]
            yanked: bool,
            upload_time_iso_8601: Option<String>,
        }
        let url = format!("https://pypi.org/pypi/{package}/json");
        let body = self.get_cached(&url, &format!("pypi/{package}.json"), false)?;
        let project: Project = serde_json::from_str(&body)
            .with_context(|| format!("parsing PyPI metadata for {package}"))?;
        Ok(project
            .releases
            .into_iter()
            .filter(|(_, files)| !files.is_empty() && files.iter().any(|file| !file.yanked))
            .map(|(version, files)| Candidate {
                prerelease: version.chars().any(|c| c.is_ascii_alphabetic()),
                published_at: files
                    .iter()
                    .find_map(|file| file.upload_time_iso_8601.clone()),
                tag: version,
            })
            .collect())
    }

    /// Published versions of a crate, yanked versions excluded.
    pub fn crate_versions(&self, crate_name: &str) -> Result<Vec<Candidate>> {
        #[derive(Deserialize)]
        struct Response {
            versions: Vec<CrateVersion>,
        }
        #[derive(Deserialize)]
        struct CrateVersion {
            num: String,
            #[serde(default)]
            yanked: bool,
            created_at: Option<String>,
        }
        let url = format!("https://crates.io/api/v1/crates/{crate_name}/versions");
        let body = self.get_cached(&url, &format!("crates/{crate_name}.json"), false)?;
        let response: Response = serde_json::from_str(&body)
            .with_context(|| format!("parsing crates.io data for {crate_name}"))?;
        Ok(response
            .versions
            .into_iter()
            .filter(|version| !version.yanked)
            .map(|version| Candidate {
                prerelease: version.num.contains('-'),
                tag: version.num,
                published_at: version.created_at,
            })
            .collect())
    }

    /// Downloads `url` to `destination`, streaming to disk. `file://` URLs
    /// are copied, which serves local mirrors and tests.
    pub fn download(&self, url: &str, destination: &Path) -> Result<()> {
        if let Some(path) = url.strip_prefix("file://") {
            std::fs::copy(path, destination).with_context(|| format!("copying {path}"))?;
            return Ok(());
        }
        if self.offline {
            bail!("offline mode: can't download {url}");
        }
        let response = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("downloading {url}"))?;
        let mut reader = response.into_body().into_reader();
        let mut file = std::fs::File::create(destination)
            .with_context(|| format!("creating {}", destination.display()))?;
        std::io::copy(&mut reader, &mut file).with_context(|| format!("downloading {url}"))?;
        file.flush()?;
        Ok(())
    }

    /// Downloads a small text file (a checksum list) into memory.
    pub fn download_text(&self, url: &str) -> Result<String> {
        if let Some(path) = url.strip_prefix("file://") {
            return std::fs::read_to_string(path).with_context(|| format!("reading {path}"));
        }
        if self.offline {
            bail!("offline mode: can't download {url}");
        }
        let mut response = self
            .agent
            .get(url)
            .call()
            .with_context(|| format!("downloading {url}"))?;
        Ok(response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_to_string()?)
    }

    /// GETs a URL, using a cached copy younger than the TTL. On a network
    /// failure a stale cached copy is used rather than failing.
    fn get_cached(&self, url: &str, cache_key: &str, github: bool) -> Result<String> {
        let cache_file = self.cache_dir.join(cache_key);
        let fresh = std::fs::metadata(&cache_file)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age < self.ttl);
        if (fresh || self.offline) && cache_file.exists() {
            return std::fs::read_to_string(&cache_file)
                .with_context(|| format!("reading {}", cache_file.display()));
        }
        if self.offline {
            bail!("offline mode and no cached response for {url}");
        }
        let mut request = self.agent.get(url);
        if github {
            request = request
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(token) = &self.github_token {
                request = request.header("Authorization", &format!("Bearer {token}"));
            }
        }
        match request.call() {
            Ok(mut response) => {
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(32 * 1024 * 1024)
                    .read_to_string()?;
                crate::util::write_atomic(&cache_file, body.as_bytes())?;
                Ok(body)
            }
            Err(error) => {
                if let Ok(stale) = std::fs::read_to_string(&cache_file) {
                    return Ok(stale);
                }
                let hint = match &error {
                    ureq::Error::StatusCode(403 | 429) if github && self.github_token.is_none() => {
                        " (GitHub rate limit; set GITHUB_TOKEN or log in with `gh auth login`)"
                    }
                    ureq::Error::StatusCode(404) => " (not found)",
                    _ => "",
                };
                Err(anyhow::Error::new(error).context(format!("fetching {url}{hint}")))
            }
        }
    }
}

/// A GitHub token from `GITHUB_TOKEN`, `GH_TOKEN`, or `gh auth token`.
fn github_token() -> Option<String> {
    for variable in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(variable)
            && !token.trim().is_empty()
        {
            return Some(token.trim().to_string());
        }
    }
    let output = std::process::Command::new("gh")
        .args(["auth", "token"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (output.status.success() && !token.is_empty()).then_some(token)
}

/// Assets that are signatures, checksums, or metadata, never the thing to install.
fn is_auxiliary(name: &str) -> bool {
    let lower = name.to_lowercase();
    const SUFFIXES: [&str; 12] = [
        ".sha256",
        ".sha256sum",
        ".sha512",
        ".sha512sum",
        ".sig",
        ".asc",
        ".pem",
        ".sbom",
        ".intoto.jsonl",
        ".minisig",
        ".md5",
        ".spdx.json",
    ];
    SUFFIXES.iter().any(|suffix| lower.ends_with(suffix))
        || lower.contains("checksums")
        || lower.starts_with("sha256sums")
}

pub fn compile_glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(Glob::new(pattern)
        .with_context(|| format!("invalid asset pattern `{pattern}`"))?
        .compile_matcher())
}

/// The first non-auxiliary asset whose name matches the glob.
pub fn find_asset<'a>(release: &'a Release, matcher: &GlobMatcher) -> Option<&'a Asset> {
    release
        .assets
        .iter()
        .find(|asset| !is_auxiliary(&asset.name) && matcher.is_match(&asset.name))
}

/// Where a release publishes the SHA-256 of `asset_name`, if anywhere.
#[derive(Clone, Debug, PartialEq)]
pub enum ChecksumSource {
    /// A sidecar file holding just this asset's hash (`<asset>.sha256`).
    Sidecar(Asset),
    /// A list of hashes for several assets (`checksums.txt`, `SHA256SUMS`).
    List(Asset),
}

pub fn find_checksum_source(release: &Release, asset_name: &str) -> Option<ChecksumSource> {
    for suffix in [".sha256", ".sha256sum", ".sha256.txt"] {
        let wanted = format!("{asset_name}{suffix}");
        if let Some(asset) = release.assets.iter().find(|asset| asset.name == wanted) {
            return Some(ChecksumSource::Sidecar(asset.clone()));
        }
    }
    release
        .assets
        .iter()
        .find(|asset| {
            let lower = asset.name.to_lowercase();
            (lower.contains("checksums")
                || lower.starts_with("sha256sums")
                || lower.ends_with("sha256sums.txt"))
                && !lower.contains("sha512")
                && !lower.ends_with(".sig")
                && !lower.ends_with(".asc")
                && !lower.ends_with(".pem")
        })
        .map(|asset| ChecksumSource::List(asset.clone()))
}

/// Pulls the expected SHA-256 for `asset_name` out of a checksum file.
/// Understands `<hash>  <name>`, `<hash> *<name>`, BSD `SHA256 (<name>) = <hash>`,
/// and a bare hash (sidecar files).
pub fn parse_checksum(text: &str, asset_name: &str, sidecar: bool) -> Option<String> {
    let is_hash = |word: &str| word.len() == 64 && word.chars().all(|c| c.is_ascii_hexdigit());
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("SHA256 (") {
            if let Some((name, hash)) = rest.split_once(") = ")
                && name == asset_name
                && is_hash(hash.trim())
            {
                return Some(hash.trim().to_lowercase());
            }
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(hash) = words.next() else { continue };
        if !is_hash(hash) {
            continue;
        }
        match words.next() {
            Some(name) => {
                let name = name.trim_start_matches('*').trim_start_matches("./");
                let file_name = name.rsplit('/').next().unwrap_or(name);
                if file_name == asset_name {
                    return Some(hash.to_lowercase());
                }
            }
            None if sidecar => return Some(hash.to_lowercase()),
            None => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(names: &[&str]) -> Release {
        Release {
            candidate: Candidate {
                tag: "v1.0.0".into(),
                prerelease: false,
                published_at: None,
            },
            assets: names
                .iter()
                .map(|name| Asset {
                    name: name.to_string(),
                    url: format!("https://example.com/{name}"),
                })
                .collect(),
        }
    }

    #[test]
    fn asset_matching_skips_checksums_and_signatures() {
        let release = release(&[
            "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz.sha256",
            "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz",
        ]);
        let matcher = compile_glob("ripgrep-*-x86_64-unknown-linux-musl.tar.gz*").unwrap();
        assert_eq!(
            find_asset(&release, &matcher).unwrap().name,
            "ripgrep-14.1.0-x86_64-unknown-linux-musl.tar.gz"
        );
    }

    #[test]
    fn checksum_sources_prefer_sidecars() {
        let assets = release(&["tool.tar.gz", "tool.tar.gz.sha256", "checksums.txt"]);
        assert!(matches!(
            find_checksum_source(&assets, "tool.tar.gz"),
            Some(ChecksumSource::Sidecar(asset)) if asset.name == "tool.tar.gz.sha256"
        ));
        let assets = release(&["tool.tar.gz", "tool_1.0_checksums.txt", "checksums.txt.sig"]);
        assert!(matches!(
            find_checksum_source(&assets, "tool.tar.gz"),
            Some(ChecksumSource::List(asset)) if asset.name == "tool_1.0_checksums.txt"
        ));
        assert_eq!(
            find_checksum_source(&release(&["tool.tar.gz"]), "tool.tar.gz"),
            None
        );
    }

    #[test]
    fn parses_checksum_file_formats() {
        let hash = "a".repeat(64);
        let other = "b".repeat(64);
        let list = format!("{other}  other.tar.gz\n{hash} *tool.tar.gz\n");
        assert_eq!(
            parse_checksum(&list, "tool.tar.gz", false),
            Some(hash.clone())
        );
        let nested = format!("{hash}  ./dist/tool.tar.gz\n");
        assert_eq!(
            parse_checksum(&nested, "tool.tar.gz", false),
            Some(hash.clone())
        );
        let bsd = format!("SHA256 (tool.tar.gz) = {hash}\n");
        assert_eq!(
            parse_checksum(&bsd, "tool.tar.gz", false),
            Some(hash.clone())
        );
        assert_eq!(
            parse_checksum(&format!("{hash}\n"), "tool.tar.gz", true),
            Some(hash.clone())
        );
        assert_eq!(
            parse_checksum(&format!("{hash}\n"), "tool.tar.gz", false),
            None
        );
        assert_eq!(parse_checksum(&list, "missing.tar.gz", false), None);
    }

    #[test]
    fn offline_mode_uses_stale_cache_and_never_downloads() {
        let cache = tempfile::tempdir().unwrap();
        let remote = Remote::new(cache.path(), Duration::from_secs(0), true);
        assert!(remote.github_releases("owner/repo").is_err());
        std::fs::create_dir_all(cache.path().join("github")).unwrap();
        std::fs::write(
            cache.path().join("github/owner__repo.json"),
            r#"[{"tag_name":"v1.0.0","prerelease":false,"draft":false,"published_at":"2026-01-01T00:00:00Z","assets":[{"name":"a.tar.gz","browser_download_url":"https://example.com/a.tar.gz"}]},
                {"tag_name":"v2.0.0","draft":true,"assets":[]}]"#,
        )
        .unwrap();
        let releases = remote.github_releases("owner/repo").unwrap();
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].assets[0].name, "a.tar.gz");
        assert!(
            remote
                .download("https://example.com/a", &cache.path().join("a"))
                .is_err()
        );
    }
}

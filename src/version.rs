//! Version parsing from release tags, version pins, and choosing which
//! release a pin resolves to.
//!
//! Tags in the wild aren't clean semantic versions (`v14.1.0`, `jq-1.7.1`,
//! `fork-ea-v0.46.7-261103`, `nightly`), so parsing is lenient: it finds the
//! first dotted number in the tag and treats a `-suffix` after it as a
//! prerelease marker.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// A version extracted from a tag: numeric parts plus an optional prerelease suffix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Version {
    pub parts: Vec<u64>,
    pub pre: Option<String>,
}

impl Version {
    /// Extracts a version from a tag or version string, or `None` when the
    /// tag has no number in it (such as `nightly`).
    pub fn parse(tag: &str) -> Option<Version> {
        let bytes = tag.as_bytes();
        let start = (0..bytes.len()).find(|&index| {
            bytes[index].is_ascii_digit() && {
                // A version starts at a digit that begins a token: at the start,
                // after a separator, or after a `v` that itself begins a token.
                let previous = index.checked_sub(1).map(|i| bytes[i]);
                match previous {
                    None => true,
                    Some(b'v' | b'V') => index < 2 || !bytes[index - 2].is_ascii_alphanumeric(),
                    Some(byte) => !byte.is_ascii_alphanumeric(),
                }
            }
        })?;
        let rest = &tag[start..];
        let numeric_end = rest
            .find(|character: char| !(character.is_ascii_digit() || character == '.'))
            .unwrap_or(rest.len());
        let numeric = rest[..numeric_end].trim_end_matches('.');
        let parts: Vec<u64> = numeric
            .split('.')
            .map(|part| part.parse().ok())
            .collect::<Option<_>>()?;
        let suffix = &rest[numeric.len()..];
        let pre = suffix
            .split('+')
            .next()
            .unwrap_or("")
            .trim_start_matches(['-', '.', '_']);
        Some(Version {
            parts,
            pre: (!pre.is_empty()).then(|| pre.to_string()),
        })
    }

    /// True when the version's leading numeric parts equal `prefix`, treating
    /// missing parts as zero (`14.1` matches the prefix `[14, 1, 0]`).
    pub fn starts_with(&self, prefix: &[u64]) -> bool {
        prefix
            .iter()
            .enumerate()
            .all(|(index, value)| self.parts.get(index).copied().unwrap_or(0) == *value)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.parts.len().max(other.parts.len());
        for index in 0..length {
            let left = self.parts.get(index).copied().unwrap_or(0);
            let right = other.parts.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => continue,
                unequal => return unequal,
            }
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => natural_cmp(left, right),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Compares strings with embedded numbers numerically (`rc10` > `rc9`).
fn natural_cmp(left: &str, right: &str) -> Ordering {
    let mut left_chars = left.chars().peekable();
    let mut right_chars = right.chars().peekable();
    loop {
        match (left_chars.peek(), right_chars.peek()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => {
                let take_number = |chars: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut digits = String::new();
                    while let Some(&character) = chars.peek() {
                        if !character.is_ascii_digit() {
                            break;
                        }
                        digits.push(character);
                        chars.next();
                    }
                    digits.trim_start_matches('0').to_string()
                };
                let a_digits = take_number(&mut left_chars);
                let b_digits = take_number(&mut right_chars);
                let ordering = a_digits
                    .len()
                    .cmp(&b_digits.len())
                    .then_with(|| a_digits.cmp(&b_digits));
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            (Some(a), Some(b)) => {
                let ordering = a.cmp(b);
                if ordering != Ordering::Equal {
                    return ordering;
                }
                left_chars.next();
                right_chars.next();
            }
        }
    }
}

/// What a tool's `pin` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pin {
    /// The newest stable release (`latest`, `any`, `*`, or no pin).
    Latest,
    /// A version prefix: `1` (major), `1.2` (minor), or `1.2.3` (exact).
    Version(Vec<u64>),
    /// One exact tag, such as `tag:nightly` or `fork-v0.46.2`.
    Tag(String),
    /// The newest release whose tag starts with this prefix, prereleases
    /// included (written `fork-ea-v0.46.7-*`).
    Prefix(String),
}

impl Pin {
    pub fn parse(text: &str) -> Result<Pin> {
        let text = text.trim();
        if text.is_empty() || text == "latest" || text == "any" || text == "*" {
            return Ok(Pin::Latest);
        }
        if let Some(tag) = text.strip_prefix("tag:") {
            if tag.is_empty() {
                bail!("`tag:` needs a tag name after it");
            }
            return Ok(Pin::Tag(tag.to_string()));
        }
        if let Some(prefix) = text.strip_suffix('*') {
            if prefix.contains('*') {
                bail!("only a single trailing `*` is supported in pin `{text}`");
            }
            return Ok(Pin::Prefix(prefix.to_string()));
        }
        if text.contains('*') {
            bail!("`*` is only allowed at the end of pin `{text}`");
        }
        let numeric = text.strip_prefix(['v', 'V']).unwrap_or(text);
        if !numeric.is_empty() && numeric.chars().all(|c| c.is_ascii_digit() || c == '.') {
            let parts: Option<Vec<u64>> =
                numeric.split('.').map(|part| part.parse().ok()).collect();
            return match parts {
                Some(parts) if parts.len() <= 4 => Ok(Pin::Version(parts)),
                _ => bail!("pin `{text}` isn't a valid version (expected 1, 1.2, or 1.2.3)"),
            };
        }
        // Anything else is an exact tag, which covers tags like `fork-v0.46.2`.
        Ok(Pin::Tag(text.to_string()))
    }
}

impl std::fmt::Display for Pin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pin::Latest => formatter.write_str("latest"),
            Pin::Version(parts) => {
                let joined: Vec<String> = parts.iter().map(u64::to_string).collect();
                formatter.write_str(&joined.join("."))
            }
            Pin::Tag(tag) => write!(formatter, "tag:{tag}"),
            Pin::Prefix(prefix) => write!(formatter, "{prefix}*"),
        }
    }
}

/// One installable version offered by a provider (a GitHub release, an npm
/// version, a crate version).
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Candidate {
    /// The tag or version string, used verbatim when installing.
    pub tag: String,
    /// Marked as a prerelease by the provider.
    pub prerelease: bool,
    /// Publication time (RFC 3339), when the provider gives one.
    pub published_at: Option<String>,
}

/// Chooses the candidate a pin resolves to. `candidates` is in provider order
/// (newest first for GitHub). Candidates failing `usable` (for example, a
/// release with no asset for this platform) are skipped.
pub fn select<'a, T>(
    pin: &Pin,
    candidates: &'a [T],
    candidate: impl Fn(&T) -> &Candidate,
    usable: impl Fn(&T) -> bool,
) -> Option<&'a T> {
    let usable_items = candidates.iter().filter(|item| usable(item));
    let is_stable = |item: &&T| {
        let entry = candidate(item);
        !entry.prerelease && Version::parse(&entry.tag).is_some_and(|version| version.pre.is_none())
    };
    match pin {
        Pin::Tag(tag) => usable_items
            .into_iter()
            .find(|item| candidate(item).tag == *tag),
        Pin::Prefix(prefix) => usable_items
            .filter(|item| candidate(item).tag.starts_with(prefix.as_str()))
            .enumerate()
            // Newest by publication time; ties and missing times fall back to provider order.
            .max_by(|(left_index, left), (right_index, right)| {
                let left_time = candidate(left).published_at.as_deref().unwrap_or("");
                let right_time = candidate(right).published_at.as_deref().unwrap_or("");
                left_time
                    .cmp(right_time)
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(_, item)| item),
        Pin::Latest => {
            let stable: Vec<&T> = usable_items.filter(is_stable).collect();
            newest_by_version(&stable, &candidate)
        }
        Pin::Version(prefix) => {
            let matching: Vec<&T> = usable_items
                .filter(is_stable)
                .filter(|item| {
                    Version::parse(&candidate(item).tag)
                        .is_some_and(|version| version.starts_with(prefix))
                })
                .collect();
            newest_by_version(&matching, &candidate)
        }
    }
}

fn newest_by_version<'a, T>(
    items: &[&'a T],
    candidate: &impl Fn(&T) -> &Candidate,
) -> Option<&'a T> {
    items
        .iter()
        .enumerate()
        .max_by(|(left_index, left), (right_index, right)| {
            let left_version = Version::parse(&candidate(left).tag);
            let right_version = Version::parse(&candidate(right).tag);
            left_version
                .cmp(&right_version)
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(_, item)| *item)
}

/// Pulls the first version-looking token out of a command's output, such as
/// `ripgrep 14.1.0 (rev abc)` or `claude 2.1.3 (Claude Code)`.
pub fn version_from_output(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|word| {
        let word = word.trim_matches(|c: char| c == ',' || c == '(' || c == ')');
        let version = Version::parse(word)?;
        (version.parts.len() >= 2).then(|| word.trim_start_matches(['v', 'V']).to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(tag: &str) -> Version {
        Version::parse(tag).unwrap_or_else(|| panic!("{tag} should parse"))
    }

    #[test]
    fn parses_real_world_tag_shapes() {
        assert_eq!(version("v14.1.0").parts, vec![14, 1, 0]);
        assert_eq!(version("14.1.0").parts, vec![14, 1, 0]);
        assert_eq!(version("jq-1.7.1").parts, vec![1, 7, 1]);
        assert_eq!(version("fork-v0.46.6").parts, vec![0, 46, 6]);
        let fork = version("fork-ea-v0.46.7-261103");
        assert_eq!(fork.parts, vec![0, 46, 7]);
        assert_eq!(fork.pre.as_deref(), Some("261103"));
        assert_eq!(
            version("0.8.0-alpha1-pre-redrawhook").pre.as_deref(),
            Some("alpha1-pre-redrawhook")
        );
        assert_eq!(version("v1.2.3+build.5").pre, None);
        assert_eq!(version("release-2.3").parts, vec![2, 3]);
        assert!(Version::parse("nightly").is_none());
    }

    #[test]
    fn prereleases_sort_below_their_release() {
        assert!(version("0.8.0") > version("0.8.0-alpha1-pre-redrawhook"));
        assert!(version("1.0.0-rc10") > version("1.0.0-rc9"));
        assert!(version("1.10.0") > version("1.9.9"));
        assert_eq!(version("1.2").cmp(&version("1.2.0")), Ordering::Equal);
    }

    #[test]
    fn parses_pins() {
        assert_eq!(Pin::parse("").unwrap(), Pin::Latest);
        assert_eq!(Pin::parse("any").unwrap(), Pin::Latest);
        assert_eq!(Pin::parse("14").unwrap(), Pin::Version(vec![14]));
        assert_eq!(Pin::parse("v1.2").unwrap(), Pin::Version(vec![1, 2]));
        assert_eq!(Pin::parse("1.2.3").unwrap(), Pin::Version(vec![1, 2, 3]));
        assert_eq!(
            Pin::parse("tag:nightly").unwrap(),
            Pin::Tag("nightly".into())
        );
        assert_eq!(
            Pin::parse("fork-v0.46.2").unwrap(),
            Pin::Tag("fork-v0.46.2".into())
        );
        assert_eq!(
            Pin::parse("ea-v0.6.0-*").unwrap(),
            Pin::Prefix("ea-v0.6.0-".into())
        );
        assert!(Pin::parse("1.*.3").is_err());
        assert!(Pin::parse("a*b*").is_err());
        assert!(Pin::parse("1..2").is_err());
        assert!(Pin::parse("tag:").is_err());
    }

    fn release(tag: &str, prerelease: bool, published: &str) -> Candidate {
        Candidate {
            tag: tag.into(),
            prerelease,
            published_at: Some(published.into()),
        }
    }

    fn pick<'a>(pin: &str, candidates: &'a [Candidate]) -> Option<&'a str> {
        select(&Pin::parse(pin).unwrap(), candidates, |c| c, |_| true).map(|c| c.tag.as_str())
    }

    #[test]
    fn selects_releases_for_each_pin_kind() {
        let releases = vec![
            release("v15.0.0-rc1", true, "2026-10-05"),
            release("v14.1.1", false, "2026-09-01"),
            release("v14.0.3", false, "2026-05-01"),
            release("v13.9.0", false, "2026-01-01"),
            release("nightly", true, "2026-10-30"),
        ];
        assert_eq!(pick("latest", &releases), Some("v14.1.1"));
        assert_eq!(pick("14", &releases), Some("v14.1.1"));
        assert_eq!(pick("14.0", &releases), Some("v14.0.3"));
        assert_eq!(pick("13.9.0", &releases), Some("v13.9.0"));
        assert_eq!(pick("13.9.1", &releases), None);
        assert_eq!(pick("tag:nightly", &releases), Some("nightly"));
        assert_eq!(pick("v15*", &releases), Some("v15.0.0-rc1"));
    }

    #[test]
    fn prefix_pins_take_the_newest_publication() {
        let releases = vec![
            release("fork-ea-v0.46.7-261020", true, "2026-10-20"),
            release("fork-ea-v0.46.7-261103", true, "2026-11-03"),
            release("fork-v0.46.6", false, "2026-10-01"),
        ];
        assert_eq!(
            pick("fork-ea-v0.46.7-*", &releases),
            Some("fork-ea-v0.46.7-261103")
        );
    }

    #[test]
    fn unusable_candidates_are_skipped() {
        let releases = vec![release("v2.0.0", false, "b"), release("v1.0.0", false, "a")];
        let chosen = select(&Pin::Latest, &releases, |c| c, |c| c.tag != "v2.0.0");
        assert_eq!(chosen.map(|c| c.tag.as_str()), Some("v1.0.0"));
    }

    #[test]
    fn extracts_versions_from_command_output() {
        assert_eq!(
            version_from_output("ripgrep 14.1.0 (rev abc)").as_deref(),
            Some("14.1.0")
        );
        assert_eq!(
            version_from_output("2.1.3 (Claude Code)").as_deref(),
            Some("2.1.3")
        );
        assert_eq!(version_from_output("v22.11.0").as_deref(), Some("22.11.0"));
        assert_eq!(version_from_output("no version here"), None);
    }
}

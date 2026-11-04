//! Host platform detection and the `when` conditions that filter tools and
//! packages by platform.

use serde::{Deserialize, Deserializer, Serialize};

/// The operating system, CPU architecture, and WSL flag of a host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Platform {
    /// `linux` or `macos`, from Rust's `std::env::consts::OS`.
    pub os: String,
    /// `x86_64` or `aarch64`, from `std::env::consts::ARCH`.
    pub arch: String,
    /// True under Windows Subsystem for Linux.
    pub wsl: bool,
}

impl Platform {
    /// Detects the platform this binary is running on.
    pub fn detect() -> Self {
        let os = std::env::consts::OS.to_string();
        let wsl = os == "linux"
            && (std::env::var_os("WSL_DISTRO_NAME").is_some()
                || std::fs::read_to_string("/proc/version")
                    .map(|text| text.to_lowercase().contains("microsoft"))
                    .unwrap_or(false));
        Self {
            os,
            arch: std::env::consts::ARCH.to_string(),
            wsl,
        }
    }

    /// Asset-table keys to try for this platform, most specific first:
    /// `linux-x86_64`, then `linux`, then `any`.
    pub fn asset_keys(&self) -> [String; 3] {
        [
            format!("{}-{}", self.os, self.arch),
            self.os.clone(),
            "any".to_string(),
        ]
    }
}

/// A platform condition. Every field that is set must match; an empty
/// condition always matches.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct When {
    /// Operating systems this applies to (`linux`, `macos`).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub os: Vec<String>,
    /// Architectures this applies to (`x86_64`, `aarch64`).
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub arch: Vec<String>,
    /// Require (true) or exclude (false) WSL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wsl: Option<bool>,
}

impl When {
    pub fn is_empty(&self) -> bool {
        self.os.is_empty() && self.arch.is_empty() && self.wsl.is_none()
    }

    pub fn matches(&self, platform: &Platform) -> bool {
        (self.os.is_empty() || self.os.contains(&platform.os))
            && (self.arch.is_empty() || self.arch.contains(&platform.arch))
            && self.wsl.is_none_or(|wsl| wsl == platform.wsl)
    }
}

/// Accepts either a single string or a list of strings.
pub fn one_or_many<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(value) => vec![value],
        OneOrMany::Many(values) => values,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_wsl() -> Platform {
        Platform {
            os: "linux".into(),
            arch: "x86_64".into(),
            wsl: true,
        }
    }

    #[test]
    fn empty_condition_matches_everything() {
        assert!(When::default().matches(&linux_wsl()));
    }

    #[test]
    fn every_set_field_must_match() {
        let condition = When {
            os: vec!["linux".into()],
            arch: vec!["aarch64".into()],
            wsl: None,
        };
        assert!(!condition.matches(&linux_wsl()));
        let condition = When {
            os: vec!["linux".into()],
            arch: vec![],
            wsl: Some(true),
        };
        assert!(condition.matches(&linux_wsl()));
        let condition = When {
            wsl: Some(false),
            ..When::default()
        };
        assert!(!condition.matches(&linux_wsl()));
    }

    #[test]
    fn asset_keys_go_from_specific_to_generic() {
        assert_eq!(linux_wsl().asset_keys(), ["linux-x86_64", "linux", "any"]);
    }
}

//! Small helpers shared across modules: atomic writes, hashing, and running
//! external commands with readable errors.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Writes `contents` to `path` through a temporary file in the same directory,
/// so readers never see a half-written file. Creates parent directories.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("creating a temporary file in {}", parent.display()))?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    if let Ok(metadata) = std::fs::metadata(path) {
        // Keep the original permissions (an executable script stays executable).
        std::fs::set_permissions(temporary.path(), metadata.permissions())?;
    }
    temporary
        .persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Hex-encoded SHA-256 of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Hex-encoded SHA-256 of a file, streamed so large downloads aren't loaded into memory.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Runs a command, returning its output, and turns a non-zero exit into an
/// error that includes the command line and its stderr.
pub fn run_checked(command: &mut Command) -> Result<Output> {
    let description = describe(command);
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running `{description}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        bail!("`{description}` failed ({}): {detail}", output.status);
    }
    Ok(output)
}

/// Runs a command and returns trimmed stdout as a string.
pub fn run_stdout(command: &mut Command) -> Result<String> {
    let output = run_checked(command)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string())
}

/// Human-readable rendering of a command line for error messages.
pub fn describe(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().to_string()];
    parts.extend(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().to_string()),
    );
    parts.join(" ")
}

/// Finds an executable on `PATH`, plus any extra directories given first.
pub fn find_executable(name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    extra_dirs
        .iter()
        .cloned()
        .chain(std::env::split_paths(&path_var))
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// True when `path` is a regular file with an execute bit set.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Compact UTC timestamp (`20261103T041500Z`) for backup suffixes and stash messages.
pub fn timestamp() -> String {
    let seconds = unix_now();
    let days = seconds / 86_400;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date
/// (Howard Hinnant's algorithm), avoiding a date-time dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Expands a leading `~/` against `home`.
pub fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates_match_known_values() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_760), (2026, 11, 3));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn atomic_write_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("script.sh");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        write_atomic(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}

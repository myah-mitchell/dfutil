//! Scheduled `dfutil update` runs through a systemd user timer, with a cron
//! line printed as the fallback where there's no systemd user session.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = "dfutil-update";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Frequency {
    Daily,
    Weekly,
}

impl Frequency {
    fn calendar(self) -> &'static str {
        match self {
            Frequency::Daily => "daily",
            Frequency::Weekly => "weekly",
        }
    }

    fn cron(self) -> &'static str {
        match self {
            Frequency::Daily => "17 4 * * *",
            Frequency::Weekly => "17 4 * * 1",
        }
    }
}

fn unit_dir(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd/user")
}

/// The unit file contents for running `exe update --yes` on a schedule.
pub fn units(exe: &Path, host_config: &Path, frequency: Frequency) -> (String, String) {
    let quote = |path: &Path| {
        format!(
            "\"{}\"",
            path.display()
                .to_string()
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
        )
    };
    let service = format!(
        "[Unit]\nDescription=Update tools and dotfiles with dfutil\n\n[Service]\nType=oneshot\nExecStart={} --host-config {} update --yes\n",
        quote(exe),
        quote(host_config)
    );
    let timer = format!(
        "[Unit]\nDescription=Scheduled dfutil update\n\n[Timer]\nOnCalendar={}\nPersistent=true\nRandomizedDelaySec=1h\n\n[Install]\nWantedBy=timers.target\n",
        frequency.calendar()
    );
    (service, timer)
}

fn systemctl(args: &[&str]) -> Result<String> {
    crate::util::run_stdout(Command::new("systemctl").arg("--user").args(args))
}

fn has_user_systemd() -> bool {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Outcome of enabling the schedule.
#[derive(Clone, Debug, Serialize)]
pub enum Enabled {
    Systemd {
        timer: PathBuf,
    },
    /// No systemd user session; this crontab line does the same thing.
    Cron {
        line: String,
    },
}

/// Writes and starts the timer, or returns a cron line to add by hand.
pub fn enable(
    home: &Path,
    exe: &Path,
    host_config: &Path,
    frequency: Frequency,
) -> Result<Enabled> {
    if !has_user_systemd() {
        let line = format!(
            "{} '{}' --host-config '{}' update --yes >/dev/null 2>&1",
            frequency.cron(),
            exe.display(),
            host_config.display()
        );
        return Ok(Enabled::Cron { line });
    }
    let directory = unit_dir(home);
    let (service, timer) = units(exe, host_config, frequency);
    crate::util::write_atomic(
        &directory.join(format!("{UNIT}.service")),
        service.as_bytes(),
    )?;
    let timer_path = directory.join(format!("{UNIT}.timer"));
    crate::util::write_atomic(&timer_path, timer.as_bytes())?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &format!("{UNIT}.timer")]).context("enabling the timer")?;
    Ok(Enabled::Systemd { timer: timer_path })
}

/// Stops the timer and removes the unit files.
pub fn disable(home: &Path) -> Result<()> {
    let directory = unit_dir(home);
    let timer = directory.join(format!("{UNIT}.timer"));
    if !timer.exists() {
        bail!(
            "no dfutil timer is installed; a cron line, if you added one, has to be removed by hand"
        );
    }
    if has_user_systemd() {
        systemctl(&["disable", "--now", &format!("{UNIT}.timer")]).ok();
    }
    std::fs::remove_file(&timer)?;
    std::fs::remove_file(directory.join(format!("{UNIT}.service"))).ok();
    if has_user_systemd() {
        systemctl(&["daemon-reload"]).ok();
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub installed: bool,
    pub schedule: Option<String>,
    pub next_run: Option<String>,
    pub last_result: Option<String>,
}

pub fn status(home: &Path) -> Status {
    let timer = unit_dir(home).join(format!("{UNIT}.timer"));
    let installed = timer.exists();
    let schedule = std::fs::read_to_string(&timer).ok().and_then(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("OnCalendar=").map(str::to_string))
    });
    let property = |unit: &str, name: &str| {
        systemctl(&["show", unit, "--property", name, "--value"])
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != "n/a")
    };
    let (next_run, last_result) = if installed && has_user_systemd() {
        (
            property(&format!("{UNIT}.timer"), "NextElapseUSecRealtime"),
            property(&format!("{UNIT}.service"), "Result"),
        )
    } else {
        (None, None)
    };
    Status {
        installed,
        schedule,
        next_run,
        last_result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_quote_paths_and_set_schedule() {
        let (service, timer) = units(
            Path::new("/home/a b/.local/bin/dfutil"),
            Path::new("/h/host.toml"),
            Frequency::Weekly,
        );
        assert!(service.contains(
            "ExecStart=\"/home/a b/.local/bin/dfutil\" --host-config \"/h/host.toml\" update --yes"
        ));
        assert!(timer.contains("OnCalendar=weekly"));
        assert!(timer.contains("Persistent=true"));
    }
}

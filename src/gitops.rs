//! Git work on the content repos, through the `git` CLI so private repos use
//! the user's own SSH keys and credential helpers.

use crate::config::PullMode;
use crate::config::{SourceRoot, Workspace};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;

fn git(repo: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo)
        // Output is parsed and shown in the TUI; the user's config may force color.
        .args([
            "-c",
            "color.ui=never",
            "-c",
            "color.status=never",
            "-c",
            "color.diff=never",
        ])
        .args(["-c", "color.branch=never"])
        // Never stop for a credential prompt in a scheduled or TUI run.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    command
}

fn run(repo: &Path, args: &[&str]) -> Result<String> {
    crate::util::run_stdout(git(repo).args(args))
}

fn succeeds(repo: &Path, args: &[&str]) -> bool {
    git(repo)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Clones secondary sources that aren't on disk yet. Returns the names cloned.
pub fn clone_missing(workspace: &Workspace, log: &dyn Fn(String)) -> Result<Vec<String>> {
    let mut cloned = Vec::new();
    for root in workspace.roots.iter().filter(|root| !root.present) {
        let Some(def) = &root.def else { continue };
        log(format!("cloning source `{}` from {}", root.name, def.url));
        if let Some(parent) = root.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut command = Command::new("git");
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["clone", "--quiet"]);
        if let Some(branch) = &def.branch {
            command.args(["--branch", branch]);
        }
        command.arg("--").arg(&def.url).arg(&root.path);
        crate::util::run_checked(&mut command)
            .with_context(|| format!("cloning source `{}`", root.name))?;
        cloned.push(root.name.clone());
    }
    Ok(cloned)
}

/// Changes waiting upstream for one repo.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Incoming {
    pub root: String,
    pub path: PathBuf,
    pub upstream: Option<String>,
    /// `git log --oneline` of commits not yet pulled.
    pub commits: Vec<String>,
    pub diff: String,
    pub problem: Option<String>,
}

/// Fetches each present repo and reports what pulling would bring in.
pub fn incoming(workspace: &Workspace, fetch: bool) -> Vec<Incoming> {
    present_roots(workspace)
        .map(|root| {
            let mut result = Incoming {
                root: root.name.clone(),
                path: root.path.clone(),
                ..Incoming::default()
            };
            if let Err(error) = (|| -> Result<()> {
                if fetch {
                    run(&root.path, &["fetch", "--quiet"]).context("fetching")?;
                }
                let upstream = upstream(&root.path)?;
                result.commits = lines(&run(
                    &root.path,
                    &["log", "--oneline", "--no-decorate", "HEAD..@{u}"],
                )?);
                result.diff = run(
                    &root.path,
                    &["diff", "--no-ext-diff", "--no-color", "HEAD...@{u}"],
                )?;
                result.upstream = Some(upstream);
                Ok(())
            })() {
                result.problem = Some(format!("{error:#}"));
            }
            result
        })
        .collect()
}

/// Local changes not yet pushed for one repo.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Outgoing {
    pub root: String,
    pub path: PathBuf,
    /// `git status --short` lines.
    pub status: Vec<String>,
    /// Commits not yet pushed.
    pub commits: Vec<String>,
    /// Working-tree diff, including untracked files.
    pub diff: String,
    pub problem: Option<String>,
}

/// Reports uncommitted and unpushed changes in each present repo.
pub fn outgoing(workspace: &Workspace) -> Vec<Outgoing> {
    present_roots(workspace)
        .map(|root| {
            let mut result = Outgoing {
                root: root.name.clone(),
                path: root.path.clone(),
                ..Outgoing::default()
            };
            if let Err(error) = (|| -> Result<()> {
                result.status = lines(&run(&root.path, &["status", "--short"])?);
                result.diff = run(&root.path, &["diff", "--no-ext-diff", "--no-color", "HEAD"])?;
                for untracked in lines(&run(
                    &root.path,
                    &["ls-files", "--others", "--exclude-standard"],
                )?) {
                    // `git diff --no-index` exits 1 when files differ, which they always do here.
                    let output = git(&root.path)
                        .args([
                            "diff",
                            "--no-ext-diff",
                            "--no-color",
                            "--no-index",
                            "--",
                            "/dev/null",
                            &untracked,
                        ])
                        .output()?;
                    result
                        .diff
                        .push_str(&String::from_utf8_lossy(&output.stdout));
                }
                if upstream(&root.path).is_ok() {
                    result.commits = lines(&run(
                        &root.path,
                        &["log", "--oneline", "--no-decorate", "@{u}..HEAD"],
                    )?);
                }
                Ok(())
            })() {
                result.problem = Some(format!("{error:#}"));
            }
            result
        })
        .collect()
}

fn present_roots(workspace: &Workspace) -> impl Iterator<Item = &SourceRoot> {
    workspace
        .roots
        .iter()
        .filter(|root| root.present && root.path.join(".git").exists())
}

fn upstream(repo: &Path) -> Result<String> {
    run(
        repo,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
    )
    .map(|text| text.trim().to_string())
    .context("the current branch has no upstream to compare with")
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// What happened to one repo during a sync.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum SyncResult {
    UpToDate,
    Updated {
        from: String,
        to: String,
    },
    /// Pulling or reapplying local changes conflicted; the repo was put back
    /// exactly as it was.
    RolledBack {
        reason: String,
    },
    Failed {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize)]
pub struct SyncReport {
    pub root: String,
    pub stashed: bool,
    pub result: SyncResult,
}

/// Pulls one repo, stashing local changes (untracked files included) first
/// and reapplying them after. If the pull or the reapply conflicts, the repo
/// is reset to where it started and the stash restored, so nothing is lost.
pub fn sync_repo(repo: &Path, name: &str, mode: PullMode) -> SyncReport {
    let mut stashed = false;
    let result = (|| -> Result<SyncResult> {
        if git_dir_busy(repo)? {
            bail!("a merge, rebase, or cherry-pick is in progress; finish it first");
        }
        upstream(repo)?;
        let old_head = run(repo, &["rev-parse", "HEAD"])?.trim().to_string();
        let dirty = !run(repo, &["status", "--porcelain"])?.trim().is_empty();
        if dirty {
            run(
                repo,
                &[
                    "stash",
                    "push",
                    "--include-untracked",
                    "--quiet",
                    "-m",
                    "dfapp sync",
                ],
            )?;
            stashed = true;
        }
        let pull_args: &[&str] = match mode {
            PullMode::Rebase => &["pull", "--rebase", "--quiet"],
            PullMode::FfOnly => &["pull", "--ff-only", "--quiet"],
        };
        if let Err(error) = run(repo, pull_args) {
            let reason = format!("pull failed: {error:#}");
            restore(repo, &old_head, stashed)?;
            return Ok(SyncResult::RolledBack { reason });
        }
        let new_head = run(repo, &["rev-parse", "HEAD"])?.trim().to_string();
        if stashed && let Err(error) = run(repo, &["stash", "apply", "--quiet"]) {
            let reason = format!("local changes conflict with the pulled changes: {error:#}");
            restore(repo, &old_head, stashed)?;
            return Ok(SyncResult::RolledBack { reason });
        }
        if stashed {
            run(repo, &["stash", "drop", "--quiet"])?;
        }
        Ok(if new_head == old_head {
            SyncResult::UpToDate
        } else {
            SyncResult::Updated {
                from: old_head[..12.min(old_head.len())].to_string(),
                to: new_head[..12.min(new_head.len())].to_string(),
            }
        })
    })();
    SyncReport {
        root: name.to_string(),
        stashed,
        result: result.unwrap_or_else(|error| SyncResult::Failed {
            reason: format!("{error:#}"),
        }),
    }
}

/// Puts a repo back to `old_head` with the dfapp stash reapplied.
fn restore(repo: &Path, old_head: &str, stashed: bool) -> Result<()> {
    if succeeds(repo, &["rev-parse", "--verify", "--quiet", "REBASE_HEAD"])
        || repo_path(repo, "rebase-merge")?.exists()
        || repo_path(repo, "rebase-apply")?.exists()
    {
        run(repo, &["rebase", "--abort"]).ok();
    }
    run(repo, &["reset", "--hard", "--quiet", old_head])?;
    if stashed {
        // A failed `stash apply` may have left untracked files from the stash
        // behind, which would block `stash pop`.
        if let Ok(listing) = run(repo, &["ls-tree", "-r", "--name-only", "stash@{0}^3"]) {
            for file in lines(&listing) {
                std::fs::remove_file(repo.join(&file)).ok();
            }
        }
        run(repo, &["stash", "pop", "--quiet"]).context(
            "restoring local changes from `git stash`; they're still in `git stash list`",
        )?;
    }
    Ok(())
}

fn repo_path(repo: &Path, name: &str) -> Result<PathBuf> {
    let relative = run(repo, &["rev-parse", "--git-path", name])?;
    Ok(repo.join(relative.trim()))
}

fn git_dir_busy(repo: &Path) -> Result<bool> {
    for name in [
        "MERGE_HEAD",
        "rebase-merge",
        "rebase-apply",
        "CHERRY_PICK_HEAD",
    ] {
        if repo_path(repo, name)?.exists() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Syncs every present repo that has an upstream.
pub fn sync_all(workspace: &Workspace) -> Vec<SyncReport> {
    present_roots(workspace)
        .map(|root| sync_repo(&root.path, &root.name, workspace.config.settings.pull))
        .collect()
}

const HOOK_MARKER: &str = "# dfapp pre-commit hook";

/// Installs a pre-commit hook in each present repo that blocks commits
/// containing a secret variable's value. Refuses to replace a hook dfapp
/// didn't write.
pub fn install_hooks(workspace: &Workspace, exe: &Path) -> Result<Vec<PathBuf>> {
    let mut installed = Vec::new();
    for root in present_roots(workspace) {
        let hooks = PathBuf::from(run(&root.path, &["rev-parse", "--git-path", "hooks"])?.trim());
        let hooks = if hooks.is_absolute() {
            hooks
        } else {
            root.path.join(hooks)
        };
        let hook = hooks.join("pre-commit");
        if let Ok(existing) = std::fs::read_to_string(&hook)
            && !existing.contains(HOOK_MARKER)
        {
            bail!(
                "{} already exists and wasn't written by dfapp; add `dfapp hook pre-commit` to it by hand",
                hook.display()
            );
        }
        let quoted = exe.display().to_string().replace('\'', "'\\''");
        let script = format!(
            "#!/bin/sh\n{HOOK_MARKER}: blocks commits that contain a secret template value.\nexec '{quoted}' hook pre-commit --repo \"$(git rev-parse --show-toplevel)\"\n"
        );
        std::fs::create_dir_all(&hooks)?;
        crate::util::write_atomic(&hook, script.as_bytes())?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
        installed.push(hook);
    }
    Ok(installed)
}

/// A secret found in staged changes. Never carries the value itself.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Leak {
    pub file: String,
    pub var: String,
}

/// Scans lines added in the staged diff for secret values.
pub fn staged_leaks(repo: &Path, secrets: &[(String, String)]) -> Result<Vec<Leak>> {
    if secrets.is_empty() {
        return Ok(Vec::new());
    }
    let diff = run(
        repo,
        &["diff", "--cached", "-U0", "--no-ext-diff", "--no-color"],
    )?;
    let mut leaks = Vec::new();
    let mut file = String::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        for (var, value) in secrets {
            if added.contains(value.as_str()) {
                let leak = Leak {
                    file: file.clone(),
                    var: var.clone(),
                };
                if !leaks.contains(&leak) {
                    leaks.push(leak);
                }
            }
        }
    }
    Ok(leaks)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn git_in(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    /// An upstream bare repo and a clone of it, both with one commit.
    pub fn repos(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let upstream = dir.join("upstream.git");
        let seed = dir.join("seed");
        let clone = dir.join("clone");
        std::fs::create_dir_all(&upstream).unwrap();
        git_in(&upstream, &["init", "--bare", "--quiet"]);
        std::fs::create_dir_all(&seed).unwrap();
        git_in(&seed, &["init", "--quiet"]);
        std::fs::write(seed.join("a.txt"), "one\ntwo\n").unwrap();
        git_in(&seed, &["add", "a.txt"]);
        git_in(&seed, &["commit", "--quiet", "-m", "first"]);
        git_in(
            &seed,
            &["remote", "add", "origin", upstream.to_str().unwrap()],
        );
        git_in(&seed, &["push", "--quiet", "-u", "origin", "HEAD:main"]);
        git_in(
            dir,
            &[
                "clone",
                "--quiet",
                upstream.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        git_in(&clone, &["config", "user.name", "t"]);
        git_in(&clone, &["config", "user.email", "t@example.com"]);
        (upstream, seed, clone)
    }

    fn push_change(seed: &Path, text: &str) {
        std::fs::write(seed.join("a.txt"), text).unwrap();
        git_in(seed, &["commit", "--quiet", "-am", "change"]);
        git_in(seed, &["push", "--quiet"]);
    }

    #[test]
    fn sync_stashes_and_reapplies_local_changes() {
        let dir = tempfile::tempdir().unwrap();
        let (_upstream, seed, clone) = repos(dir.path());
        push_change(&seed, "one\ntwo\nthree\n");
        std::fs::write(clone.join("local.txt"), "mine").unwrap();
        let report = sync_repo(&clone, "main", PullMode::Rebase);
        assert!(report.stashed);
        assert!(
            matches!(report.result, SyncResult::Updated { .. }),
            "{report:?}"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("local.txt")).unwrap(),
            "mine"
        );
        assert!(git_in(&clone, &["stash", "list"]).is_empty());
        assert_eq!(
            sync_repo(&clone, "main", PullMode::Rebase).result,
            SyncResult::UpToDate
        );
    }

    #[test]
    fn conflicting_sync_rolls_back_everything() {
        let dir = tempfile::tempdir().unwrap();
        let (_upstream, seed, clone) = repos(dir.path());
        push_change(&seed, "ONE\ntwo\n");
        std::fs::write(clone.join("a.txt"), "uno\ntwo\n").unwrap();
        std::fs::write(clone.join("new.txt"), "untracked").unwrap();
        let before = git_in(&clone, &["rev-parse", "HEAD"]);
        let report = sync_repo(&clone, "main", PullMode::Rebase);
        assert!(
            matches!(report.result, SyncResult::RolledBack { .. }),
            "{report:?}"
        );
        assert_eq!(git_in(&clone, &["rev-parse", "HEAD"]), before);
        assert_eq!(
            std::fs::read_to_string(clone.join("a.txt")).unwrap(),
            "uno\ntwo\n"
        );
        assert_eq!(
            std::fs::read_to_string(clone.join("new.txt")).unwrap(),
            "untracked"
        );
        assert!(git_in(&clone, &["stash", "list"]).is_empty());
    }

    #[test]
    fn diverged_local_commits_that_conflict_roll_back() {
        let dir = tempfile::tempdir().unwrap();
        let (_upstream, seed, clone) = repos(dir.path());
        push_change(&seed, "ONE\ntwo\n");
        std::fs::write(clone.join("a.txt"), "uno\ntwo\n").unwrap();
        git_in(&clone, &["commit", "--quiet", "-am", "local"]);
        let before = git_in(&clone, &["rev-parse", "HEAD"]);
        let report = sync_repo(&clone, "main", PullMode::Rebase);
        assert!(
            matches!(report.result, SyncResult::RolledBack { .. }),
            "{report:?}"
        );
        assert_eq!(git_in(&clone, &["rev-parse", "HEAD"]), before);
        assert!(!clone.join(".git/rebase-merge").exists());
    }

    #[test]
    fn staged_secret_values_are_reported_without_values() {
        let dir = tempfile::tempdir().unwrap();
        let (_upstream, _seed, clone) = repos(dir.path());
        std::fs::write(clone.join("config"), "token = hunter22\n").unwrap();
        git_in(&clone, &["add", "config"]);
        let secrets = vec![("token".to_string(), "hunter22".to_string())];
        let leaks = staged_leaks(&clone, &secrets).unwrap();
        assert_eq!(
            leaks,
            [Leak {
                file: "config".into(),
                var: "token".into()
            }]
        );
        let none = staged_leaks(&clone, &[("other".into(), "absent".into())]).unwrap();
        assert!(none.is_empty());
    }
}

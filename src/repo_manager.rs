use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use tokio::process::Command;

use crate::config::{AppConfig, RepoConfig};

/// Returns the managed repo root directory.
pub fn repos_dir() -> Result<PathBuf> {
    Ok(AppConfig::config_dir()?.join("repos"))
}

/// Returns the managed clone path for a given owner/name.
/// Rejects path traversal attempts (.. or path separators in components).
pub fn managed_path(owner: &str, name: &str) -> Result<PathBuf> {
    validate_path_component(owner, "owner")?;
    validate_path_component(name, "name")?;
    Ok(repos_dir()?.join(owner).join(name))
}

fn validate_path_component(component: &str, label: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains('\0')
    {
        return Err(anyhow!(
            "invalid repo {label}: {component:?} (path traversal rejected)"
        ));
    }
    Ok(())
}

/// Resolves the effective local path for a repo.
/// If `local_path` is set, returns that. Otherwise returns the managed path.
pub fn resolve_local_path(repo_cfg: &RepoConfig) -> Result<PathBuf> {
    if let Some(ref path) = repo_cfg.local_path {
        Ok(path.clone())
    } else {
        managed_path(&repo_cfg.owner, &repo_cfg.name)
    }
}

/// Clones a repo into the managed directory if not already present.
/// Uses HTTPS with the token passed via http.extraHeader to avoid embedding it in .git/config.
/// Returns the path to the clone.
pub async fn ensure_cloned(owner: &str, name: &str, token: &str) -> Result<PathBuf> {
    let path = managed_path(owner, name)?;

    if path.join(".git").exists() {
        tracing::debug!(repo = %format!("{owner}/{name}"), path = %path.display(), "managed clone already exists");
        return Ok(path);
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let url = format!("https://github.com/{owner}/{name}.git");

    tracing::info!(repo = %format!("{owner}/{name}"), path = %path.display(), "cloning repository");

    let output = Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "clone",
            "--single-branch",
            &url,
        ])
        .arg(&path)
        // Pass auth via env vars instead of -c args to avoid leaking token in /proc/pid/cmdline
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run git clone")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("git clone failed: {}", stderr.trim()));
    }

    // Strip the auth header from the stored remote URL to avoid token leakage
    let _ = Command::new("git")
        .args([
            "remote",
            "set-url",
            "origin",
            &format!("https://github.com/{owner}/{name}.git"),
        ])
        .current_dir(&path)
        .output()
        .await;

    tracing::info!(repo = %format!("{owner}/{name}"), "clone complete");
    Ok(path)
}

/// Fetches latest from origin and resets tracked files to match.
/// Does not remove untracked or ignored files, so it is safe for configured
/// user-managed local paths.
pub async fn fetch_latest(repo_path: &Path, token: &str) -> Result<()> {
    fetch_latest_inner(repo_path, token, false).await
}

/// Fetches latest for managed clones and removes untracked/ignored byproducts.
/// Only use this for bot-owned clone directories.
pub async fn fetch_latest_managed(repo_path: &Path, token: &str) -> Result<()> {
    fetch_latest_inner(repo_path, token, true).await
}

async fn fetch_latest_inner(repo_path: &Path, token: &str, clean_untracked: bool) -> Result<()> {
    if !repo_path.join(".git").exists() {
        return Err(anyhow!("not a git repository: {}", repo_path.display()));
    }

    if let Err(err) = run_git_with_auth(
        repo_path,
        token,
        &["-c", "core.hooksPath=/dev/null", "fetch", "origin"],
    )
    .await
    {
        tracing::warn!(path = %repo_path.display(), "git fetch failed: {err}");
        return Err(err);
    }

    // Determine default branch
    let target = git_stdout(
        repo_path,
        &["symbolic-ref", "refs/remotes/origin/HEAD", "--short"],
    )
    .await
    .unwrap_or_else(|_| "origin/HEAD".to_string());

    run_git(repo_path, &["reset", "--hard", &target]).await?;
    if clean_untracked {
        clean_worktree(repo_path).await?;
    }
    Ok(())
}

pub async fn checkout_origin_branch(
    repo_path: &Path,
    local_branch: &str,
    origin_branch: &str,
) -> Result<()> {
    run_git(
        repo_path,
        &[
            "checkout",
            "-B",
            local_branch,
            &format!("origin/{origin_branch}"),
        ],
    )
    .await
}

pub async fn hard_reset_to_origin(repo_path: &Path, origin_branch: &str) -> Result<()> {
    run_git(
        repo_path,
        &["reset", "--hard", &format!("origin/{origin_branch}")],
    )
    .await?;
    clean_worktree(repo_path).await
}

pub async fn clean_ignored_worktree(repo_path: &Path) -> Result<()> {
    run_git(repo_path, &["clean", "-fdX"]).await
}

async fn clean_worktree(repo_path: &Path) -> Result<()> {
    run_git(repo_path, &["clean", "-fdx"]).await
}

pub async fn current_head_sha(repo_path: &Path) -> Result<String> {
    git_stdout(repo_path, &["rev-parse", "HEAD"]).await
}

pub async fn changed_file_paths(repo_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run git status --porcelain=v1 -z")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git status --porcelain=v1 -z failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(parse_status_paths(&output.stdout))
}

pub async fn commit_paths(repo_path: &Path, paths: &[String], message: &str) -> Result<()> {
    if paths.is_empty() {
        return Err(anyhow!("cannot commit empty path set"));
    }

    let mut add = Command::new("git");
    add.arg("add").arg("--").args(paths).current_dir(repo_path);
    let output = add
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run git add")?;

    if !output.status.success() {
        return Err(anyhow!(
            "git add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    commit_staged(repo_path, message).await
}

async fn commit_staged(repo_path: &Path, message: &str) -> Result<()> {
    run_git(
        repo_path,
        &[
            "-c",
            "user.name=pr-reviewer",
            "-c",
            "user.email=pr-reviewer@users.noreply.github.com",
            "commit",
            "-m",
            message,
        ],
    )
    .await
}

fn parse_status_paths(status: &[u8]) -> Vec<String> {
    let mut paths = Vec::new();
    let mut records = status
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty());

    while let Some(record) = records.next() {
        if record.len() < 4 {
            continue;
        }

        let status = &record[..2];
        paths.push(String::from_utf8_lossy(&record[3..]).into_owned());

        if status[0] == b'R' || status[0] == b'C' || status[1] == b'R' || status[1] == b'C' {
            let _old_path = records.next();
        }
    }

    paths
}

pub async fn push_head(repo_path: &Path, token: &str, branch: &str) -> Result<()> {
    run_git_with_auth(
        repo_path,
        token,
        &[
            "push",
            "--force-with-lease",
            "origin",
            &format!("HEAD:{branch}"),
        ],
    )
    .await
}

async fn run_git(repo_path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn run_git_with_auth(repo_path: &Path, token: &str, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "http.extraHeader")
        .env(
            "GIT_CONFIG_VALUE_0",
            format!("Authorization: Bearer {token}"),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

async fn git_stdout(repo_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        return Err(anyhow!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Delete a managed clone directory.
/// Returns true if the directory existed and was removed.
pub async fn purge(owner: &str, name: &str) -> Result<bool> {
    let path = managed_path(owner, name)?;
    if !path.exists() {
        return Ok(false);
    }
    tokio::fs::remove_dir_all(&path)
        .await
        .with_context(|| format!("failed to remove managed clone at {}", path.display()))?;

    // Clean up empty parent (owner dir) if it's now empty
    if let Some(parent) = path.parent() {
        if let Ok(mut entries) = tokio::fs::read_dir(parent).await {
            if entries.next_entry().await?.is_none() {
                let _ = tokio::fs::remove_dir(parent).await;
            }
        }
    }

    Ok(true)
}

/// Remove managed clones that are not in the active repo list.
/// Only removes repos from the managed repos/ directory (not manually configured local_path repos).
pub async fn cleanup(active_repos: &[RepoConfig]) -> Result<Vec<String>> {
    let base = repos_dir()?;
    if !base.exists() {
        return Ok(vec![]);
    }

    let mut removed = Vec::new();
    let mut owners = tokio::fs::read_dir(&base).await?;

    while let Some(owner_entry) = owners.next_entry().await? {
        if !owner_entry.file_type().await?.is_dir() {
            continue;
        }
        let owner_name = owner_entry.file_name().to_string_lossy().to_string();
        let mut repos = tokio::fs::read_dir(owner_entry.path()).await?;

        while let Some(repo_entry) = repos.next_entry().await? {
            if !repo_entry.file_type().await?.is_dir() {
                continue;
            }
            let repo_name = repo_entry.file_name().to_string_lossy().to_string();
            let full_name = format!("{owner_name}/{repo_name}");

            let is_active = active_repos
                .iter()
                .any(|r| r.is_managed() && r.full_name().eq_ignore_ascii_case(&full_name));

            if !is_active {
                if let Err(err) = tokio::fs::remove_dir_all(repo_entry.path()).await {
                    tracing::warn!(repo = %full_name, error = %err, "failed to remove orphaned clone");
                } else {
                    removed.push(full_name);
                }
            }
        }

        // Remove empty owner dir
        if let Ok(mut entries) = tokio::fs::read_dir(owner_entry.path()).await {
            if entries.next_entry().await?.is_none() {
                let _ = tokio::fs::remove_dir(owner_entry.path()).await;
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::{
        changed_file_paths, clean_worktree, fetch_latest, fetch_latest_managed, parse_status_paths,
    };
    use std::fs;
    use std::path::Path;
    use tokio::process::Command;

    #[tokio::test]
    async fn clean_worktree_removes_untracked_and_ignored_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init"]).await;

        fs::write(dir.path().join(".gitignore"), "*.log\n").expect("write gitignore");
        fs::write(dir.path().join("tracked.txt"), "tracked").expect("write tracked");
        run_git(dir.path(), &["add", ".gitignore", "tracked.txt"]).await;
        run_git(
            dir.path(),
            &[
                "-c",
                "user.name=pr-reviewer",
                "-c",
                "user.email=pr-reviewer@users.noreply.github.com",
                "commit",
                "-m",
                "initial",
            ],
        )
        .await;

        fs::write(dir.path().join("untracked.txt"), "stale").expect("write untracked");
        fs::write(dir.path().join("ignored.log"), "stale").expect("write ignored");

        clean_worktree(dir.path()).await.expect("clean worktree");

        assert!(!dir.path().join("untracked.txt").exists());
        assert!(!dir.path().join("ignored.log").exists());
        assert!(dir.path().join("tracked.txt").exists());
    }

    #[tokio::test]
    async fn fetch_latest_preserves_local_untracked_and_ignored_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_remote_backed_repo(dir.path()).await;

        fs::write(repo.join("untracked.txt"), "local").expect("write untracked");
        fs::write(repo.join("ignored.log"), "local").expect("write ignored");

        fetch_latest(&repo, "").await.expect("fetch latest");

        assert!(repo.join("untracked.txt").exists());
        assert!(repo.join("ignored.log").exists());
        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).expect("read tracked"),
            "updated"
        );
    }

    #[tokio::test]
    async fn fetch_latest_managed_removes_untracked_and_ignored_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = create_remote_backed_repo(dir.path()).await;

        fs::write(repo.join("untracked.txt"), "local").expect("write untracked");
        fs::write(repo.join("ignored.log"), "local").expect("write ignored");

        fetch_latest_managed(&repo, "")
            .await
            .expect("fetch latest managed");

        assert!(!repo.join("untracked.txt").exists());
        assert!(!repo.join("ignored.log").exists());
        assert_eq!(
            fs::read_to_string(repo.join("tracked.txt")).expect("read tracked"),
            "updated"
        );
    }

    #[test]
    fn parse_status_paths_handles_nul_delimited_paths() {
        let paths = parse_status_paths(
            b" M src/file with spaces.rs\0?? weird\"quote.rs\0A  path\\with\\slashes.txt\0",
        );

        assert_eq!(
            paths,
            vec![
                "src/file with spaces.rs",
                "weird\"quote.rs",
                "path\\with\\slashes.txt"
            ]
        );
    }

    #[test]
    fn parse_status_paths_keeps_rename_destination() {
        let paths =
            parse_status_paths(b"R  new path.rs\0old path.rs\0C  copy path.rs\0source path.rs\0");

        assert_eq!(paths, vec!["new path.rs", "copy path.rs"]);
    }

    #[tokio::test]
    async fn changed_file_paths_handles_special_untracked_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        run_git(dir.path(), &["init"]).await;

        fs::write(dir.path().join("file with spaces.txt"), "spaces").expect("write spaces");
        fs::write(dir.path().join("quote\"name.txt"), "quote").expect("write quote");

        let mut paths = changed_file_paths(dir.path()).await.expect("changed paths");
        paths.sort();

        assert_eq!(paths, vec!["file with spaces.txt", "quote\"name.txt"]);
    }

    async fn create_remote_backed_repo(root: &Path) -> std::path::PathBuf {
        let remote = root.join("remote.git");
        let seed = root.join("seed");
        let work = root.join("work");

        run_cmd(
            root,
            &["init", "--bare", remote.to_str().expect("remote path")],
        )
        .await;
        fs::create_dir_all(&seed).expect("create seed");
        run_git(&seed, &["init"]).await;
        fs::write(seed.join(".gitignore"), "*.log\n").expect("write gitignore");
        fs::write(seed.join("tracked.txt"), "initial").expect("write tracked");
        run_git(&seed, &["add", ".gitignore", "tracked.txt"]).await;
        commit_all(&seed, "initial").await;
        run_git(&seed, &["branch", "-M", "main"]).await;
        run_git(
            &seed,
            &[
                "remote",
                "add",
                "origin",
                remote.to_str().expect("remote path"),
            ],
        )
        .await;
        run_git(&seed, &["push", "-u", "origin", "main"]).await;
        run_cmd(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).await;

        run_cmd(
            root,
            &[
                "clone",
                remote.to_str().expect("remote path"),
                work.to_str().expect("work path"),
            ],
        )
        .await;

        fs::write(seed.join("tracked.txt"), "updated").expect("update tracked");
        run_git(&seed, &["add", "tracked.txt"]).await;
        commit_all(&seed, "update tracked").await;
        run_git(&seed, &["push", "origin", "main"]).await;

        work
    }

    async fn commit_all(repo_path: &Path, message: &str) {
        run_git(
            repo_path,
            &[
                "-c",
                "user.name=pr-reviewer",
                "-c",
                "user.email=pr-reviewer@users.noreply.github.com",
                "commit",
                "-m",
                message,
            ],
        )
        .await;
    }

    async fn run_git(repo_path: &Path, args: &[&str]) {
        run_cmd(repo_path, args).await;
    }

    async fn run_cmd(repo_path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_path)
            .output()
            .await
            .expect("run git");

        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

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

/// Fetches latest from origin and resets working tree to match.
/// Safe to call on managed clones (never user-edited).
pub async fn fetch_latest(repo_path: &Path, token: &str) -> Result<()> {
    if !repo_path.join(".git").exists() {
        return Err(anyhow!("not a git repository: {}", repo_path.display()));
    }

    let fetch = Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "fetch", "origin"])
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
        .context("failed to run git fetch")?;

    if !fetch.status.success() {
        let stderr = String::from_utf8_lossy(&fetch.stderr);
        tracing::warn!(path = %repo_path.display(), "git fetch failed: {}", stderr.trim());
        return Err(anyhow!("git fetch failed: {}", stderr.trim()));
    }

    // Determine default branch
    let head_ref = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD", "--short"])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await;

    let target = match head_ref {
        Ok(ref out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => "origin/HEAD".to_string(),
    };

    let reset = Command::new("git")
        .args(["reset", "--hard", &target])
        .current_dir(repo_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .context("failed to run git reset")?;

    if !reset.status.success() {
        let stderr = String::from_utf8_lossy(&reset.stderr);
        return Err(anyhow!("git reset --hard failed: {}", stderr.trim()));
    }

    Ok(())
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

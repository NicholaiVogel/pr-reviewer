use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::config::{ForkPolicy, RepoConfig};
use crate::github::types::PullRequest;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ForkDecision {
    Skip(String),
    Limited,
    Full,
}

pub fn evaluate_fork_policy(repo_cfg: &RepoConfig, pr: &PullRequest) -> ForkDecision {
    let author = pr.user.login.as_str();
    if repo_cfg
        .trusted_authors
        .iter()
        .any(|trusted| trusted.eq_ignore_ascii_case(author))
    {
        return ForkDecision::Full;
    }

    if !is_fork_pr(repo_cfg, pr) {
        return ForkDecision::Full;
    }

    match repo_cfg.fork_policy {
        ForkPolicy::Ignore => ForkDecision::Skip("fork PR ignored by policy".to_string()),
        ForkPolicy::Limited => ForkDecision::Limited,
        ForkPolicy::Full => ForkDecision::Full,
    }
}

pub fn is_fork_pr(repo_cfg: &RepoConfig, pr: &PullRequest) -> bool {
    let expected = format!("{}/{}", repo_cfg.owner, repo_cfg.name);
    match &pr.head.repo {
        Some(repo) => !repo.full_name.eq_ignore_ascii_case(&expected),
        None => true,
    }
}

pub fn validate_relative_path(path: &Path) -> Result<()> {
    if path.is_absolute() {
        return Err(anyhow!(
            "absolute paths are not allowed: {}",
            path.display()
        ));
    }

    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(anyhow!("parent directory traversal is not allowed"));
        }
    }

    Ok(())
}

pub fn canonicalize_within_root(root: &Path, rel_path: &Path) -> Result<PathBuf> {
    validate_relative_path(rel_path)?;
    let root = root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize root {}", root.display()))?;
    let full = root.join(rel_path);
    let canonical = full
        .canonicalize()
        .with_context(|| format!("failed to canonicalize path {}", full.display()))?;

    if !canonical.starts_with(&root) {
        return Err(anyhow!(
            "path escapes repository root: {}",
            canonical.display()
        ));
    }

    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn traversal_is_rejected() {
        let p = Path::new("../etc/passwd");
        assert!(validate_relative_path(p).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn symlink_outside_root_is_rejected() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&repo).expect("repo dir");
        fs::create_dir_all(&outside).expect("outside dir");

        let link = repo.join("bad-link");
        symlink(&outside, &link).expect("symlink create");

        let err = canonicalize_within_root(&repo, Path::new("bad-link")).expect_err("should fail");
        assert!(err.to_string().contains("escapes repository root"));
    }
}

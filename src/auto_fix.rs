use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::config::{AppConfig, RepoConfig};
use crate::github::client::{GitHubClient, ListPullsResult};
use crate::github::types::{CreatePullRequestRequest, PullRequest};
use crate::harness;
use crate::harness::spawn::{run_harness, HarnessRunRequest};
use crate::repo_manager;
use crate::store::db::Database;

#[derive(Debug, Clone)]
pub struct AutoFixOutcome {
    pub scanned_sha: String,
    pub pr_number: Option<u64>,
    pub changed_files: usize,
    pub duration_secs: f64,
}

pub async fn scan_and_open_pr(
    config: &AppConfig,
    repo_cfg: &RepoConfig,
    github: &GitHubClient,
    db: &Database,
) -> Result<Option<AutoFixOutcome>> {
    if !repo_cfg.auto_fix.enabled {
        return Ok(None);
    }

    if !repo_cfg.is_managed() {
        tracing::warn!(
            repo = %repo_cfg.full_name(),
            "auto-fix is only supported for managed clones; skipping local_path repo"
        );
        return Ok(None);
    }

    let start = Instant::now();
    let repo_name = repo_cfg.full_name();
    let repo_path = repo_cfg.effective_local_path()?;
    repo_manager::fetch_latest_managed(&repo_path, github.token()).await?;
    repo_manager::checkout_origin_branch(
        &repo_path,
        &repo_cfg.auto_fix.base_branch,
        &repo_cfg.auto_fix.base_branch,
    )
    .await?;

    let base_sha = repo_manager::current_head_sha(&repo_path).await?;
    let already_scanned = db.get_last_auto_fix_sha(&repo_name).await?;
    if !should_scan_base_sha(already_scanned.as_deref(), &base_sha) {
        return Ok(None);
    }
    db.set_last_auto_fix_scan(&repo_name, &base_sha, None)
        .await?;

    let branch = format!(
        "{}/{}",
        repo_cfg.auto_fix.branch_prefix.trim_end_matches('/'),
        &base_sha[..12.min(base_sha.len())]
    );
    repo_manager::checkout_origin_branch(&repo_path, &branch, &repo_cfg.auto_fix.base_branch)
        .await?;

    let harness_kind = repo_cfg.resolved_harness(config);
    let harness = harness::for_kind(harness_kind);
    let prompt = build_auto_fix_prompt(repo_cfg, &base_sha);
    let output = run_harness(
        harness.as_ref(),
        HarnessRunRequest {
            prompt,
            model: repo_cfg.resolved_model(config).to_string(),
            reasoning_effort: repo_cfg.resolved_reasoning_effort(config),
            working_dir: repo_path.clone(),
            timeout_secs: config.harness.timeout_secs,
        },
    )
    .await
    .context("auto-fix harness failed")?;

    cleanup_harness_artifacts(&repo_path).await;
    repo_manager::clean_ignored_worktree(&repo_path).await?;

    if output.exit_code.is_some_and(|code| code != 0) {
        return Err(anyhow!(
            "auto-fix harness exited with {:?}: {}",
            output.exit_code,
            output.stderr.trim()
        ));
    }

    let changed_paths = repo_manager::changed_file_paths(&repo_path).await?;
    if changed_paths.is_empty() {
        repo_manager::checkout_origin_branch(
            &repo_path,
            &repo_cfg.auto_fix.base_branch,
            &repo_cfg.auto_fix.base_branch,
        )
        .await?;
        db.set_last_auto_fix_scan(&repo_name, &base_sha, None)
            .await?;
        return Ok(Some(AutoFixOutcome {
            scanned_sha: base_sha,
            pr_number: None,
            changed_files: 0,
            duration_secs: start.elapsed().as_secs_f64(),
        }));
    }

    let changed_files = changed_paths.len();
    if changed_files > repo_cfg.auto_fix.max_changed_files {
        repo_manager::hard_reset_to_origin(&repo_path, &repo_cfg.auto_fix.base_branch).await?;
        return Err(anyhow!(
            "auto-fix changed {changed_files} files, exceeding max_changed_files={}",
            repo_cfg.auto_fix.max_changed_files
        ));
    }

    let (committable_paths, rejected_paths) = split_reviewable_auto_fix_paths(changed_paths);
    if !rejected_paths.is_empty() {
        repo_manager::hard_reset_to_origin(&repo_path, &repo_cfg.auto_fix.base_branch).await?;
        return Err(anyhow!(
            "auto-fix produced non-reviewable byproduct paths: {}",
            rejected_paths.join(", ")
        ));
    }

    repo_manager::commit_paths(
        &repo_path,
        &committable_paths,
        &format!(
            "fix: address automated scan findings for {}",
            &base_sha[..12.min(base_sha.len())]
        ),
    )
    .await?;
    repo_manager::push_head(&repo_path, github.token(), &branch).await?;

    let pr_body = build_pr_body(&base_sha);
    let existing_pr = match github
        .list_open_prs(&repo_cfg.owner, &repo_cfg.name, None)
        .await?
    {
        ListPullsResult::Updated { prs, .. } => {
            find_existing_auto_fix_pr(prs, &branch, &repo_cfg.auto_fix.base_branch, &repo_name)
        }
        ListPullsResult::NotModified { .. } => None,
    };

    let pr_number = if let Some(pr) = existing_pr {
        pr.number
    } else {
        github
            .create_pull_request_with_options(
                &repo_cfg.owner,
                &repo_cfg.name,
                &CreatePullRequestRequest {
                    title: "fix: address automated scan findings".to_string(),
                    head: branch.clone(),
                    base: repo_cfg.auto_fix.base_branch.clone(),
                    body: pr_body,
                    draft: repo_cfg.auto_fix.draft_pr,
                },
            )
            .await?
            .number
    };

    db.set_last_auto_fix_scan(&repo_name, &base_sha, Some(pr_number))
        .await?;

    Ok(Some(AutoFixOutcome {
        scanned_sha: base_sha,
        pr_number: Some(pr_number),
        changed_files,
        duration_secs: start.elapsed().as_secs_f64(),
    }))
}

fn should_scan_base_sha(already_scanned: Option<&str>, base_sha: &str) -> bool {
    already_scanned != Some(base_sha)
}

fn find_existing_auto_fix_pr(
    prs: Vec<PullRequest>,
    branch: &str,
    base_branch: &str,
    head_repo: &str,
) -> Option<PullRequest> {
    prs.into_iter().find(|pr| {
        pr.head.ref_name == branch
            && pr.base.ref_name == base_branch
            && pr
                .head
                .repo
                .as_ref()
                .is_some_and(|repo| repo.full_name.eq_ignore_ascii_case(head_repo))
    })
}

fn split_reviewable_auto_fix_paths(paths: Vec<String>) -> (Vec<String>, Vec<String>) {
    paths
        .into_iter()
        .partition(|path| is_reviewable_auto_fix_path(path))
}

fn is_reviewable_auto_fix_path(path: &str) -> bool {
    if path.starts_with('/')
        || path.contains('\0')
        || path.split('/').any(|part| {
            matches!(
                part,
                "" | "."
                    | ".."
                    | ".git"
                    | "target"
                    | "node_modules"
                    | ".cache"
                    | "tmp"
                    | "temp"
                    | "coverage"
                    | "dist"
                    | "build"
            )
        })
    {
        return false;
    }

    let Some(name) = path.rsplit('/').next() else {
        return false;
    };
    if matches!(
        name,
        "Cargo.lock"
            | "Dockerfile"
            | "Makefile"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "bun.lock"
            | "bun.lockb"
            | "yarn.lock"
    ) {
        return true;
    }

    let Some(ext) = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    else {
        return false;
    };
    matches!(
        ext.as_str(),
        "astro"
            | "c"
            | "cc"
            | "cfg"
            | "conf"
            | "cpp"
            | "cs"
            | "css"
            | "go"
            | "h"
            | "hpp"
            | "html"
            | "java"
            | "js"
            | "json"
            | "jsx"
            | "kt"
            | "lock"
            | "md"
            | "php"
            | "py"
            | "rb"
            | "rs"
            | "scss"
            | "sh"
            | "sql"
            | "svelte"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "vue"
            | "xml"
            | "yaml"
            | "yml"
    )
}

fn build_auto_fix_prompt(repo_cfg: &RepoConfig, base_sha: &str) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are running inside a managed repository clone for pr-reviewer.\n");
    prompt.push_str("Audit the current default-branch checkout for concrete security issues and correctness bugs introduced or present at this release point.\n");
    prompt.push_str("Make a small, focused code patch only when you find a real issue. Do not do broad refactors, formatting churn, dependency upgrades, or speculative hardening.\n");
    prompt.push_str("Prefer adding or updating focused tests when the repository's test layout makes that practical. Keep the patch reviewable.\n");
    prompt.push_str(
        "If you find no concrete issue worth fixing, leave the working tree unchanged.\n\n",
    );
    prompt.push_str(&format!("Repository: {}\n", repo_cfg.full_name()));
    prompt.push_str(&format!("Base SHA: {base_sha}\n"));
    if let Some(instructions) = repo_cfg.custom_instructions.as_deref() {
        prompt.push_str("\nRepository instructions:\n");
        prompt.push_str(instructions);
        prompt.push('\n');
    }
    prompt
}

fn build_pr_body(base_sha: &str) -> String {
    let mut body = String::new();
    body.push_str("Automated bug/security scan from `pr-reviewer`.\n\n");
    body.push_str(&format!("Base SHA scanned: `{base_sha}`\n\n"));
    body.push_str("The generated branch contains the focused patch produced by the configured local harness. Review the commit diff for the exact changes.\n");
    body
}

async fn cleanup_harness_artifacts(repo_path: &std::path::Path) {
    let _ = tokio::fs::remove_file(crate::harness::codex::last_message_path(repo_path)).await;
}

#[cfg(test)]
mod tests {
    use super::{
        build_pr_body, find_existing_auto_fix_pr, is_reviewable_auto_fix_path,
        should_scan_base_sha, split_reviewable_auto_fix_paths,
    };
    use crate::github::types::{PullRequest, PullRequestBase, PullRequestHead, RepoRef, User};

    #[test]
    fn pr_body_does_not_include_harness_output() {
        let body = build_pr_body("abc123");

        assert!(body.contains("Base SHA scanned: `abc123`"));
        assert!(!body.contains("Harness summary"));
        assert!(!body.contains("```"));
    }

    #[test]
    fn same_base_sha_is_not_scanned_again() {
        assert!(!should_scan_base_sha(Some("abc123"), "abc123"));
        assert!(should_scan_base_sha(None, "abc123"));
        assert!(should_scan_base_sha(Some("oldsha"), "abc123"));
    }

    #[test]
    fn existing_auto_fix_pr_reuses_matching_branch_and_base() {
        let matching = test_pr(42, "pr-reviewer/auto-fix/abc123", "main");
        let wrong_branch = test_pr(43, "feature/manual", "main");
        let wrong_base = test_pr(44, "pr-reviewer/auto-fix/abc123", "develop");

        let found = find_existing_auto_fix_pr(
            vec![wrong_branch, wrong_base, matching],
            "pr-reviewer/auto-fix/abc123",
            "main",
            "owner/repo",
        )
        .expect("matching PR should be reused");

        assert_eq!(found.number, 42);
    }

    #[test]
    fn existing_auto_fix_pr_ignores_fork_with_matching_branch_and_base() {
        let fork = test_pr_with_head_repo(42, "pr-reviewer/auto-fix/abc123", "main", "fork/repo");
        let upstream = test_pr(43, "pr-reviewer/auto-fix/abc123", "main");

        let found = find_existing_auto_fix_pr(
            vec![fork, upstream],
            "pr-reviewer/auto-fix/abc123",
            "main",
            "owner/repo",
        )
        .expect("upstream PR should be reused");

        assert_eq!(found.number, 43);
    }

    #[test]
    fn auto_fix_paths_allow_source_config_and_docs_only() {
        let (allowed, rejected) = split_reviewable_auto_fix_paths(vec![
            "src/lib.rs".to_string(),
            "tests/regression.ts".to_string(),
            "docs/fix.md".to_string(),
            "config.example.toml".to_string(),
            "debug.log".to_string(),
            "target/tmp/output.txt".to_string(),
            ".cache/harness.json".to_string(),
        ]);

        assert_eq!(
            allowed,
            vec![
                "src/lib.rs",
                "tests/regression.ts",
                "docs/fix.md",
                "config.example.toml"
            ]
        );
        assert_eq!(
            rejected,
            vec!["debug.log", "target/tmp/output.txt", ".cache/harness.json"]
        );
    }

    #[test]
    fn auto_fix_paths_reject_traversal_and_extensionless_byproducts() {
        assert!(!is_reviewable_auto_fix_path("../src/lib.rs"));
        assert!(!is_reviewable_auto_fix_path("tmpfile"));
        assert!(!is_reviewable_auto_fix_path("notes/debug"));
        assert!(is_reviewable_auto_fix_path("Cargo.lock"));
        assert!(is_reviewable_auto_fix_path("scripts/fix.sh"));
    }

    fn test_pr(number: u64, head_branch: &str, base_branch: &str) -> PullRequest {
        test_pr_with_head_repo(number, head_branch, base_branch, "owner/repo")
    }

    fn test_pr_with_head_repo(
        number: u64,
        head_branch: &str,
        base_branch: &str,
        head_repo: &str,
    ) -> PullRequest {
        PullRequest {
            number,
            title: "test".to_string(),
            body: None,
            draft: false,
            state: "open".to_string(),
            user: test_user("reviewer"),
            head: PullRequestHead {
                sha: "abc123".to_string(),
                ref_name: head_branch.to_string(),
                repo: Some(test_repo_ref(head_repo, "owner")),
            },
            base: PullRequestBase {
                ref_name: base_branch.to_string(),
                repo: test_repo_ref("owner/repo", "owner"),
            },
            html_url: None,
            updated_at: None,
            closed_at: None,
            merged_at: None,
        }
    }

    fn test_repo_ref(full_name: &str, owner: &str) -> RepoRef {
        RepoRef {
            full_name: full_name.to_string(),
            fork: false,
            owner: test_user(owner),
        }
    }

    fn test_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: Some("User".to_string()),
        }
    }
}

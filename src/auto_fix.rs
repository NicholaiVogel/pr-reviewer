use std::time::Instant;

use anyhow::{anyhow, Context, Result};

use crate::config::{AppConfig, RepoConfig};
use crate::github::client::{GitHubClient, ListPullsResult};
use crate::github::types::{CreatePullRequestRequest, IssueComment, PullRequest};
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
    if !repo_cfg.auto_fix.enabled || !repo_cfg.auto_fix.scan_default_branch {
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

pub async fn repair_marked_pr(
    config: &AppConfig,
    repo_cfg: &RepoConfig,
    github: &GitHubClient,
    db: &Database,
    pr_data: &PullRequest,
    issue_comments: &[IssueComment],
    authenticated_user: Option<&str>,
    command_request_body: Option<&str>,
) -> Result<Option<AutoFixOutcome>> {
    if !repo_cfg.auto_fix.enabled {
        return Ok(None);
    }

    let repair_context = if let Some(marker) = matching_repair_marker(
        pr_data,
        issue_comments,
        config.defaults.bot_name.as_str(),
        authenticated_user,
    ) {
        marker.comment_body
    } else if let Some(body) = command_request_body {
        format!("Maintainer requested repair with `@pr-reviewer fix`.\n\nCommand comment:\n{body}")
    } else {
        return Ok(None);
    };

    if !repo_cfg.is_managed() {
        tracing::warn!(
            repo = %repo_cfg.full_name(),
            pr = pr_data.number,
            "auto-fix PR repair is only supported for managed clones; skipping local_path repo"
        );
        return Ok(None);
    }

    if !is_same_repo_pr(repo_cfg, pr_data) {
        tracing::info!(
            repo = %repo_cfg.full_name(),
            pr = pr_data.number,
            head_repo = ?pr_data.head.repo.as_ref().map(|repo| repo.full_name.as_str()),
            "auto-fix PR repair only mutates same-repo PR branches"
        );
        return Ok(None);
    }

    let repair_allowed = db
        .can_auto_fix_repair(
            &repo_cfg.full_name(),
            pr_data.number as i64,
            &pr_data.head.sha,
            repo_cfg.auto_fix.max_repairs_per_pr,
            repo_cfg.auto_fix.max_repairs_per_head,
        )
        .await?;
    if !repair_allowed {
        tracing::debug!(
            repo = %repo_cfg.full_name(),
            pr = pr_data.number,
            sha = %pr_data.head.sha,
            "auto-fix repair skipped by iteration limits"
        );
        return Ok(None);
    }

    let start = Instant::now();
    let repo_path = repo_cfg.effective_local_path()?;
    repo_manager::fetch_latest_managed(&repo_path, github.token()).await?;
    repo_manager::checkout_origin_branch(
        &repo_path,
        &pr_data.head.ref_name,
        &pr_data.head.ref_name,
    )
    .await?;

    let checked_out_sha = repo_manager::current_head_sha(&repo_path).await?;
    if checked_out_sha != pr_data.head.sha {
        repo_manager::hard_reset_to_origin(&repo_path, &pr_data.head.ref_name).await?;
        return Err(anyhow!(
            "auto-fix repair marker targeted {}, but checked out {} for {}#{}",
            pr_data.head.sha,
            checked_out_sha,
            repo_cfg.full_name(),
            pr_data.number
        ));
    }

    let harness_kind = repo_cfg.resolved_harness(config);
    let harness = harness::for_kind(harness_kind);
    let prompt = build_pr_repair_prompt(repo_cfg, pr_data, &repair_context);
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
    .context("auto-fix PR repair harness failed")?;

    cleanup_harness_artifacts(&repo_path).await;
    repo_manager::clean_ignored_worktree(&repo_path).await?;

    if output.exit_code.is_some_and(|code| code != 0) {
        repo_manager::hard_reset_to_origin(&repo_path, &pr_data.head.ref_name).await?;
        return Err(anyhow!(
            "auto-fix PR repair harness exited with {:?}: {}",
            output.exit_code,
            output.stderr.trim()
        ));
    }

    let changed_paths = repo_manager::changed_file_paths(&repo_path).await?;
    if changed_paths.is_empty() {
        repo_manager::hard_reset_to_origin(&repo_path, &pr_data.head.ref_name).await?;
        return Ok(Some(AutoFixOutcome {
            scanned_sha: pr_data.head.sha.clone(),
            pr_number: Some(pr_data.number),
            changed_files: 0,
            duration_secs: start.elapsed().as_secs_f64(),
        }));
    }

    let changed_files = changed_paths.len();
    if changed_files > repo_cfg.auto_fix.max_changed_files {
        repo_manager::hard_reset_to_origin(&repo_path, &pr_data.head.ref_name).await?;
        return Err(anyhow!(
            "auto-fix PR repair changed {changed_files} files, exceeding max_changed_files={}",
            repo_cfg.auto_fix.max_changed_files
        ));
    }

    let (committable_paths, rejected_paths) = split_reviewable_auto_fix_paths(changed_paths);
    if !rejected_paths.is_empty() {
        repo_manager::hard_reset_to_origin(&repo_path, &pr_data.head.ref_name).await?;
        return Err(anyhow!(
            "auto-fix PR repair produced non-reviewable byproduct paths: {}",
            rejected_paths.join(", ")
        ));
    }

    repo_manager::commit_paths(
        &repo_path,
        &committable_paths,
        &format!(
            "fix: address pr-reviewer findings for {}",
            &pr_data.head.sha[..12.min(pr_data.head.sha.len())]
        ),
    )
    .await?;
    repo_manager::push_head(&repo_path, github.token(), &pr_data.head.ref_name).await?;
    db.record_auto_fix_repair(
        &repo_cfg.full_name(),
        pr_data.number as i64,
        &pr_data.head.sha,
    )
    .await?;

    Ok(Some(AutoFixOutcome {
        scanned_sha: pr_data.head.sha.clone(),
        pr_number: Some(pr_data.number),
        changed_files,
        duration_secs: start.elapsed().as_secs_f64(),
    }))
}

pub fn has_matching_repair_marker(
    pr_data: &PullRequest,
    issue_comments: &[IssueComment],
    bot_name: &str,
    authenticated_user: Option<&str>,
) -> bool {
    matching_repair_marker(pr_data, issue_comments, bot_name, authenticated_user).is_some()
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

#[derive(Debug, Clone)]
struct RepairMarker {
    comment_body: String,
}

fn matching_repair_marker(
    pr_data: &PullRequest,
    issue_comments: &[IssueComment],
    bot_name: &str,
    authenticated_user: Option<&str>,
) -> Option<RepairMarker> {
    issue_comments.iter().rev().find_map(|comment| {
        if !login_matches_bot(&comment.user.login, bot_name, authenticated_user) {
            return None;
        }
        let marker = parse_repair_marker(&comment.body)?;
        if marker.pr_number == pr_data.number && marker.sha == pr_data.head.sha {
            return Some(RepairMarker {
                comment_body: comment.body.clone(),
            });
        }
        None
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct ParsedRepairMarker {
    pr_number: u64,
    sha: String,
}

fn parse_repair_marker(body: &str) -> Option<ParsedRepairMarker> {
    let marker_start = "<!-- pr-reviewer-action:fix-required";
    let start = body.find(marker_start)?;
    let after_start = &body[start + marker_start.len()..];
    let end = after_start.find("-->")?;
    let marker_body = &after_start[..end];
    let mut pr_number = None;
    let mut sha = None;
    for token in marker_body.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        match key {
            "pr" => pr_number = value.parse::<u64>().ok(),
            "sha" => sha = Some(value.to_string()),
            _ => {}
        }
    }
    Some(ParsedRepairMarker {
        pr_number: pr_number?,
        sha: sha?,
    })
}

fn login_matches_bot(login: &str, bot_name: &str, authenticated_user: Option<&str>) -> bool {
    login.eq_ignore_ascii_case(bot_name)
        || authenticated_user.is_some_and(|user| login.eq_ignore_ascii_case(user))
}

fn is_same_repo_pr(repo_cfg: &RepoConfig, pr_data: &PullRequest) -> bool {
    pr_data.head.repo.as_ref().is_some_and(|repo| {
        !repo.fork && repo.full_name.eq_ignore_ascii_case(&repo_cfg.full_name())
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

fn build_pr_repair_prompt(
    repo_cfg: &RepoConfig,
    pr_data: &PullRequest,
    durable_review_comment: &str,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are running inside a managed repository clone for pr-reviewer.\n");
    prompt.push_str("The checkout is the exact pull request head that pr-reviewer already reviewed. Apply a small, focused patch that addresses the blocking pr-reviewer findings only.\n");
    prompt.push_str("Do not broaden scope, reformat unrelated files, upgrade dependencies, or change behavior that was not called out by the review. Prefer focused tests when practical.\n");
    prompt.push_str("If the review finding is wrong, stale, already fixed, security-sensitive, or cannot be repaired safely in this checkout, leave the working tree unchanged.\n\n");
    prompt.push_str(&format!("Repository: {}\n", repo_cfg.full_name()));
    prompt.push_str(&format!("Pull request: #{}\n", pr_data.number));
    prompt.push_str(&format!("Reviewed head SHA: {}\n", pr_data.head.sha));
    prompt.push_str(&format!("Branch: {}\n", pr_data.head.ref_name));
    if let Some(instructions) = repo_cfg.custom_instructions.as_deref() {
        prompt.push_str("\nRepository instructions:\n");
        prompt.push_str(instructions);
        prompt.push('\n');
    }
    prompt.push_str("\nDurable pr-reviewer comment:\n");
    prompt.push_str(durable_review_comment);
    prompt.push('\n');
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
        build_pr_body, build_pr_repair_prompt, find_existing_auto_fix_pr,
        has_matching_repair_marker, is_reviewable_auto_fix_path, is_same_repo_pr,
        parse_repair_marker, should_scan_base_sha, split_reviewable_auto_fix_paths,
    };
    use crate::config::RepoConfig;
    use crate::github::types::{
        IssueComment, PullRequest, PullRequestBase, PullRequestHead, RepoRef, User,
    };

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

    #[test]
    fn parses_repair_marker() {
        let parsed = parse_repair_marker(
            "<!-- pr-reviewer-action:fix-required pr=42 sha=abc123 finding=review-feedback -->",
        )
        .expect("marker");

        assert_eq!(parsed.pr_number, 42);
        assert_eq!(parsed.sha, "abc123");
    }

    #[test]
    fn matching_repair_marker_requires_bot_and_live_sha() {
        let pr = test_pr(42, "feature/fix", "main");
        let comments = vec![
            issue_comment(
                1,
                "contributor",
                "<!-- pr-reviewer-action:fix-required pr=42 sha=abc123 finding=review-feedback -->",
            ),
            issue_comment(
                2,
                "pr-reviewer",
                "<!-- pr-reviewer-action:fix-required pr=42 sha=old finding=review-feedback -->",
            ),
            issue_comment(
                3,
                "pr-reviewer",
                "<!-- pr-reviewer-action:fix-required pr=42 sha=abc123 finding=review-feedback -->",
            ),
        ];

        assert!(has_matching_repair_marker(
            &pr,
            &comments,
            "pr-reviewer",
            None
        ));
    }

    #[test]
    fn same_repo_pr_rejects_forks() {
        let cfg = RepoConfig {
            owner: "owner".to_string(),
            name: "repo".to_string(),
            local_path: None,
            harness: None,
            model: None,
            reasoning_effort: None,
            fork_policy: Default::default(),
            trusted_authors: vec![],
            ignore_paths: vec![],
            custom_instructions: None,
            gitnexus: false,
            workflow: vec![],
            auto_fix: Default::default(),
        };
        let same_repo = test_pr(42, "feature/fix", "main");
        let fork = test_pr_with_head_repo(43, "feature/fix", "main", "fork/repo");

        assert!(is_same_repo_pr(&cfg, &same_repo));
        assert!(!is_same_repo_pr(&cfg, &fork));
    }

    #[test]
    fn pr_repair_prompt_includes_review_marker_without_harness_output_contract() {
        let cfg = RepoConfig {
            owner: "owner".to_string(),
            name: "repo".to_string(),
            local_path: None,
            harness: None,
            model: None,
            reasoning_effort: None,
            fork_policy: Default::default(),
            trusted_authors: vec![],
            ignore_paths: vec![],
            custom_instructions: None,
            gitnexus: false,
            workflow: vec![],
            auto_fix: Default::default(),
        };
        let pr = test_pr(42, "feature/fix", "main");
        let prompt = build_pr_repair_prompt(
            &cfg,
            &pr,
            "<!-- pr-reviewer-action:fix-required pr=42 sha=abc123 -->\nReview body",
        );

        assert!(prompt.contains("Pull request: #42"));
        assert!(prompt.contains("Reviewed head SHA: abc123"));
        assert!(prompt.contains("Review body"));
        assert!(prompt.contains("leave the working tree unchanged"));
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

    fn issue_comment(id: u64, login: &str, body: &str) -> IssueComment {
        IssueComment {
            id,
            body: body.to_string(),
            user: test_user(login),
            author_association: None,
            created_at: format!("2026-04-29T00:{id:02}:00Z"),
        }
    }

    fn test_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: Some("User".to_string()),
        }
    }
}

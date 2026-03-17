use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tempfile::tempdir;

use crate::config::{AppConfig, HarnessKind, RepoConfig};
use crate::context::diff_parser::{parse_unified_diff, DiffSide};
use crate::context::gitnexus;
use crate::context::retriever::{assemble_context, ContextMode};
use crate::github::comments;
use crate::github::types::{
    CreateReviewComment, CreateReviewRequest, PullRequest, PullRequestReview, ReviewComment,
};
use crate::github::{client::GitHubClient, pr};
use crate::harness;
use crate::harness::spawn::{run_harness, HarnessRunRequest};
use crate::review::parser::{
    parse_reply_output, parse_review_output, ConfidenceRatings, ParseOutcome, ReplyParseOutcome,
    ReviewVerdict,
};
use crate::review::prompt::build_review_prompt;
use crate::safety::{evaluate_fork_policy, ForkDecision};
use crate::store::db::{dedupe_key, Database, ReviewClaim};

const IN_PROGRESS_COMMENT_TIMEOUT_SECS: u64 = 4;
const IN_PROGRESS_COMMENT_MAX_CHARS: usize = 220;

#[derive(Debug, Clone, Default)]
pub struct ReviewOptions {
    pub dry_run: bool,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewRunResult {
    pub repo: String,
    pub pr_number: u64,
    pub sha: String,
    pub status: String,
    pub verdict: Option<ReviewVerdict>,
    pub comments_posted: usize,
}

#[derive(Clone)]
pub struct ReviewEngine {
    config: Arc<AppConfig>,
    github: GitHubClient,
    db: Database,
    authenticated_user: Option<String>,
}

impl ReviewEngine {
    pub fn new(config: Arc<AppConfig>, github: GitHubClient, db: Database) -> Self {
        Self {
            config,
            github,
            db,
            authenticated_user: None,
        }
    }

    pub async fn init(&mut self) {
        self.authenticated_user = self.github.get_authenticated_user().await.ok();
    }

    pub async fn review_pr(
        &self,
        repo_cfg: &RepoConfig,
        pr_number: u64,
        options: ReviewOptions,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        let pr_data =
            pr::get_pull_request(&self.github, &repo_cfg.owner, &repo_cfg.name, pr_number)
                .await
                .with_context(|| format!("failed fetching PR {repo_name}#{pr_number}"))?;

        self.review_existing_pr(repo_cfg, &pr_data, options).await
    }

    pub async fn review_existing_pr(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        options: ReviewOptions,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        if pr_data.draft && !self.config.defaults.review_drafts {
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:draft".to_string(),
                verdict: None,
                comments_posted: 0,
            });
        }

        let fork_decision = evaluate_fork_policy(repo_cfg, pr_data);
        if let ForkDecision::Skip(reason) = fork_decision {
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: format!("skipped:{reason}"),
                verdict: None,
                comments_posted: 0,
            });
        }

        let harness_kind = options
            .harness
            .unwrap_or_else(|| repo_cfg.resolved_harness(&self.config));
        let model = options
            .model
            .clone()
            .or_else(|| repo_cfg.model.clone())
            .unwrap_or_else(|| self.config.harness.model.clone());
        let dedupe = dedupe_key(
            &repo_name,
            pr_data.number,
            &pr_data.head.sha,
            harness_kind.as_str(),
        );

        let claim = ReviewClaim {
            dedupe_key: dedupe.clone(),
            repo: repo_name.clone(),
            pr_number: pr_data.number as i64,
            sha: pr_data.head.sha.clone(),
            harness: harness_kind.as_str().to_string(),
            model: Some(model.clone()),
        };

        if !self.db.claim_review(claim).await? {
            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:duplicate".to_string(),
                verdict: None,
                comments_posted: 0,
            });
        }

        let dry_run = options.dry_run || self.config.defaults.dry_run;

        // Fetch existing reviews once — used for both in-progress guard and duplicate check
        // inside run_review_pipeline, avoiding a redundant API call.
        let existing_reviews = comments::get_existing_reviews(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();

        // Check if we already posted a review for this SHA before posting in-progress comment,
        // to avoid orphaned "started reviewing" comments when the already-posted guard fires.
        if !dry_run {
            let reviewer_owner = match &self.authenticated_user {
                Some(login) => login.clone(),
                None => self
                    .github
                    .get_authenticated_user()
                    .await
                    .unwrap_or_else(|_| self.config.defaults.bot_name.clone()),
            };

            let already_posted = existing_reviews.iter().any(|r| {
                r.commit_id.as_deref() == Some(pr_data.head.sha.as_str())
                    && login_matches_bot(
                        &r.user.login,
                        self.config.defaults.bot_name.as_str(),
                        Some(reviewer_owner.as_str()),
                    )
            });

            if !already_posted {

                let has_prior_bot_review = existing_reviews
                    .iter()
                    .any(|r| {
                        login_matches_bot(
                            &r.user.login,
                            self.config.defaults.bot_name.as_str(),
                            Some(reviewer_owner.as_str()),
                        )
                    });

                let in_progress = self
                    .compose_in_progress_comment(
                        harness_kind,
                        &model,
                        &reviewer_owner,
                        &pr_data.user.login,
                        &pr_data.title,
                        &pr_data.head.sha,
                        has_prior_bot_review,
                    )
                    .await;
                if let Err(err) = comments::create_issue_comment(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                    &in_progress,
                )
                .await
                {
                    tracing::warn!(
                        repo = %repo_cfg.full_name(),
                        pr = pr_data.number,
                        error = %err,
                        "failed to post in-progress review comment"
                    );
                }
            }
        }

        let started = Instant::now();
        let outcome = self
            .run_review_pipeline(repo_cfg, pr_data, options, harness_kind, &model, existing_reviews)
            .await;

        match outcome {
            Ok(mut result) => {
                self.db
                    .upsert_pr_state(
                        &repo_name,
                        pr_data.number as i64,
                        Some(&pr_data.head.sha),
                        None,
                    )
                    .await?;
                result.sha = pr_data.head.sha.clone();
                Ok(result)
            }
            Err(err) => {
                let duration = started.elapsed().as_secs_f64();
                let _ = self
                    .db
                    .fail_review(&dedupe, &format!("{err:#}"), duration)
                    .await;
                Err(err)
            }
        }
    }

    async fn compose_in_progress_comment(
        &self,
        harness_kind: HarnessKind,
        model: &str,
        owner_login: &str,
        pr_author: &str,
        pr_title: &str,
        sha: &str,
        has_prior_bot_review: bool,
    ) -> String {
        let fallback = build_in_progress_comment_fallback(
            owner_login,
            pr_author,
            pr_title,
            sha,
            has_prior_bot_review,
        );

        let prompt = build_in_progress_comment_prompt(
            owner_login,
            pr_author,
            pr_title,
            sha,
            has_prior_bot_review,
        );

        let temp = match tempdir() {
            Ok(dir) => dir,
            Err(err) => {
                tracing::warn!(error = %err, "failed to create temp dir for in-progress comment prompt");
                return fallback;
            }
        };

        let harness_impl = harness::for_kind(harness_kind);
        let output = match run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model: model.to_string(),
                working_dir: temp.path().to_path_buf(),
                timeout_secs: IN_PROGRESS_COMMENT_TIMEOUT_SECS,
            },
        )
        .await
        {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    harness = harness_kind.as_str(),
                    error = %err,
                    "in-progress comment generation failed; using fallback"
                );
                return fallback;
            }
        };

        let Some(raw) = extract_in_progress_comment(&output.stdout, &output.stderr) else {
            return fallback;
        };
        let Some(mut composed) = normalize_in_progress_comment(&raw) else {
            return fallback;
        };

        if has_prior_bot_review && looks_like_reintroduction(&composed) {
            return fallback;
        }

        if !has_prior_bot_review && !composed.contains("[pr-reviewer](") {
            let suffix = format!(" Powered by [pr-reviewer]({}).", pr_reviewer_project_url());
            if composed.len() + suffix.len() > IN_PROGRESS_COMMENT_MAX_CHARS {
                return fallback;
            }
            composed.push_str(&suffix);
        }

        if composed.len() > IN_PROGRESS_COMMENT_MAX_CHARS {
            return fallback;
        }

        composed
    }

    async fn run_review_pipeline(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        options: ReviewOptions,
        harness_kind: HarnessKind,
        model: &str,
        existing_reviews: Vec<PullRequestReview>,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        let dedupe = dedupe_key(
            &repo_name,
            pr_data.number,
            &pr_data.head.sha,
            harness_kind.as_str(),
        );

        let prior_state = self
            .db
            .get_pr_state(&repo_name, pr_data.number as i64)
            .await?;
        let prior_sha = prior_state
            .and_then(|s| s.last_reviewed_sha)
            .filter(|sha| sha != &pr_data.head.sha);

        let diff = pr::get_diff(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await?;
        let parsed_diff = parse_unified_diff(&diff).context("failed parsing diff")?;

        let changed_files: Vec<String> = parsed_diff
            .files
            .iter()
            .map(|f| {
                if f.new_path != "/dev/null" {
                    f.new_path.clone()
                } else {
                    f.old_path.clone()
                }
            })
            .collect();

        let gitnexus_context = if repo_cfg.gitnexus {
            match repo_cfg.effective_local_path() {
                Ok(local) => {
                    // Fetch latest for managed clones so GitNexus index is fresh
                    if repo_cfg.is_managed() {
                        if let Err(err) = crate::repo_manager::fetch_latest(
                            &local,
                            self.github.token(),
                        )
                        .await
                        {
                            tracing::warn!(
                                repo = %repo_cfg.full_name(),
                                error = %err,
                                "failed to fetch latest for managed clone; using stale state"
                            );
                        }
                    }
                    match gitnexus::query_context(&local, &changed_files).await {
                        Ok(Some(value)) => Some(value),
                        _ => None,
                    }
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let context_mode = match evaluate_fork_policy(repo_cfg, pr_data) {
            ForkDecision::Limited => ContextMode::Limited,
            _ => ContextMode::Full,
        };

        let assembled = assemble_context(
            &self.github,
            repo_cfg,
            pr_data,
            &diff,
            &parsed_diff,
            &self.config.defaults,
            context_mode,
            gitnexus_context.as_deref(),
        )
        .await?;

        let mut context_with_history = assembled.text.clone();
        let prior_reviews_context = build_prior_reviews_context(
            &existing_reviews,
            self.config.defaults.bot_name.as_str(),
            pr_data.head.sha.as_str(),
        );
        if !prior_reviews_context.is_empty() {
            context_with_history.push_str("\n## Prior Bot Reviews\n");
            context_with_history.push_str(&prior_reviews_context);
            context_with_history.push('\n');
        }

        if let Some(previous_sha) = prior_sha.as_deref() {
            if let Ok(delta_diff) = self
                .github
                .get_compare_diff(
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    previous_sha,
                    &pr_data.head.sha,
                )
                .await
            {
                context_with_history.push_str(&format!(
                    "\n## Incremental Diff Since Last Reviewed SHA ({previous_sha} -> {})\n```diff\n",
                    pr_data.head.sha
                ));
                context_with_history.push_str(&truncate_lines(&delta_diff, 400));
                context_with_history.push_str("\n```\n");
            }
        }

        let prompt = build_review_prompt(
            repo_cfg,
            pr_data,
            &context_with_history,
            &self.config.defaults.bot_name,
        );

        let temp = tempdir().context("failed to create temp working dir")?;
        let prompt_path = temp.path().join("review_prompt.md");
        tokio::fs::write(&prompt_path, &prompt)
            .await
            .context("failed to write prompt file")?;

        let harness_impl = harness::for_kind(harness_kind);
        let harness_output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model: model.to_string(),
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let parse_outcome = parse_review_output(&harness_output.stdout, &harness_output.stderr)?;

        let mut body = String::new();
        let mut verdict = ReviewVerdict::Comment;
        let mut confidence: Option<ConfidenceRatings> = None;
        let mut inline_comments: Vec<CreateReviewComment> = Vec::new();
        let mut unmapped_comments: Vec<String> = Vec::new();

        match parse_outcome {
            ParseOutcome::Structured(review) => {
                body.push_str(&review.summary);
                verdict = review.verdict;
                confidence = Some(review.confidence.clone());
                body.push_str("\n\n");
                body.push_str(&format_confidence_markdown(&review.confidence));
                body.push_str("\n\n");
                body.push_str(&format_confidence_json_block(&review.confidence));
                for comment in review.comments {
                    let path = clean_comment_path(&comment.file);
                    let right = parsed_diff.position_for(&path, comment.line, DiffSide::Right);
                    let left = parsed_diff.position_for(&path, comment.line, DiffSide::Left);

                    if let Some(position) = right.or(left) {
                        inline_comments.push(CreateReviewComment {
                            path: path.clone(),
                            line: comment.line,
                            side: match position.side {
                                DiffSide::Left => "LEFT".to_string(),
                                DiffSide::Right => "RIGHT".to_string(),
                            },
                            body: comment.body,
                        });
                    } else {
                        unmapped_comments
                            .push(format!("- {}:{} - {}", path, comment.line, comment.body));
                    }
                }
            }
            ParseOutcome::RawSummary(raw) => {
                body.push_str("Harness output could not be parsed as structured JSON.\n\n");
                body.push_str(&raw);
            }
            ParseOutcome::Empty => {
                return Err(anyhow!("harness returned empty output"));
            }
        }

        if !unmapped_comments.is_empty() {
            body.push_str("\n\nUnmapped findings (not on changed lines):\n");
            for line in unmapped_comments {
                body.push_str(&line);
                body.push('\n');
            }
        }

        let dry_run = options.dry_run || self.config.defaults.dry_run;
        if dry_run {
            self.db
                .complete_review(
                    &dedupe,
                    inline_comments.len() as i64,
                    Some(confidence_verdict_label(verdict, confidence.as_ref())),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "completed:dry-run".to_string(),
                verdict: Some(verdict),
                comments_posted: inline_comments.len(),
            });
        }

        if existing_reviews.iter().any(|r| {
            r.commit_id.as_deref() == Some(pr_data.head.sha.as_str())
                && login_matches_bot(
                    &r.user.login,
                    self.config.defaults.bot_name.as_str(),
                    self.authenticated_user.as_deref(),
                )
        }) {
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(confidence_verdict_label(
                        ReviewVerdict::Comment,
                        confidence.as_ref(),
                    )),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:already-posted".to_string(),
                verdict: Some(ReviewVerdict::Comment),
                comments_posted: 0,
            });
        }

        // GitHub rejects APPROVE/REQUEST_CHANGES on your own PRs — downgrade to COMMENT.
        // Use cached user from init(), fall back to live API call, fail-closed on total failure.
        let event = match verdict {
            ReviewVerdict::Comment => verdict.as_github_event(),
            ReviewVerdict::Approve | ReviewVerdict::RequestChanges => {
                let bot_login = match &self.authenticated_user {
                    Some(u) => Some(u.clone()),
                    None => self.github.get_authenticated_user().await.ok(),
                };
                match bot_login.as_deref() {
                    Some(u) if u.eq_ignore_ascii_case(&pr_data.user.login) => "COMMENT",
                    Some(_) => verdict.as_github_event(),
                    None => {
                        tracing::warn!("could not determine authenticated user; downgrading verdict to COMMENT");
                        "COMMENT"
                    }
                }
            }
        };

        let request = CreateReviewRequest {
            body: body.clone(),
            event: event.to_string(),
            comments: inline_comments.clone(),
        };

        let post_result = comments::create_review(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            &request,
        )
        .await;

        // If review post failed (e.g. 422 on self-review), retry as COMMENT without inline comments.
        // Append the inline comments to the body so they're not silently lost.
        let mut used_retry = false;
        let post_result = if let Err(ref first_err) = post_result {
            let err_str = format!("{first_err}");
            if err_str.contains("(422") || err_str.contains("Can not approve") {
                tracing::warn!(
                    repo = %repo_cfg.full_name(),
                    pr = pr_data.number,
                    error = %first_err,
                    "review post failed with 422; retrying as COMMENT with inline comments folded into body"
                );
                let mut retry_body = body.clone();
                if !inline_comments.is_empty() {
                    retry_body
                        .push_str("\n\n**Inline comments (could not post as line comments):**\n");
                    for c in &inline_comments {
                        retry_body.push_str(&format!("- `{}:{}` — {}\n", c.path, c.line, c.body));
                    }
                }
                let retry_request = CreateReviewRequest {
                    body: retry_body,
                    event: "COMMENT".to_string(),
                    comments: vec![],
                };
                used_retry = true;
                comments::create_review(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                    &retry_request,
                )
                .await
            } else {
                post_result
            }
        } else {
            post_result
        };

        if let Err(err) = post_result {
            let fallback =
                format!("Review post failed; fallback summary posted.\n\nError: {err}\n\n{body}");
            comments::create_issue_comment(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
                &fallback,
            )
            .await?;
        }

        // When 422 retry posted comments in the body instead of inline, record 0 inline comments.
        // Also store the actual posted event (which may differ from the LLM verdict after
        // self-review downgrade or 422 retry).
        let posted_inline = if used_retry { 0 } else { inline_comments.len() as i64 };
        let actual_event = if used_retry { "COMMENT" } else { event };
        self.db
            .complete_review(
                &dedupe,
                posted_inline,
                Some(actual_event),
                harness_output.duration_secs,
                assembled.files_included as i64,
                assembled.diff_lines as i64,
            )
            .await?;

        Ok(ReviewRunResult {
            repo: repo_name,
            pr_number: pr_data.number,
            sha: pr_data.head.sha.clone(),
            status: "completed".to_string(),
            verdict: Some(verdict),
            comments_posted: if used_retry { 0 } else { inline_comments.len() },
        })
    }

    pub async fn reply_to_comment(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        comment: &ReviewComment,
    ) -> Result<()> {
        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let model = repo_cfg.resolved_model(&self.config).to_string();

        let latest_diff = pr::get_diff(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();

        let all_comments = comments::get_review_comments(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            None,
        )
        .await
        .unwrap_or_default();
        let existing_reviews = comments::get_existing_reviews(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();
        let thread_history = build_thread_history(&all_comments, comment.id);
        let baseline_confidence = extract_latest_confidence_snapshot(
            self.config.defaults.bot_name.as_str(),
            &existing_reviews,
            &all_comments,
        );

        let mut file_snippet = String::new();
        if let Some(line) = comment.line {
            if let Ok(Some(content)) = self
                .github
                .get_file_content(
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    &comment.path,
                    &pr_data.head.sha,
                )
                .await
            {
                file_snippet = extract_file_snippet(&content, line as usize, 12);
            }
        }

        let mut prompt = String::new();
        prompt.push_str("You are replying as an automated code review assistant.\n");
        prompt.push_str("Write a concise, technical response to this review thread comment.\n");
        prompt.push_str("Use the latest diff and thread context to determine whether the issue appears fixed.\n");
        prompt.push_str("If fixed, say so explicitly and reference concrete evidence. If not fixed, explain what remains.\n");
        prompt.push_str("Output JSON in a fenced block tagged exactly `pr-review-reply-json` with this schema:\n");
        prompt.push_str("{\"reply\": string, \"confidence\": {style_maintainability, repo_convention_adherence, merge_conflict_detection, security_vulnerability_detection, injection_risk_detection, attack_surface_risk_assessment, future_hardening_guidance, scope_alignment, duplication_awareness, tooling_pattern_leverage, functional_completeness, pattern_correctness, documentation_coverage}}\n");
        prompt.push_str("All confidence values must be integers from 1 to 10.\n\n");
        prompt.push_str(&format!("Repo: {}/{}\n", repo_cfg.owner, repo_cfg.name));
        prompt.push_str(&format!("PR: #{}\n", pr_data.number));
        prompt.push_str(&format!("Target comment id: {}\n", comment.id));
        prompt.push_str(&format!("Target comment path: {}\n", comment.path));
        prompt.push_str(&format!("Target comment line: {:?}\n", comment.line));
        if let Some(conf) = baseline_confidence.as_ref() {
            prompt.push_str("\nLatest known confidence ratings:\n");
            prompt.push_str(&format_confidence_markdown(conf));
            prompt.push('\n');
        }
        prompt.push_str("\nThread history:\n");
        prompt.push_str(&thread_history);
        prompt.push_str("\n\nLatest diff (truncated):\n```diff\n");
        prompt.push_str(&truncate_lines(&latest_diff, 300));
        prompt.push_str("\n```\n");

        if !file_snippet.is_empty() {
            prompt.push_str("\nLatest file snippet around referenced line:\n```\n");
            prompt.push_str(&file_snippet);
            prompt.push_str("\n```\n");
        }

        let temp = tempdir().context("failed to create temp dir for reply")?;
        let harness_impl = harness::for_kind(harness_kind);
        let output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let reply = match parse_reply_output(&output.stdout, &output.stderr)? {
            ReplyParseOutcome::Structured(update) => {
                let mut text = update.reply;
                text.push_str("\n\n");
                text.push_str(&format_confidence_markdown(&update.confidence));
                text.push_str("\n\n");
                text.push_str(&format_confidence_json_block(&update.confidence));
                text
            }
            ReplyParseOutcome::Raw(raw) => raw,
            ReplyParseOutcome::Empty => String::new(),
        };

        if reply.is_empty() {
            return Err(anyhow!("harness returned empty reply output"));
        }

        comments::reply_to_review_comment(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
            comment.id,
            &reply,
        )
        .await?;

        Ok(())
    }
}

fn clean_comment_path(path: &str) -> String {
    path.trim().trim_start_matches("./").to_string()
}

fn build_prior_reviews_context(
    reviews: &[PullRequestReview],
    bot_name: &str,
    current_sha: &str,
) -> String {
    let mut relevant: Vec<&PullRequestReview> = reviews
        .iter()
        .filter(|r| {
            r.user.login.eq_ignore_ascii_case(bot_name)
                && r.commit_id.as_deref() != Some(current_sha)
        })
        .collect();
    if relevant.is_empty() {
        return String::new();
    }
    relevant.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
    let tail = if relevant.len() > 5 {
        &relevant[relevant.len() - 5..]
    } else {
        &relevant[..]
    };

    let mut out = String::new();
    for review in tail {
        let sha = review.commit_id.as_deref().unwrap_or("unknown-sha");
        let state = review.state.as_deref().unwrap_or("UNKNOWN");
        let submitted = review.submitted_at.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "- sha={} state={} submitted_at={}\n",
            sha, state, submitted
        ));
        if let Some(body) = review.body.as_deref() {
            let summary = body.lines().take(8).collect::<Vec<_>>().join("\n");
            out.push_str(summary.trim());
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn truncate_lines(input: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (idx, line) in input.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... [truncated]\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn build_thread_history(comments: &[ReviewComment], target_comment_id: u64) -> String {
    if comments.is_empty() {
        return "No thread history available.".to_string();
    }

    let parent_map: HashMap<u64, Option<u64>> =
        comments.iter().map(|c| (c.id, c.in_reply_to_id)).collect();
    let target_root = find_thread_root_id(&parent_map, target_comment_id);

    let mut thread_comments: Vec<&ReviewComment> = comments
        .iter()
        .filter(|c| find_thread_root_id(&parent_map, c.id) == target_root)
        .collect();

    thread_comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));

    let mut out = String::new();
    for c in thread_comments {
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            c.created_at,
            c.user.login,
            c.body.replace('\n', " ")
        ));
    }
    if out.is_empty() {
        out.push_str("No thread history available.");
    }
    out
}

fn find_thread_root_id(parent_map: &HashMap<u64, Option<u64>>, id: u64) -> u64 {
    let mut current = id;
    let mut guard = 0usize;
    while guard < 64 {
        guard += 1;
        match parent_map.get(&current).copied().flatten() {
            Some(parent) => current = parent,
            None => return current,
        }
    }
    current
}

fn extract_file_snippet(content: &str, line: usize, radius: usize) -> String {
    if line == 0 {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let start = line.saturating_sub(radius + 1);
    let end = (line + radius).min(lines.len());

    let mut out = String::new();
    for idx in start..end {
        out.push_str(&format!("{:>5}: {}\n", idx + 1, lines[idx]));
    }
    out
}

fn short_sha(sha: &str) -> &str {
    if sha.len() <= 8 {
        sha
    } else {
        &sha[..8]
    }
}

#[derive(Debug, Deserialize)]
struct InProgressCommentPayload {
    comment: String,
}

fn build_in_progress_comment_prompt(
    owner_login: &str,
    pr_author: &str,
    pr_title: &str,
    sha: &str,
    has_prior_bot_review: bool,
) -> String {
    let clean_owner = owner_login.trim().trim_start_matches('@');
    let owner_label = if clean_owner.is_empty() {
        "@the-owner".to_string()
    } else {
        format!("@{clean_owner}")
    };

    let clean_author = pr_author.trim().trim_start_matches('@');
    let author_label = if clean_author.is_empty() {
        "@teammate".to_string()
    } else {
        format!("@{clean_author}")
    };

    let title_context = sanitize_in_progress_title(pr_title.trim());
    let focus = infer_pr_focus(pr_title);
    let short = short_sha(sha);

    let mut prompt = String::new();
    prompt.push_str("Write one short GitHub PR in-progress comment.\n");
    prompt.push_str("Voice: conversational teammate, calm, direct, plainspoken.\n");
    prompt.push_str("No emojis. No customer-support phrasing.\n");
    prompt.push_str("Output MUST be a fenced JSON block tagged `pr-review-in-progress-json` with exactly this schema:\n");
    prompt.push_str("{\"comment\": string}\n");
    prompt.push_str("Comment rules:\n");
    prompt.push_str("- One or two sentences only.\n");
    prompt.push_str("- Single line only.\n");
    prompt.push_str("- Keep it under 220 characters.\n");
    prompt.push_str("- Mention commit `");
    prompt.push_str(short);
    prompt.push_str("`.\n");
    prompt.push_str("- Mention that you're reviewing ");
    prompt.push_str(focus);
    prompt.push_str(" in `");
    prompt.push_str(&title_context);
    prompt.push_str("`.\n");
    prompt.push_str("- Address the author as ");
    prompt.push_str(&author_label);
    prompt.push_str(".\n");
    if has_prior_bot_review {
        prompt.push_str("- This is a follow-up pass on the same PR: do NOT re-introduce yourself and do NOT include a tool/repo link.\n");
    } else {
        prompt.push_str("- This is first-touch on this PR: briefly identify as ");
        prompt.push_str(&owner_label);
        prompt.push_str("'s PR-reviewing agent and include [pr-reviewer](");
        prompt.push_str(pr_reviewer_project_url());
        prompt.push_str(").\n");
    }

    prompt
}

fn build_in_progress_comment_fallback(
    owner_login: &str,
    pr_author: &str,
    pr_title: &str,
    sha: &str,
    has_prior_bot_review: bool,
) -> String {
    let clean_owner = owner_login.trim().trim_start_matches('@');
    let owner_label = if clean_owner.is_empty() {
        "the connected GitHub account".to_string()
    } else {
        format!("@{clean_owner}")
    };

    let clean_author = pr_author.trim().trim_start_matches('@');
    let greeting = if clean_author.is_empty() {
        "Hi there".to_string()
    } else {
        format!("Hi @{clean_author}")
    };

    let clean_title = pr_title.trim();
    let title_context = if clean_title.is_empty() {
        "this PR".to_string()
    } else {
        format!("`{}`", sanitize_in_progress_title(clean_title))
    };

    if has_prior_bot_review {
        format!(
            "{} - quick follow-up pass on {} (commit `{}`); taking another look at {} and I will report back shortly.",
            greeting,
            title_context,
            short_sha(sha),
            infer_pr_focus(pr_title),
        )
    } else {
        format!(
            "{} - I'm {}'s PR-reviewing agent powered by [pr-reviewer]({}). I'm taking a look at {} in {} (commit `{}`) now and I'll follow up shortly with feedback.",
            greeting,
            owner_label,
            pr_reviewer_project_url(),
            infer_pr_focus(pr_title),
            title_context,
            short_sha(sha),
        )
    }
}

fn extract_in_progress_comment(stdout: &str, stderr: &str) -> Option<String> {
    let raw = preferred_harness_output(stdout, stderr);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(marked) = extract_marked_json(trimmed, "pr-review-in-progress-json") {
        if let Ok(parsed) = serde_json::from_str::<InProgressCommentPayload>(&marked) {
            return Some(parsed.comment);
        }
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(comment) = find_comment_text(&value) {
            return Some(comment);
        }
    }

    let mut candidates = extract_json_objects(trimmed);
    candidates.reverse();
    for candidate in candidates {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if let Some(comment) = find_comment_text(&value) {
                return Some(comment);
            }
        }
    }

    if looks_like_harness_error_output(trimmed) {
        return None;
    }

    Some(trimmed.to_string())
}

fn preferred_harness_output(stdout: &str, stderr: &str) -> String {
    if !stdout.trim().is_empty() {
        return stdout.trim().to_string();
    }
    stderr.trim().to_string()
}

fn normalize_in_progress_comment(raw: &str) -> Option<String> {
    let collapsed = raw
        .trim()
        .trim_matches('"')
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if collapsed.is_empty() {
        return None;
    }

    if collapsed.len() > IN_PROGRESS_COMMENT_MAX_CHARS {
        return None;
    }

    Some(collapsed)
}

fn looks_like_reintroduction(comment: &str) -> bool {
    let lower = comment.to_ascii_lowercase();
    lower.contains("pr-reviewing agent")
        || lower.contains("powered by [pr-reviewer]")
        || lower.contains("powered by pr-reviewer")
        || lower.contains("[pr-reviewer](")
}

fn looks_like_harness_error_output(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("error:")
        || lower.contains("failed")
        || lower.contains("traceback")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("usage:")
        || lower.contains("command not found")
}

fn login_matches_bot(
    login: &str,
    configured_bot_name: &str,
    authenticated_user: Option<&str>,
) -> bool {
    login.eq_ignore_ascii_case(configured_bot_name)
        || authenticated_user.is_some_and(|user| login.eq_ignore_ascii_case(user))
}

fn extract_marked_json(text: &str, marker_name: &str) -> Option<String> {
    let marker = format!("```{marker_name}");
    let start = text.find(&marker)?;
    let after = &text[start + marker.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("```")?;
    Some(after[..end].trim().to_string())
}

fn extract_json_objects(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut start_idx: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start_idx = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start_idx.take() {
                        results.push(text[start..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    results
}

fn find_comment_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Object(map) => {
            for key in ["comment", "status_comment", "message", "text", "content"] {
                if let Some(text) = map.get(key).and_then(|v| v.as_str()) {
                    if !text.trim().is_empty() {
                        return Some(text.to_string());
                    }
                }
            }
            for nested in map.values() {
                if let Some(text) = find_comment_text(nested) {
                    return Some(text);
                }
            }
            None
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(text) = find_comment_text(item) {
                    return Some(text);
                }
            }
            None
        }
        _ => None,
    }
}

fn sanitize_in_progress_title(title: &str) -> String {
    let sanitized = title
        .replace(['\r', '\n'], " ")
        .replace('"', "'")
        .replace('`', "'");
    sanitized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn pr_reviewer_project_url() -> &'static str {
    resolve_project_url(env!("CARGO_PKG_REPOSITORY"))
}

fn resolve_project_url(configured: &'static str) -> &'static str {
    let trimmed = configured.trim();
    if trimmed.is_empty() {
        "https://github.com/NicholaiVogel/pr-reviewer"
    } else {
        trimmed
    }
}

fn infer_pr_focus(pr_title: &str) -> &'static str {
    let tokens = title_tokens(pr_title);

    let is_fix = has_focus_keyword(
        &tokens,
        &[
            "fix",
            "fixes",
            "fixed",
            "fixing",
            "bug",
            "bugs",
            "hotfix",
            "regression",
            "issue",
            "issues",
            "patch",
            "patches",
        ],
    );
    if is_fix {
        return "the fixes";
    }

    let is_feature = has_focus_keyword(
        &tokens,
        &[
            "feat",
            "feature",
            "features",
            "implement",
            "implements",
            "implemented",
            "introduce",
            "introduces",
            "introduced",
            "add",
            "adds",
            "added",
            "support",
            "supports",
            "supported",
        ],
    );
    if is_feature {
        return "the feature work";
    }

    let is_docs = has_focus_keyword(
        &tokens,
        &[
            "docs",
            "doc",
            "readme",
            "guide",
            "guides",
            "comment",
            "comments",
            "documentation",
        ],
    );
    if is_docs {
        return "the documentation updates";
    }

    let is_refactor = has_focus_keyword(
        &tokens,
        &[
            "refactor",
            "refactors",
            "cleanup",
            "cleanups",
            "chore",
            "chores",
            "rename",
            "renames",
            "simplify",
            "simplifies",
            "simplified",
        ],
    );
    if is_refactor {
        return "the refactor and cleanup work";
    }

    "the changes"
}

fn title_tokens(pr_title: &str) -> Vec<String> {
    pr_title
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn has_focus_keyword(tokens: &[String], keywords: &[&str]) -> bool {
    tokens
        .iter()
        .any(|token| keywords.iter().any(|keyword| token == keyword))
}

fn confidence_verdict_label(
    verdict: ReviewVerdict,
    confidence: Option<&ConfidenceRatings>,
) -> &'static str {
    // If confidence is low (average < 5), downgrade approve to comment
    if let Some(conf) = confidence {
        if verdict == ReviewVerdict::Approve && conf.average() < 5.0 {
            return "COMMENT";
        }
    }
    verdict.as_github_event()
}

fn format_confidence_markdown(conf: &ConfidenceRatings) -> String {
    let mut out = String::new();
    out.push_str(&format!("### Confidence: {:.1}/10\n", conf.average()));
    out.push_str(&format!(
        "- Style consistency & maintainability: {}\n",
        conf.style_maintainability
    ));
    out.push_str(&format!(
        "- Repository conventions adherence: {}\n",
        conf.repo_convention_adherence
    ));
    out.push_str(&format!(
        "- Merge conflict detection confidence: {}\n",
        conf.merge_conflict_detection
    ));
    out.push_str(&format!(
        "- Security vulnerability detection confidence: {}\n",
        conf.security_vulnerability_detection
    ));
    out.push_str(&format!(
        "- Injection risk detection confidence: {}\n",
        conf.injection_risk_detection
    ));
    out.push_str(&format!(
        "- Attack-surface risk assessment confidence: {}\n",
        conf.attack_surface_risk_assessment
    ));
    out.push_str(&format!(
        "- Future hardening guidance confidence: {}\n",
        conf.future_hardening_guidance
    ));
    out.push_str(&format!(
        "- Scope alignment confidence: {}\n",
        conf.scope_alignment
    ));
    out.push_str(&format!(
        "- Existing functionality awareness: {}\n",
        conf.duplication_awareness
    ));
    out.push_str(&format!(
        "- Existing tooling/pattern leverage: {}\n",
        conf.tooling_pattern_leverage
    ));
    out.push_str(&format!(
        "- Functional completeness confidence: {}\n",
        conf.functional_completeness
    ));
    out.push_str(&format!(
        "- Pattern correctness confidence: {}\n",
        conf.pattern_correctness
    ));
    out.push_str(&format!(
        "- Documentation coverage confidence: {}\n",
        conf.documentation_coverage
    ));
    out
}

fn format_confidence_json_block(conf: &ConfidenceRatings) -> String {
    let json = serde_json::to_string_pretty(conf).unwrap_or_else(|_| "{}".to_string());
    format!("```pr-review-confidence-json\n{}\n```", json)
}

fn extract_latest_confidence_snapshot(
    bot_name: &str,
    reviews: &[PullRequestReview],
    comments: &[ReviewComment],
) -> Option<ConfidenceRatings> {
    let mut entries: Vec<(String, String)> = Vec::new();

    for review in reviews {
        if !review.user.login.eq_ignore_ascii_case(bot_name) {
            continue;
        }
        let ts = review
            .submitted_at
            .clone()
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
        if let Some(body) = review.body.as_deref() {
            entries.push((ts, body.to_string()));
        }
    }

    for comment in comments {
        if !comment.user.login.eq_ignore_ascii_case(bot_name) {
            continue;
        }
        entries.push((comment.created_at.clone(), comment.body.clone()));
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries.reverse();

    for (_, text) in entries {
        if let Some(conf) = extract_confidence_from_text(&text) {
            return Some(conf);
        }
    }

    None
}

fn extract_confidence_from_text(text: &str) -> Option<ConfidenceRatings> {
    let marker = "```pr-review-confidence-json";
    let start = text.find(marker)?;
    let after = &text[start + marker.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("```")?;
    serde_json::from_str::<ConfidenceRatings>(after[..end].trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        build_in_progress_comment_fallback, extract_in_progress_comment, infer_pr_focus,
        login_matches_bot, looks_like_harness_error_output, looks_like_reintroduction,
        normalize_in_progress_comment, pr_reviewer_project_url, resolve_project_url,
    };

    #[test]
    fn first_touch_comment_mentions_owner_and_repo_link() {
        let message = build_in_progress_comment_fallback(
            "octocat",
            "contributor",
            "fix race condition in queue",
            "527fae59abcde",
            false,
        );

        assert!(message.contains("@octocat's PR-reviewing agent"));
        assert!(message.contains("Hi @contributor"));
        assert!(message.contains(&format!("[pr-reviewer]({})", pr_reviewer_project_url())));
        assert!(message.contains("commit `527fae59`"));
    }

    #[test]
    fn follow_up_comment_is_conversational_without_reintro() {
        let message = build_in_progress_comment_fallback(
            "octocat",
            "contributor",
            "fix race condition in queue",
            "527fae59abcde",
            true,
        );

        assert!(message.contains("quick follow-up pass"));
        assert!(!message.contains("PR-reviewing agent"));
        assert!(!message.contains("[pr-reviewer]("));
    }

    #[test]
    fn fallback_comment_sanitizes_double_quotes_in_title() {
        let message = build_in_progress_comment_fallback(
            "octocat",
            "contributor",
            "fix \"the\" bug",
            "527fae59",
            false,
        );
        assert!(message.contains("`fix 'the' bug`"));
    }

    #[test]
    fn fallback_comment_sanitizes_backticks_in_title() {
        let message = build_in_progress_comment_fallback(
            "octocat",
            "contributor",
            "fix `foo` crash",
            "527fae59",
            false,
        );
        assert!(message.contains("`fix 'foo' crash`"));
    }

    #[test]
    fn fallback_comment_neutralizes_markdown_links_in_title() {
        let message = build_in_progress_comment_fallback(
            "octocat",
            "contributor",
            "docs [click](https://example.com)",
            "527fae59",
            false,
        );
        assert!(message.contains("`docs [click](https://example.com)`"));
    }

    #[test]
    fn fallback_comment_normalizes_newlines_in_title() {
        let message = build_in_progress_comment_fallback(
            "octocat",
            "contributor",
            "fix quote\nand link\r\nrendering",
            "527fae59",
            false,
        );
        assert!(message.contains("`fix quote and link rendering`"));
        assert!(!message.contains('\n'));
        assert!(!message.contains('\r'));
    }

    #[test]
    fn parses_marked_json_in_progress_comment() {
        let output = "```pr-review-in-progress-json\n{\"comment\":\"Hi @you - quick pass on commit `abc12345`.\"}\n```";
        let parsed = extract_in_progress_comment(output, "").expect("parsed comment");
        assert_eq!(parsed, "Hi @you - quick pass on commit `abc12345`.");
    }

    #[test]
    fn normalizes_generated_comment_whitespace() {
        let normalized = normalize_in_progress_comment("  hi there \n  doing a pass  ").unwrap();
        assert_eq!(normalized, "hi there doing a pass");
    }

    #[test]
    fn reintroduction_detector_flags_tool_intro() {
        assert!(looks_like_reintroduction(
            "I'm @octocat's PR-reviewing agent powered by [pr-reviewer](https://example.com)."
        ));
    }

    #[test]
    fn harness_error_output_is_rejected() {
        assert!(looks_like_harness_error_output(
            "Error: command failed with exit status 1"
        ));
    }

    #[test]
    fn login_match_checks_configured_and_authenticated_names() {
        assert!(login_matches_bot("pr-reviewer", "pr-reviewer", Some("NicholaiVogel")));
        assert!(login_matches_bot("NicholaiVogel", "pr-reviewer", Some("NicholaiVogel")));
        assert!(!login_matches_bot("someone-else", "pr-reviewer", Some("NicholaiVogel")));
    }

    #[test]
    fn project_url_uses_fallback_when_empty() {
        assert_eq!(
            resolve_project_url(""),
            "https://github.com/NicholaiVogel/pr-reviewer"
        );
    }

    #[test]
    fn infer_focus_uses_title_keywords() {
        assert_eq!(infer_pr_focus("fix flaky test"), "the fixes");
        assert_eq!(
            infer_pr_focus("feat: add metrics dashboard"),
            "the feature work"
        );
        assert_eq!(
            infer_pr_focus("docs: update README"),
            "the documentation updates"
        );
        assert_eq!(
            infer_pr_focus("refactor parser internals"),
            "the refactor and cleanup work"
        );
        assert_eq!(infer_pr_focus("misc updates"), "the changes");
    }

    #[test]
    fn infer_focus_avoids_substring_false_positives() {
        assert_eq!(infer_pr_focus("address logging"), "the changes");
        assert_eq!(infer_pr_focus("dismissal policy"), "the changes");
    }
}

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tempfile::tempdir;

use crate::config::{AppConfig, HarnessKind, ReasoningEffort, RepoConfig};
use crate::context::diff_parser::{parse_unified_diff, DiffSide};
use crate::context::gitnexus;
use crate::context::retriever::{assemble_context, ContextMode};
use crate::github::comments;
use crate::github::types::{
    CreateReviewComment, CreateReviewRequest, IssueComment, PullRequest, PullRequestReview,
    ReviewComment,
};
use crate::github::{client::GitHubClient, pr};
use crate::harness;
use crate::harness::spawn::{run_harness, HarnessRunRequest};
use crate::review::parser::{
    parse_reply_output, parse_review_output, CommentSeverity, ParseOutcome, ReplyParseOutcome,
    ReviewComment as ParsedReviewComment, ReviewVerdict,
};
use crate::review::prompt::build_review_prompt;
use crate::safety::{evaluate_fork_policy, ForkDecision};
use crate::store::db::{Database, ReviewAttemptRecord, ReviewClaim, dedupe_key};

const IN_PROGRESS_COMMENT_TIMEOUT_SECS: u64 = 4;
const IN_PROGRESS_COMMENT_MAX_CHARS: usize = 220;
const GITHUB_COMMENT_MAX_CHARS: usize = 65_536;
const GITHUB_COMMENT_TRUNCATION_NOTE: &str =
    "\n\n[truncated by pr-reviewer to fit GitHub's comment length limit]";
const SCREENSHOT_NOTE_TEXT: &str =
    "> **Note:** This PR touches UI files but no screenshots were referenced in the description. Consider adding visual previews for reviewers.";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum FindingStatus {
    Open,
    LikelyAddressed,
    DismissedByHuman,
    RejectedWithRationale,
    OutOfScopeForPr,
}

#[derive(Debug, Clone)]
struct FindingRecord {
    key: String,
    path: String,
    line: Option<u32>,
    body: String,
    token_set: HashSet<String>,
    status: FindingStatus,
}

#[derive(Debug, Clone, Default)]
struct ReviewMemory {
    findings: Vec<FindingRecord>,
    already_noted_screenshot: bool,
    has_prior_bot_review: bool,
    explicit_boundaries: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ReviewOptions {
    pub dry_run: bool,
    pub harness: Option<HarnessKind>,
    pub model: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub force: bool,
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

    pub fn authenticated_user(&self) -> Option<&str> {
        self.authenticated_user.as_deref()
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
        let dry_run = options.dry_run || self.config.defaults.dry_run;
        let reasoning_effort = options
            .reasoning_effort
            .or(repo_cfg.reasoning_effort)
            .or(self.config.harness.reasoning_effort);
        let dedupe = dedupe_key(
            &repo_name,
            pr_data.number,
            &pr_data.head.sha,
            harness_kind.as_str(),
            dry_run,
        );

        let claim = ReviewClaim {
            dedupe_key: dedupe.clone(),
            repo: repo_name.clone(),
            pr_number: pr_data.number as i64,
            sha: pr_data.head.sha.clone(),
            harness: harness_kind.as_str().to_string(),
            model: Some(model.clone()),
        };

        if options.force {
            match self.db.delete_review_claim(&dedupe).await? {
                Some(ref status) if status == "claimed" => {
                    tracing::warn!(
                        dedupe_key = %dedupe,
                        "force flag deleted an in-flight 'claimed' entry — if the daemon is \
                         mid-pipeline for this PR, its complete_review/fail_review will silently \
                         no-op, leaving the new claim stuck until the next stale sweep"
                    );
                }
                Some(_) => {
                    tracing::info!(dedupe_key = %dedupe, "force flag cleared existing failed claim");
                }
                None => {
                    tracing::warn!(
                        dedupe_key = %dedupe,
                        "--force had no effect: no stale or failed claim found; \
                         if this PR+SHA was already successfully reviewed the completed \
                         entry is preserved and cannot be overridden with --force"
                    );
                }
            }
        }

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
                let has_prior_bot_review = existing_reviews.iter().any(|r| {
                    login_matches_bot(
                        &r.user.login,
                        self.config.defaults.bot_name.as_str(),
                        Some(reviewer_owner.as_str()),
                    )
                });

                // Only post in-progress comment on first touch. Follow-up rounds
                // skip it to reduce noise.
                if !has_prior_bot_review {
                    let in_progress = self
                        .compose_in_progress_comment(
                            harness_kind,
                            &model,
                            reasoning_effort,
                            &pr_data.user.login,
                            &pr_data.title,
                            &pr_data.head.sha,
                            false,
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
        }

        let started = Instant::now();
        let outcome = self
            .run_review_pipeline(
                repo_cfg,
                pr_data,
                harness_kind,
                &model,
                reasoning_effort,
                dry_run,
                options.force,
                existing_reviews,
            )
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
        reasoning_effort: Option<ReasoningEffort>,
        pr_author: &str,
        pr_title: &str,
        sha: &str,
        has_prior_bot_review: bool,
    ) -> String {
        let fallback =
            build_in_progress_comment_fallback(pr_author, pr_title, sha, has_prior_bot_review);

        let prompt =
            build_in_progress_comment_prompt(pr_author, pr_title, sha, has_prior_bot_review);

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
                reasoning_effort,
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
        harness_kind: HarnessKind,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        dry_run: bool,
        force: bool,
        existing_reviews: Vec<PullRequestReview>,
    ) -> Result<ReviewRunResult> {
        let repo_name = repo_cfg.full_name();
        let dedupe = dedupe_key(
            &repo_name,
            pr_data.number,
            &pr_data.head.sha,
            harness_kind.as_str(),
            dry_run,
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

        // Skip docs-only or trivial diffs to avoid wasting compute
        if let Some(skip_reason) = should_skip_review(&changed_files, &self.config.defaults) {
            tracing::info!(
                repo = %repo_name,
                pr = pr_data.number,
                reason = skip_reason,
                "skipping review: {skip_reason}",
            );
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(&format!("skipped:{skip_reason}")),
                    0.0,
                    0,
                    parsed_diff.total_hunk_lines as i64,
                    None,
                    None,
                    None,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: format!("skipped:{skip_reason}"),
                verdict: None,
                comments_posted: 0,
            });
        }

        let mut gitnexus_used = if repo_cfg.gitnexus { Some(false) } else { None };
        let mut gitnexus_latency_ms: Option<i64> = None;
        let mut gitnexus_hit_count: Option<i64> = None;

        let gitnexus_context = if repo_cfg.gitnexus {
            match repo_cfg.effective_local_path() {
                Ok(local) => {
                    // Fetch latest for managed clones so GitNexus index is fresh
                    if repo_cfg.is_managed() {
                        if let Err(err) =
                            crate::repo_manager::fetch_latest(&local, self.github.token()).await
                        {
                            tracing::warn!(
                                repo = %repo_cfg.full_name(),
                                error = %err,
                                "failed to fetch latest for managed clone; using stale state"
                            );
                        }
                    }
                    match gitnexus::is_index_stale(&local).await {
                        Ok(Some(true)) => {
                            tracing::warn!(
                                repo = %repo_cfg.full_name(),
                                "gitnexus index is stale; run `pr-reviewer index {}` to refresh",
                                repo_cfg.full_name()
                            );
                        }
                        Ok(_) => {}
                        Err(err) => {
                            tracing::debug!(
                                repo = %repo_cfg.full_name(),
                                error = %err,
                                "failed to determine gitnexus index freshness"
                            );
                        }
                    }

                    match gitnexus::query_context_with_metrics(
                        &local,
                        repo_cfg.name.as_str(),
                        &changed_files,
                    )
                    .await
                    {
                        Ok(result) => {
                            gitnexus_used = Some(result.used);
                            if result.used {
                                gitnexus_latency_ms = Some(result.latency_ms);
                                gitnexus_hit_count = Some(result.hit_count);
                            }
                            result.text
                        }
                        Err(_) => None,
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

        // Fetch inline review comments, PR issue comments, and repo conventions in parallel
        let (review_comments, issue_comments, repo_conventions) = tokio::join!(
            async {
                comments::get_review_comments(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                    None,
                )
                .await
                .unwrap_or_default()
            },
            async {
                comments::get_issue_comments(
                    &self.github,
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    pr_data.number,
                )
                .await
                .unwrap_or_default()
            },
            crate::context::retriever::fetch_repo_conventions(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                &pr_data.head.sha,
            ),
        );

        let mut context_with_history = assembled.text.clone();
        let review_memory = build_review_memory(
            &existing_reviews,
            &review_comments,
            &issue_comments,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
            pr_data.head.sha.as_str(),
            Some(&changed_files),
        );
        let prior_reviews_context = build_prior_reviews_context(
            &existing_reviews,
            &review_comments,
            &issue_comments,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
            pr_data.head.sha.as_str(),
        );
        if !prior_reviews_context.is_empty() {
            context_with_history.push_str("\n## Prior Review History\n");
            context_with_history.push_str(&prior_reviews_context);
            context_with_history.push('\n');
        }

        let addressed_findings = build_addressed_findings(
            &review_comments,
            &parsed_diff,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
        );
        if !addressed_findings.is_empty() {
            context_with_history.push_str("\n## Previously Flagged Items\n");
            context_with_history.push_str(&addressed_findings);
            context_with_history.push('\n');
        }
        let review_state_context = build_review_state_context(&review_memory);
        if !review_state_context.is_empty() {
            context_with_history.push_str("\n## Review State\n");
            context_with_history.push_str(&review_state_context);
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

        let context_original_bytes = context_with_history.len();
        if context_with_history.len() > self.config.defaults.max_prompt_bytes {
            let max_bytes = self.config.defaults.max_prompt_bytes;
            let note = "\n\n[context truncated before prompt build due size budget]\n";
            if max_bytes > note.len() {
                truncate_utf8_to_max_bytes(&mut context_with_history, max_bytes - note.len());
                context_with_history.push_str(note);
            } else {
                truncate_utf8_to_max_bytes(&mut context_with_history, max_bytes);
            }
        }

        let has_prior_reviews =
            review_memory.has_prior_bot_review || !prior_reviews_context.is_empty();

        // Detect UI file changes and check for screenshots in PR body
        let ui_files = detect_ui_files(&changed_files);
        let ui_files_for_prompt =
            if !ui_files.is_empty() && !has_screenshot_references(pr_data.body.as_deref()) {
                Some(ui_files)
            } else {
                None
            };

        let prompt = build_review_prompt(
            repo_cfg,
            pr_data,
            &context_with_history,
            &self.config.defaults.bot_name,
            repo_conventions.as_deref(),
            has_prior_reviews,
            ui_files_for_prompt.as_deref(),
        );

        tracing::info!(
            repo = %repo_cfg.full_name(),
            pr = pr_data.number,
            context_bytes_before = context_original_bytes,
            context_bytes_after = context_with_history.len(),
            prompt_bytes = prompt.len(),
            changed_files_included = assembled.files_included,
            related_files_included = assembled.related_files_included,
            assembled_context_bytes = assembled.bytes_total,
            assembled_context_truncated = assembled.truncated,
            "built review prompt"
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
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let parse_outcome = parse_review_output(&harness_output.stdout, &harness_output.stderr)?;

        let mut body = String::new();
        let mut verdict = ReviewVerdict::Comment;
        let mut inline_comments: Vec<CreateReviewComment> = Vec::new();
        let mut unmapped_comments: Vec<String> = Vec::new();
        let has_structured_findings: bool;

        // Prepend bot identity header on every review
        body.push_str(&format!(
            "> Automated review by [pr-reviewer]({}) | model: {} | commit: `{}`\n\n",
            pr_reviewer_project_url(),
            model,
            short_sha(&pr_data.head.sha),
        ));

        match parse_outcome {
            ParseOutcome::Structured(review) => {
                body.push_str(&review.summary);
                verdict = review.verdict;
                let confidence_reasons = review
                    .confidence
                    .reasons
                    .iter()
                    .map(|reason| reason.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!(
                    "\n\n**Confidence:** {} [{}] - {}\n",
                    review.confidence.level, confidence_reasons, review.confidence.justification,
                ));

                if review.ui_screenshot_needed && !review_memory.already_noted_screenshot {
                    body.push('\n');
                    body.push_str(SCREENSHOT_NOTE_TEXT);
                    body.push('\n');
                }

                let mut seen_keys = HashSet::new();
                let mut surviving_comments: Vec<ParsedReviewComment> = Vec::new();
                for mut comment in review.comments {
                    let path = clean_comment_path(&comment.file);
                    let key = finding_key(&path, Some(comment.line), &comment.body);
                    if !seen_keys.insert(key) {
                        continue;
                    }
                    if should_suppress_finding(&comment, &path, &review_memory) {
                        continue;
                    }
                    if comment.severity == CommentSeverity::Blocking
                        && !blocker_has_sufficient_evidence(
                            &comment,
                            &path,
                            review.confidence.level,
                        )
                    {
                        comment.severity = CommentSeverity::Warning;
                    }
                    surviving_comments.push(comment);
                }

                if verdict == ReviewVerdict::RequestChanges
                    && !surviving_comments
                        .iter()
                        .any(|comment| comment.severity == CommentSeverity::Blocking)
                {
                    verdict = ReviewVerdict::Comment;
                }

                for comment in surviving_comments {
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
                has_structured_findings =
                    !inline_comments.is_empty() || !unmapped_comments.is_empty();
            }
            ParseOutcome::RawSummary(raw) => {
                if looks_like_harness_transport_output(&raw) {
                    return Err(anyhow!(
                        "harness returned machine transport output instead of a review body"
                    ));
                }
                has_structured_findings = true;
                body.push_str("Harness output could not be parsed as structured JSON.\n\n");
                body.push_str(&raw);
            }
            ParseOutcome::Empty => {
                return Err(anyhow!("harness returned empty output"));
            }
        }

        if !unmapped_comments.is_empty() {
            body.push_str("\n\nUnmapped findings (not on changed lines):\n");
            for line in &unmapped_comments {
                body.push_str(&line);
                body.push('\n');
            }
        }

        let body = truncate_github_comment_body(&body);

        if dry_run {
            self.db
                .complete_review(
                    &dedupe,
                    inline_comments.len() as i64,
                    Some(verdict_label(verdict)),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count,
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

        let screenshot_note_added =
            body.contains(SCREENSHOT_NOTE_TEXT) && !review_memory.already_noted_screenshot;
        if review_memory.has_prior_bot_review
            && inline_comments.is_empty()
            && unmapped_comments.is_empty()
            && !screenshot_note_added
            && !has_structured_findings
        {
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(verdict_label(ReviewVerdict::Comment)),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count,
                )
                .await?;

            return Ok(ReviewRunResult {
                repo: repo_name,
                pr_number: pr_data.number,
                sha: pr_data.head.sha.clone(),
                status: "skipped:no-novel-findings".to_string(),
                verdict: Some(ReviewVerdict::Comment),
                comments_posted: 0,
            });
        }

        if !force
            && existing_reviews.iter().any(|r| {
                r.commit_id.as_deref() == Some(pr_data.head.sha.as_str())
                    && login_matches_bot(
                        &r.user.login,
                        self.config.defaults.bot_name.as_str(),
                        self.authenticated_user.as_deref(),
                    )
            })
        {
            self.db
                .complete_review(
                    &dedupe,
                    0,
                    Some(verdict_label(ReviewVerdict::Comment)),
                    harness_output.duration_secs,
                    assembled.files_included as i64,
                    assembled.diff_lines as i64,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count,
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

        // Bot never approves. REQUEST_CHANGES still needs self-review downgrade
        // because GitHub rejects it on your own PRs.
        let event = match verdict {
            ReviewVerdict::NoIssues | ReviewVerdict::Approve | ReviewVerdict::Comment => "COMMENT",
            ReviewVerdict::RequestChanges => {
                let bot_login = match &self.authenticated_user {
                    Some(u) => Some(u.clone()),
                    None => self.github.get_authenticated_user().await.ok(),
                };
                match bot_login.as_deref() {
                    Some(u) if u.eq_ignore_ascii_case(&pr_data.user.login) => "COMMENT",
                    Some(_) => "REQUEST_CHANGES",
                    None => {
                        tracing::warn!("could not determine authenticated user; downgrading REQUEST_CHANGES to COMMENT");
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
                let retry_request = CreateReviewRequest {
                    body: build_retry_review_body(&body, &inline_comments),
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
            let fallback = truncate_github_comment_body(&format!(
                "Review post failed; fallback summary posted.\n\nError: {err}\n\n{body}"
            ));
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
        let posted_inline = if used_retry {
            0
        } else {
            inline_comments.len() as i64
        };
        let actual_event = if used_retry { "COMMENT" } else { event };
        self.db
            .complete_review(
                &dedupe,
                posted_inline,
                Some(actual_event),
                harness_output.duration_secs,
                assembled.files_included as i64,
                assembled.diff_lines as i64,
                gitnexus_used,
                gitnexus_latency_ms,
                gitnexus_hit_count,
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

    pub async fn finalize_closed_pr_review(
        &self,
        repo_cfg: &RepoConfig,
        pr_number: u64,
    ) -> Result<bool> {
        let pr_data = pr::get_pull_request(&self.github, &repo_cfg.owner, &repo_cfg.name, pr_number)
            .await
            .with_context(|| {
                format!("failed fetching PR {}#{pr_number} for final archive", repo_cfg.full_name())
            })?;

        if pr_data.state.eq_ignore_ascii_case("open") {
            return Ok(false);
        }

        self.archive_closed_pr_review(repo_cfg, &pr_data).await?;
        Ok(true)
    }

    async fn archive_closed_pr_review(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
    ) -> Result<()> {
        let repo_name = repo_cfg.full_name();
        let terminal_state = terminal_pr_state(pr_data);
        let review_attempts = self
            .db
            .list_pr_review_attempts(&repo_name, pr_data.number as i64)
            .await?;

        let (existing_reviews, review_comments, issue_comments) = tokio::join!(
            comments::get_existing_reviews(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
            ),
            comments::get_review_comments(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
                None,
            ),
            comments::get_issue_comments(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
            ),
        );

        // Log warnings for API failures so the transcript honestly reflects
        // missing data instead of silently treating errors as "no content".
        let existing_reviews = match existing_reviews {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch reviews for final archive; transcript will be incomplete"
                );
                vec![]
            }
        };
        let review_comments = match review_comments {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch review comments for final archive; transcript will be incomplete"
                );
                vec![]
            }
        };
        let issue_comments = match issue_comments {
            Ok(v) => v,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to fetch issue comments for final archive; transcript will be incomplete"
                );
                vec![]
            }
        };

        let transcript = build_final_review_transcript(
            &repo_name,
            pr_data,
            terminal_state,
            &review_attempts,
            &existing_reviews,
            &review_comments,
            &issue_comments,
        );

        let summary = match self
            .compose_final_review_summary(repo_cfg, pr_data, terminal_state, &transcript)
            .await
        {
            Ok(summary) => summary,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_name,
                    pr = pr_data.number,
                    error = %err,
                    "failed to generate final review summary; using fallback"
                );
                build_final_review_summary_fallback(pr_data, terminal_state, &review_attempts)
            }
        };

        self.db
            .upsert_review_archive(
                &repo_name,
                pr_data.number as i64,
                &pr_data.head.sha,
                terminal_state,
                pr_data.closed_at.as_deref().or(pr_data.merged_at.as_deref()),
                &transcript,
                &summary,
            )
            .await?;

        tracing::info!(
            repo = %repo_name,
            pr = pr_data.number,
            terminal_state,
            "saved final review transcript and summary"
        );
        Ok(())
    }

    async fn compose_final_review_summary(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        terminal_state: &str,
        transcript: &str,
    ) -> Result<String> {
        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let model = repo_cfg.resolved_model(&self.config).to_string();
        let reasoning_effort = repo_cfg.resolved_reasoning_effort(&self.config);
        let mut prompt = build_final_review_summary_prompt(
            &repo_cfg.full_name(),
            pr_data,
            terminal_state,
            transcript,
        );
        let max_bytes = self.config.defaults.max_prompt_bytes;
        let note = "\n\n[review transcript truncated before final-summary generation due size budget]\n";
        if prompt.len() > max_bytes {
            if max_bytes > note.len() {
                truncate_utf8_to_max_bytes(&mut prompt, max_bytes - note.len());
                prompt.push_str(note);
            } else {
                truncate_utf8_to_max_bytes(&mut prompt, max_bytes);
            }
        }

        let temp = tempdir().context("failed to create temp dir for final review summary")?;
        let harness_impl = harness::for_kind(harness_kind);
        let output = run_harness(
            harness_impl.as_ref(),
            HarnessRunRequest {
                prompt,
                model,
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let summary = normalize_summary_output(&output.stdout, &output.stderr)
            .ok_or_else(|| anyhow!("final review summary harness returned empty output"))?;
        Ok(summary)
    }

    pub async fn reply_to_comment(
        &self,
        repo_cfg: &RepoConfig,
        pr_data: &PullRequest,
        comment: &ReviewComment,
    ) -> Result<()> {
        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let model = repo_cfg.resolved_model(&self.config).to_string();
        let reasoning_effort = repo_cfg.resolved_reasoning_effort(&self.config);

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
        let thread_history = build_thread_history(&all_comments, comment.id);
        let issue_comments = comments::get_issue_comments(
            &self.github,
            &repo_cfg.owner,
            &repo_cfg.name,
            pr_data.number,
        )
        .await
        .unwrap_or_default();
        let review_memory = build_review_memory(
            &[],
            &all_comments,
            &issue_comments,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
            pr_data.head.sha.as_str(),
            None,
        );

        if let Some(reply) = build_direct_pushback_reply(
            &thread_history,
            self.config.defaults.bot_name.as_str(),
            self.authenticated_user.as_deref(),
        ) {
            comments::reply_to_review_comment(
                &self.github,
                &repo_cfg.owner,
                &repo_cfg.name,
                pr_data.number,
                comment.id,
                &reply,
            )
            .await?;
            return Ok(());
        }

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
        prompt.push_str("Respond to this review thread comment.\n");
        prompt.push_str("First classify the thread state as one of: fixed, not_fixed, intentional_or_accepted, out_of_scope_for_this_pr, needs_human_judgment.\n");
        prompt.push_str("If a human explicitly says the concern is intentional, acceptable, or out of scope for this PR, acknowledge that and stop pressing the same line in this thread. If fixed, say so explicitly and reference concrete evidence. If not fixed, explain what remains without reopening adjacent design debates.\n");
        prompt.push_str("Output JSON in a fenced block tagged exactly `pr-review-reply-json` with this schema:\n");
        prompt.push_str("{\"reply\": string}\n\n");
        prompt.push_str(&format!("Repo: {}/{}\n", repo_cfg.owner, repo_cfg.name));
        prompt.push_str(&format!("PR: #{}\n", pr_data.number));
        prompt.push_str(&format!("Target comment id: {}\n", comment.id));
        prompt.push_str(&format!("Target comment path: {}\n", comment.path));
        prompt.push_str(&format!("Target comment line: {:?}\n", comment.line));
        prompt.push_str("\nThread history:\n");
        prompt.push_str(&thread_history);
        prompt.push_str("\n\nLatest diff (truncated):\n```diff\n");
        prompt.push_str(&truncate_lines(&latest_diff, 300));
        prompt.push_str("\n```\n");

        let review_state_context = build_review_state_context(&review_memory);
        if !review_state_context.is_empty() {
            prompt.push_str("\nReview state:\n");
            prompt.push_str(&review_state_context);
            prompt.push('\n');
        }

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
                reasoning_effort,
                working_dir: temp.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let reply = match parse_reply_output(&output.stdout, &output.stderr)? {
            ReplyParseOutcome::Structured(update) => update.reply,
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

/// Dismissal signals from humans indicating a finding should not be re-flagged.
const DISMISSAL_SIGNALS: &[&str] = &[
    "intentional",
    "by design",
    "won't fix",
    "wontfix",
    "acknowledged",
    "expected",
    "not a bug",
    "fine",
    "this is fine",
    "leave it",
    "acceptable",
    "known",
    "on purpose",
];

const OUT_OF_SCOPE_SIGNALS: &[&str] = &[
    "out of scope",
    "not in scope",
    "not in this pr",
    "not for this pr",
    "not taking further",
    "not expanding this pr",
    "not reopening",
    "not part of this pr",
];

const RATIONALE_REJECTION_SIGNALS: &[&str] = &[
    "false positive",
    "not correct",
    "you are wrong",
    "this is wrong",
    "that is wrong",
    "misread",
    "misunderstanding",
    "not the issue",
];

fn has_dismissal_signal(text: &str) -> bool {
    let lower = text.to_lowercase();
    // Split on whitespace and punctuation, but keep hyphens attached to words
    // so "fine-grained" stays as one token and doesn't match "fine"
    let words: Vec<&str> = lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    let joined = words.join(" ");
    DISMISSAL_SIGNALS.iter().any(|signal| {
        if signal.contains(' ') {
            joined.contains(signal)
        } else {
            words.iter().any(|w| *w == *signal)
        }
    })
}

fn has_out_of_scope_signal(text: &str) -> bool {
    let lower = normalize_text(text);
    OUT_OF_SCOPE_SIGNALS
        .iter()
        .any(|signal| lower.contains(&normalize_text(signal)))
}

fn has_rationale_rejection_signal(text: &str) -> bool {
    let lower = normalize_text(text);
    RATIONALE_REJECTION_SIGNALS
        .iter()
        .any(|signal| lower.contains(&normalize_text(signal)))
}

fn normalize_text(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn finding_key(path: &str, line: Option<u32>, body: &str) -> String {
    format!(
        "{}|{}|{}",
        path.trim().to_ascii_lowercase(),
        line.map(|v| v.to_string())
            .unwrap_or_else(|| "0".to_string()),
        normalize_text(body)
    )
}

fn token_set(text: &str) -> HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "this", "that", "with", "from", "into", "when", "will", "would", "should", "could", "also",
        "then", "than", "they", "them", "their", "about", "under", "over", "after", "before",
        "because", "which", "while", "where", "there", "here", "have", "has", "had", "does",
        "doesnt", "dont", "isnt", "cant", "only", "just", "being", "through", "still", "later",
        "every", "other", "again", "across", "agent", "scope", "scoped", "pr", "review",
        "reviewer", "blocking", "warning", "comment",
    ];

    normalize_text(text)
        .split_whitespace()
        .filter(|token| token.len() >= 4 && !STOPWORDS.contains(token))
        .map(ToString::to_string)
        .collect()
}

fn token_overlap(left: &HashSet<String>, right: &HashSet<String>) -> usize {
    left.intersection(right).count()
}

fn build_review_memory(
    reviews: &[PullRequestReview],
    review_comments: &[ReviewComment],
    issue_comments: &[IssueComment],
    bot_name: &str,
    bot_alias: Option<&str>,
    current_sha: &str,
    changed_files: Option<&[String]>,
) -> ReviewMemory {
    let parent_map: HashMap<u64, Option<u64>> = review_comments
        .iter()
        .map(|c| (c.id, c.in_reply_to_id))
        .collect();
    let mut threads: HashMap<u64, Vec<&ReviewComment>> = HashMap::new();
    for comment in review_comments {
        let root = find_thread_root_id(&parent_map, comment.id);
        threads.entry(root).or_default().push(comment);
    }

    let mut explicit_boundaries = Vec::new();
    for comment in issue_comments {
        if !login_matches_bot(&comment.user.login, bot_name, bot_alias)
            && has_out_of_scope_signal(&comment.body)
        {
            explicit_boundaries.push(comment.body.clone());
        }
    }

    let findings = threads
        .into_values()
        .filter_map(|comments| {
            let mut sorted = comments;
            sorted.sort_by(|a, b| a.created_at.cmp(&b.created_at));
            let finding = sorted
                .iter()
                .find(|c| login_matches_bot(&c.user.login, bot_name, bot_alias))?;

            let human_replies: Vec<&&ReviewComment> = sorted
                .iter()
                .filter(|c| !login_matches_bot(&c.user.login, bot_name, bot_alias))
                .collect();
            if human_replies.is_empty() {
                let mut status = FindingStatus::Open;
                if let Some(changed_files) = changed_files {
                    if !changed_files.iter().any(|p| p == &finding.path) {
                        status = FindingStatus::LikelyAddressed;
                    }
                }
                return Some(FindingRecord {
                    key: finding_key(&finding.path, finding.line, &finding.body),
                    path: finding.path.clone(),
                    line: finding.line,
                    body: finding.body.clone(),
                    token_set: token_set(&finding.body),
                    status,
                });
            }

            let mut status = FindingStatus::Open;

            for reply in &human_replies {
                let reply_status = classify_rebuttal_status(&reply.body);
                if let Some(reply_status) = reply_status {
                    if reply_status == FindingStatus::OutOfScopeForPr {
                        explicit_boundaries.push(reply.body.clone());
                    }
                    if status_precedence(&reply_status) > status_precedence(&status) {
                        status = reply_status;
                    }
                }
            }

            if status == FindingStatus::Open {
                if let Some(changed_files) = changed_files {
                    if !changed_files.iter().any(|p| p == &finding.path) {
                        status = FindingStatus::LikelyAddressed;
                    }
                }
            }

            Some(FindingRecord {
                key: finding_key(&finding.path, finding.line, &finding.body),
                path: finding.path.clone(),
                line: finding.line,
                body: finding.body.clone(),
                token_set: token_set(&finding.body),
                status,
            })
        })
        .collect::<Vec<_>>();

    let already_noted_screenshot = reviews.iter().any(|review| {
        login_matches_bot(&review.user.login, bot_name, bot_alias)
            && review
                .body
                .as_deref()
                .is_some_and(|body| body.contains(SCREENSHOT_NOTE_TEXT))
    }) || issue_comments.iter().any(|comment| {
        login_matches_bot(&comment.user.login, bot_name, bot_alias)
            && comment.body.contains(SCREENSHOT_NOTE_TEXT)
    });

    let has_prior_bot_review = reviews.iter().any(|review| {
        login_matches_bot(&review.user.login, bot_name, bot_alias)
            && review.commit_id.as_deref() != Some(current_sha)
    });

    ReviewMemory {
        findings,
        already_noted_screenshot,
        has_prior_bot_review,
        explicit_boundaries,
    }
}

fn classify_rebuttal_status(text: &str) -> Option<FindingStatus> {
    if has_out_of_scope_signal(text) {
        return Some(FindingStatus::OutOfScopeForPr);
    }

    if has_rationale_rejection_signal(text) {
        return Some(FindingStatus::RejectedWithRationale);
    }

    if has_dismissal_signal(text) {
        return Some(FindingStatus::DismissedByHuman);
    }

    None
}

fn status_precedence(status: &FindingStatus) -> usize {
    match status {
        FindingStatus::Open => 0,
        FindingStatus::LikelyAddressed => 1,
        FindingStatus::DismissedByHuman => 2,
        FindingStatus::RejectedWithRationale => 3,
        FindingStatus::OutOfScopeForPr => 4,
    }
}

// Strip confidence markdown/JSON blocks from a review body to save context space.
fn strip_confidence_blocks(body: &str) -> String {
    let mut out = String::new();
    let mut in_confidence_block = false;
    let mut in_unmapped = false;
    for line in body.lines() {
        // Enter confidence block (fenced code or heading)
        if line.starts_with("### Confidence:") || line.starts_with("```pr-review-confidence-json") {
            in_confidence_block = true;
            continue;
        }
        // Exit confidence block
        if in_confidence_block {
            if line.starts_with("```")
                || (line.starts_with("### ") && !line.starts_with("### Confidence:"))
            {
                in_confidence_block = false;
                if line.starts_with("### ") {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            continue;
        }
        // Enter unmapped findings section
        if line.starts_with("Unmapped findings (not on changed lines):") {
            in_unmapped = true;
            continue;
        }
        // Unmapped findings: skip indented lines (list items), exit on anything else
        if in_unmapped {
            if line.starts_with("- ") || line.starts_with("  ") || line.trim().is_empty() {
                continue;
            }
            // Non-indented, non-empty line: end of unmapped section
            in_unmapped = false;
        }
        // Skip old confidence dimension line items
        if line.starts_with("- Style consistency")
            || line.starts_with("- Repository conventions adherence")
            || line.starts_with("- Merge conflict detection")
            || line.starts_with("- Security vulnerability detection")
            || line.starts_with("- Injection risk detection")
            || line.starts_with("- Attack-surface risk")
            || line.starts_with("- Future hardening")
            || line.starts_with("- Scope alignment")
            || line.starts_with("- Existing functionality")
            || line.starts_with("- Existing tooling")
            || line.starts_with("- Functional completeness")
            || line.starts_with("- Pattern correctness")
            || line.starts_with("- Documentation coverage")
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn build_prior_reviews_context(
    reviews: &[PullRequestReview],
    _review_comments: &[ReviewComment],
    issue_comments: &[IssueComment],
    bot_name: &str,
    bot_alias: Option<&str>,
    current_sha: &str,
) -> String {
    let mut out = String::new();

    // --- Section 1: Prior bot review summaries ---
    let mut relevant: Vec<&PullRequestReview> = reviews
        .iter()
        .filter(|r| {
            login_matches_bot(&r.user.login, bot_name, bot_alias)
                && r.commit_id.as_deref() != Some(current_sha)
        })
        .collect();
    relevant.sort_by(|a, b| a.submitted_at.cmp(&b.submitted_at));
    let tail = if relevant.len() > 3 {
        &relevant[relevant.len() - 3..]
    } else {
        &relevant[..]
    };

    for review in tail {
        let sha = review.commit_id.as_deref().unwrap_or("unknown-sha");
        let state = review.state.as_deref().unwrap_or("UNKNOWN");
        let submitted = review.submitted_at.as_deref().unwrap_or("unknown");
        out.push_str(&format!(
            "### Review on sha={} state={} ({})\n",
            sha, state, submitted
        ));
        if let Some(body) = review.body.as_deref() {
            let cleaned = strip_confidence_blocks(body);
            let truncated: String = cleaned.chars().take(2000).collect();
            out.push_str(truncated.trim());
            if cleaned.len() > 2000 {
                out.push_str("\n... [truncated]");
            }
            out.push('\n');
        }
        out.push('\n');
    }

    // Note: Inline comment threads are handled by build_addressed_findings()
    // which provides status labels ([dismissed], [likely addressed], etc.)
    // to avoid double-inclusion of the same thread data.

    // --- Section 2: PR issue comments (general conversation) ---
    let human_issue_comments: Vec<&IssueComment> = issue_comments
        .iter()
        .filter(|c| !login_matches_bot(&c.user.login, bot_name, bot_alias))
        .collect();

    if !human_issue_comments.is_empty() {
        let tail = if human_issue_comments.len() > 5 {
            &human_issue_comments[human_issue_comments.len() - 5..]
        } else {
            &human_issue_comments[..]
        };

        out.push_str("### PR Conversation\n");
        for comment in tail {
            let body_preview: String = comment.body.lines().take(15).collect::<Vec<_>>().join("\n");
            out.push_str(&format!(
                "- (@{}, {}): {}\n",
                comment.user.login,
                comment.created_at,
                body_preview.replace('\n', " "),
            ));
        }
        out.push('\n');
    }

    out
}

/// Build a list of previously flagged findings with their status.
fn build_addressed_findings(
    review_comments: &[ReviewComment],
    parsed_diff: &crate::context::diff_parser::ParsedDiff,
    bot_name: &str,
    bot_alias: Option<&str>,
) -> String {
    if review_comments.is_empty() {
        return String::new();
    }

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
    let changed_file_set: std::collections::HashSet<&str> =
        changed_files.iter().map(|f| f.as_str()).collect();

    let memory = build_review_memory(
        &[],
        review_comments,
        &[],
        bot_name,
        bot_alias,
        "",
        Some(&changed_files),
    );

    let mut out = String::new();

    for finding in memory.findings {
        let file_in_diff = changed_file_set.contains(finding.path.as_str());
        let status = match finding.status {
            FindingStatus::DismissedByHuman => "[dismissed by human]",
            FindingStatus::RejectedWithRationale => "[rejected with rationale]",
            FindingStatus::OutOfScopeForPr => "[out of scope for this pr]",
            FindingStatus::Open if !file_in_diff => "[likely addressed]",
            FindingStatus::Open | FindingStatus::LikelyAddressed => "[potentially addressed]",
        };

        let body_preview: String = finding.body.chars().take(200).collect();
        out.push_str(&format!(
            "- {}:{:?} {} {}\n",
            finding.path,
            finding.line,
            status,
            body_preview.replace('\n', " "),
        ));
    }

    out
}

fn build_review_state_context(memory: &ReviewMemory) -> String {
    let mut out = String::new();

    for finding in &memory.findings {
        let status = match finding.status {
            FindingStatus::Open => "open",
            FindingStatus::LikelyAddressed => "likely addressed",
            FindingStatus::DismissedByHuman => "dismissed by human",
            FindingStatus::RejectedWithRationale => "rejected with rationale",
            FindingStatus::OutOfScopeForPr => "out of scope for this pr",
        };
        let preview: String = finding.body.chars().take(180).collect();
        out.push_str(&format!(
            "- {}:{:?} [{}] {}\n",
            finding.path,
            finding.line,
            status,
            preview.replace('\n', " ")
        ));
    }

    if !memory.explicit_boundaries.is_empty() {
        out.push_str("\n### Explicit PR boundaries from human replies\n");
        for boundary in &memory.explicit_boundaries {
            out.push_str("- ");
            out.push_str(&boundary.replace('\n', " "));
            out.push('\n');
        }
    }

    out
}

fn can_reopen_rebutted_finding(
    comment: &ParsedReviewComment,
    path: &str,
    memory: &ReviewMemory,
) -> bool {
    comment
        .evidence_note
        .as_deref()
        .is_some_and(|note| !note.trim().is_empty())
        || memory.findings.iter().any(|prior| {
            prior.path == path
                && prior.status == FindingStatus::Open
                && prior.line != Some(comment.line)
                && token_overlap(&prior.token_set, &token_set(&comment.body)) >= 2
        })
}

fn should_suppress_finding(
    comment: &ParsedReviewComment,
    path: &str,
    memory: &ReviewMemory,
) -> bool {
    let key = finding_key(path, Some(comment.line), &comment.body);
    let tokens = token_set(&comment.body);

    for prior in &memory.findings {
        let exact_match = prior.key == key;
        let same_family = prior.path == path && token_overlap(&prior.token_set, &tokens) >= 2;
        if !exact_match && !same_family {
            continue;
        }

        match prior.status {
            FindingStatus::DismissedByHuman
            | FindingStatus::RejectedWithRationale
            | FindingStatus::OutOfScopeForPr => {
                if !can_reopen_rebutted_finding(comment, path, memory) {
                    return true;
                }
            }
            FindingStatus::LikelyAddressed => return true,
            FindingStatus::Open => {}
        }
    }

    if !memory.explicit_boundaries.is_empty() {
        for boundary in &memory.explicit_boundaries {
            if token_overlap(&tokens, &token_set(boundary)) >= 2
                && !can_reopen_rebutted_finding(comment, path, memory)
            {
                return true;
            }
        }
    }

    false
}

fn blocker_has_sufficient_evidence(
    comment: &ParsedReviewComment,
    path: &str,
    review_confidence: crate::review::parser::ConfidenceLevel,
) -> bool {
    if comment
        .evidence_note
        .as_deref()
        .is_some_and(|note| !note.trim().is_empty())
    {
        return true;
    }

    if review_confidence == crate::review::parser::ConfidenceLevel::Low {
        return false;
    }

    let body = normalize_text(&comment.body);
    let path_normalized = normalize_text(path);

    // Strong: body references the specific file path
    if body.contains(&path_normalized) {
        return true;
    }

    // Strong: causal verb phrases that indicate concrete reasoning
    const CAUSAL_PHRASES: &[&str] = &[
        "will panic",
        "will fail",
        "will crash",
        "will break",
        "will deadlock",
        "will overflow",
        "will loop",
        "will block",
        "will hang",
        "will leak",
        "will corrupt",
        "will lose",
        "will drop",
        "will skip",
        "will miss",
        "will silently",
        "will always",
        "will never",
        "can panic",
        "can fail",
        "can crash",
        "can deadlock",
        "can overflow",
        "can corrupt",
        "can lose",
        "unconditionally",
        "hardcodes",
        "hardcoded",
        "unreachable",
        "always returns",
        "never returns",
        "infinite loop",
        "use after free",
        "null dereference",
        "data race",
        "undefined behavior",
        "buffer overflow",
        "sql injection",
        "command injection",
    ];

    CAUSAL_PHRASES.iter().any(|phrase| body.contains(phrase))
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
        let label = if c.user.is_bot() {
            format!("{} (bot)", c.user.login)
        } else {
            c.user.login.clone()
        };
        out.push_str(&format!(
            "- [{}] {}: {}\n",
            c.created_at,
            label,
            c.body.replace('\n', " ")
        ));
    }
    if out.is_empty() {
        out.push_str("No thread history available.");
    }
    out
}

fn build_direct_pushback_reply(
    thread_history: &str,
    bot_name: &str,
    bot_alias: Option<&str>,
) -> Option<String> {
    let bot_lower = bot_name.to_ascii_lowercase();
    let alias_lower = bot_alias.map(|a| a.to_ascii_lowercase());
    let mention_target = format!("@{}", bot_lower);
    let mention_alias = alias_lower.as_ref().map(|a| format!("@{}", a));

    // Check for @bot mentions only in non-bot lines (avoid matching the bot's own text)
    let human_lines: Vec<&str> = thread_history
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            // Thread history format: "- [timestamp] login: body"
            // Skip lines authored by the bot itself
            !line_is_authored_by_bot(&lower, &bot_lower, alias_lower.as_deref())
        })
        .collect();

    for line in &human_lines {
        let lower = line.to_ascii_lowercase();
        if lower.contains(&mention_target) {
            return None;
        }
        if let Some(ref alias_mention) = mention_alias {
            if lower.contains(alias_mention) {
                return None;
            }
        }
    }

    // Only scan human lines for pushback signals. The bot's own review body
    // can contain phrases like "not correct" or "on purpose" as part of its
    // analysis, which would falsely trigger a canned acknowledgement.
    if human_lines
        .iter()
        .rev()
        .any(|line| has_out_of_scope_signal(line))
    {
        return Some(
            "acknowledged. i won't keep pushing that line in this PR unless later changes show a direct regression in the scoped feature.".to_string(),
        );
    }

    if human_lines
        .iter()
        .rev()
        .any(|line| has_dismissal_signal(line) || has_rationale_rejection_signal(line))
    {
        return Some(
            "got it. treating that as intentional for this PR, so i won't keep re-raising the same concern unless later changes add new concrete evidence.".to_string(),
        );
    }

    None
}

/// Check if a lowercased thread-history line was authored by the bot.
/// Thread history format: `- [timestamp] login: body` or `- [timestamp] login (bot): body`
fn line_is_authored_by_bot(lower_line: &str, bot_lower: &str, alias_lower: Option<&str>) -> bool {
    // Strip leading "- [timestamp] " to get "login: body" or "login (bot): body"
    let after_bracket = match lower_line.find("] ") {
        Some(pos) => &lower_line[pos + 2..],
        None => return false,
    };
    let raw_login = match after_bracket.find(": ") {
        Some(pos) => after_bracket[..pos].trim(),
        None => return false,
    };
    // Strip " (bot)" suffix if present
    let login = raw_login
        .strip_suffix(" (bot)")
        .unwrap_or(raw_login);
    login == bot_lower || alias_lower.is_some_and(|a| login == a)
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

#[derive(Debug)]
struct ReviewTranscriptEvent {
    sort_key: String,
    ordinal: usize,
    heading: String,
    body: String,
}

fn terminal_pr_state(pr_data: &PullRequest) -> &'static str {
    if pr_data.merged_at.is_some() {
        "merged"
    } else {
        "closed"
    }
}

fn build_final_review_transcript(
    repo_name: &str,
    pr_data: &PullRequest,
    terminal_state: &str,
    review_attempts: &[ReviewAttemptRecord],
    existing_reviews: &[PullRequestReview],
    review_comments: &[ReviewComment],
    issue_comments: &[IssueComment],
) -> String {
    let mut transcript = String::new();
    transcript.push_str("# PR Review Transcript\n\n");
    transcript.push_str(&format!("- Repo: {repo_name}\n"));
    transcript.push_str(&format!("- PR: #{} - {}\n", pr_data.number, pr_data.title));
    transcript.push_str(&format!("- Author: @{}\n", pr_data.user.login));
    transcript.push_str(&format!("- Terminal state: {terminal_state}\n"));
    transcript.push_str(&format!("- Head SHA at archive time: {}\n", pr_data.head.sha));
    if let Some(url) = pr_data.html_url.as_deref() {
        transcript.push_str(&format!("- URL: {url}\n"));
    }
    if let Some(closed_at) = pr_data.closed_at.as_deref().or(pr_data.merged_at.as_deref()) {
        transcript.push_str(&format!("- Closed at: {closed_at}\n"));
    }
    transcript.push('\n');

    let mut events = Vec::new();
    let mut ordinal = 0usize;

    for attempt in review_attempts {
        ordinal += 1;
        let mut body = String::new();
        body.push_str(&format!("- sha: {}\n", attempt.sha));
        body.push_str(&format!("- harness: {}\n", attempt.harness));
        if let Some(model) = attempt.model.as_deref() {
            body.push_str(&format!("- model: {model}\n"));
        }
        body.push_str(&format!("- status: {}\n", attempt.status));
        if let Some(verdict) = attempt.verdict.as_deref() {
            body.push_str(&format!("- verdict: {verdict}\n"));
        }
        if let Some(comments_posted) = attempt.comments_posted {
            body.push_str(&format!("- comments_posted: {comments_posted}\n"));
        }
        if let Some(duration_secs) = attempt.duration_secs {
            body.push_str(&format!("- duration_secs: {:.1}\n", duration_secs));
        }
        if let Some(completed_at) = attempt.completed_at.as_deref() {
            body.push_str(&format!("- completed_at: {completed_at}\n"));
        }
        if let Some(error) = attempt.error_message.as_deref() {
            body.push_str("- error:\n");
            body.push_str(&quote_markdown_block(error));
        }

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(&attempt.created_at),
            ordinal,
            heading: format!("Local review attempt {}", attempt.created_at),
            body,
        });
    }

    for review in existing_reviews {
        ordinal += 1;
        let submitted_at = review.submitted_at.as_deref().unwrap_or("unknown");
        let mut body = String::new();
        if let Some(state) = review.state.as_deref() {
            body.push_str(&format!("- state: {state}\n"));
        }
        if let Some(commit_id) = review.commit_id.as_deref() {
            body.push_str(&format!("- commit: {commit_id}\n"));
        }
        body.push_str("- body:\n");
        body.push_str(&quote_markdown_block(
            review.body.as_deref().unwrap_or("(empty review body)"),
        ));

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(submitted_at),
            ordinal,
            heading: format!("GitHub review by @{} ({submitted_at})", review.user.login),
            body,
        });
    }

    for comment in review_comments {
        ordinal += 1;
        let location = comment
            .line
            .map(|line| format!("{}:{}", comment.path, line))
            .unwrap_or_else(|| comment.path.clone());
        let mut body = String::new();
        if let Some(parent_id) = comment.in_reply_to_id {
            body.push_str(&format!("- in_reply_to_id: {parent_id}\n"));
        }
        body.push_str("- body:\n");
        body.push_str(&quote_markdown_block(&comment.body));

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(&comment.created_at),
            ordinal,
            heading: format!(
                "Review thread comment by @{} on {} ({})",
                comment.user.login, location, comment.created_at
            ),
            body,
        });
    }

    for comment in issue_comments {
        ordinal += 1;
        let mut body = String::new();
        body.push_str("- body:\n");
        body.push_str(&quote_markdown_block(&comment.body));

        events.push(ReviewTranscriptEvent {
            sort_key: timestamp_sort_key(&comment.created_at),
            ordinal,
            heading: format!("Issue comment by @{} ({})", comment.user.login, comment.created_at),
            body,
        });
    }

    events.sort_by(|left, right| {
        left.sort_key
            .cmp(&right.sort_key)
            .then(left.ordinal.cmp(&right.ordinal))
    });

    if events.is_empty() {
        transcript.push_str("No persisted review attempts or GitHub conversation were available.\n");
        return transcript;
    }

    transcript.push_str("## Timeline\n\n");
    for event in events {
        transcript.push_str("### ");
        transcript.push_str(&event.heading);
        transcript.push('\n');
        transcript.push_str(&event.body);
        if !event.body.ends_with('\n') {
            transcript.push('\n');
        }
        transcript.push('\n');
    }

    transcript
}

fn build_final_review_summary_prompt(
    repo_name: &str,
    pr_data: &PullRequest,
    terminal_state: &str,
    transcript: &str,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("Write a concise retrospective summary of this completed PR review.\n");
    prompt.push_str("The PR is already closed or merged, so describe what the review covered and how it concluded.\n");
    prompt.push_str("Focus on the important review themes, notable issues raised, whether follow-up discussion resolved them, and the final outcome.\n");
    prompt.push_str("If details are sparse, say that plainly instead of inventing specifics.\n");
    prompt.push_str("Output plain markdown only. No code fences. Keep it under 250 words.\n\n");
    prompt.push_str(&format!("Repo: {repo_name}\n"));
    prompt.push_str(&format!("PR: #{}\n", pr_data.number));
    prompt.push_str(&format!("Title: {}\n", pr_data.title));
    prompt.push_str(&format!("Author: @{}\n", pr_data.user.login));
    prompt.push_str(&format!("Terminal state: {terminal_state}\n"));
    prompt.push_str(&format!("Head SHA at archive time: {}\n\n", pr_data.head.sha));
    prompt.push_str("Transcript:\n\n");
    prompt.push_str(transcript);
    prompt
}

fn build_final_review_summary_fallback(
    pr_data: &PullRequest,
    terminal_state: &str,
    review_attempts: &[ReviewAttemptRecord],
) -> String {
    let completed = review_attempts
        .iter()
        .filter(|attempt| attempt.status == "completed")
        .count();
    let failed = review_attempts
        .iter()
        .filter(|attempt| attempt.status == "failed")
        .count();
    let last_verdict = review_attempts
        .iter()
        .rev()
        .find_map(|attempt| attempt.verdict.as_deref());

    let mut summary = format!(
        "PR #{} ended as {} after {} recorded review attempt(s).",
        pr_data.number,
        terminal_state,
        review_attempts.len()
    );
    if completed > 0 || failed > 0 {
        summary.push_str(&format!(" {} completed, {} failed.", completed, failed));
    }
    if let Some(verdict) = last_verdict {
        summary.push_str(&format!(" Final recorded verdict: {verdict}."));
    }
    summary.push_str(" Full transcript archived in the database.");
    summary
}

fn normalize_summary_output(stdout: &str, stderr: &str) -> Option<String> {
    let out = stdout.trim();
    let err = stderr.trim();
    let combined = if !out.is_empty() {
        out
    } else if !err.is_empty() {
        err
    } else {
        return None;
    };
    let normalized = strip_fenced_code_block(combined.trim());
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Strip an optional fenced code block wrapper (``` or ```lang) that some
/// harnesses add around their output.  Falls back to plain backtick trimming
/// for outputs that are just backtick-quoted without a newline.
fn strip_fenced_code_block(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() >= 2 {
        let first = lines[0].trim();
        let last = lines[lines.len() - 1].trim();
        if first.starts_with("```") && last == "```" {
            // Strip the opening fence (with optional language tag) and closing fence.
            // Return the inner content — even if empty — rather than falling through
            // to the backtick-trimming path, which would leave the language tag (e.g.
            // "markdown") as if it were real content when the inner body is blank.
            let inner = &lines[1..lines.len() - 1];
            return inner.join("\n").trim().to_string();
        }
    }
    // Fallback: strip loose backticks from edges (bare ` wrapping)
    text.trim_matches('`').trim().to_string()
}

fn quote_markdown_block(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "> (empty)\n".to_string();
    }

    let mut out = String::new();
    for line in trimmed.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn timestamp_sort_key(timestamp: &str) -> String {
    let trimmed = timestamp.trim();
    if trimmed.is_empty() {
        return "9999-12-31T23:59:59Z".to_string();
    }
    if trimmed.contains('T') {
        return trimmed.to_string();
    }
    format!("{}Z", trimmed.replace(' ', "T"))
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
    pr_author: &str,
    pr_title: &str,
    sha: &str,
    has_prior_bot_review: bool,
) -> String {
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
        prompt.push_str("- This is first-touch on this PR: briefly identify as an automated code reviewer and include [pr-reviewer](");
        prompt.push_str(pr_reviewer_project_url());
        prompt.push_str(").\n");
    }

    prompt
}

fn build_in_progress_comment_fallback(
    pr_author: &str,
    pr_title: &str,
    sha: &str,
    has_prior_bot_review: bool,
) -> String {
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
            "{} - I'm an automated code reviewer powered by [pr-reviewer]({}). I'm taking a look at {} in {} (commit `{}`) now and I'll follow up shortly with feedback.",
            greeting,
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

    // Do not post arbitrary raw harness output to GitHub comments.
    // If we cannot parse structured text, fall back to deterministic templates.
    let _ = looks_like_harness_error_output(trimmed);
    None
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

fn truncate_github_comment_body(body: &str) -> String {
    if body.chars().count() <= GITHUB_COMMENT_MAX_CHARS {
        return body.to_string();
    }

    let keep = GITHUB_COMMENT_MAX_CHARS.saturating_sub(GITHUB_COMMENT_TRUNCATION_NOTE.len());
    let mut truncated: String = body.chars().take(keep).collect();
    while truncated.ends_with('\n') {
        truncated.pop();
    }
    truncated.push_str(GITHUB_COMMENT_TRUNCATION_NOTE);
    truncated
}

fn build_retry_review_body(body: &str, inline_comments: &[CreateReviewComment]) -> String {
    if inline_comments.is_empty() {
        return truncate_github_comment_body(body);
    }

    let mut retry_body = truncate_github_comment_body(body);
    let header = "\n\n**Inline comments (could not post as line comments):**\n";
    if retry_body.chars().count() + header.chars().count() > GITHUB_COMMENT_MAX_CHARS {
        return retry_body;
    }
    retry_body.push_str(header);

    let total = inline_comments.len();
    let mut appended = 0usize;

    for (idx, comment) in inline_comments.iter().enumerate() {
        let normalized_comment = comment
            .body
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let line = format!(
            "- `{}:{}` - {}\n",
            comment.path, comment.line, normalized_comment
        );
        let remaining = total.saturating_sub(idx + 1);
        let omission_note = if remaining > 0 {
            format!(
                "\n_Omitted {remaining} additional inline comments to stay within GitHub's body limit._"
            )
        } else {
            String::new()
        };

        if retry_body.chars().count() + line.chars().count() + omission_note.chars().count()
            > GITHUB_COMMENT_MAX_CHARS
        {
            break;
        }

        retry_body.push_str(&line);
        appended += 1;
    }

    let omitted = total.saturating_sub(appended);
    if omitted > 0 {
        let omission_note = format!(
            "\n_Omitted {omitted} additional inline comments to stay within GitHub's body limit._"
        );
        if retry_body.chars().count() + omission_note.chars().count() <= GITHUB_COMMENT_MAX_CHARS {
            retry_body.push_str(&omission_note);
        }
    }

    retry_body
}

fn looks_like_reintroduction(comment: &str) -> bool {
    let lower = comment.to_ascii_lowercase();
    lower.contains("pr-reviewing agent")
        || lower.contains("automated code reviewer")
        || lower.contains("powered by [pr-reviewer]")
        || lower.contains("powered by pr-reviewer")
        || lower.contains("[pr-reviewer](")
}

fn looks_like_harness_error_output(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("error:")
        || lower.contains("traceback")
        || lower.contains("panic")
        || lower.contains("exception")
        || lower.contains("exit status")
        || lower.contains("usage:")
        || lower.contains("command not found")
}

fn looks_like_harness_transport_output(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.contains("{\"type\":\"thread.started\"")
        || trimmed.contains("{\"type\":\"turn.started\"")
        || (trimmed.contains("\"type\":\"command_execution\"")
            && trimmed.contains("\"item.started\""))
        || trimmed.contains("Unable to open session log file")
        || (trimmed.contains("codex: line") && trimmed.contains("unbound variable"))
}

fn truncate_utf8_to_max_bytes(input: &mut String, max_bytes: usize) {
    if input.len() <= max_bytes {
        return;
    }
    let mut boundary = max_bytes.min(input.len());
    while boundary > 0 && !input.is_char_boundary(boundary) {
        boundary -= 1;
    }
    input.truncate(boundary);
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
        &["docs", "doc", "readme", "guide", "guides", "documentation"],
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

/// Check if a diff should be skipped based on file types and config.
fn should_skip_review(
    changed_files: &[String],
    defaults: &crate::config::DefaultsConfig,
) -> Option<&'static str> {
    if changed_files.is_empty() {
        return Some("empty-diff");
    }

    if defaults.skip_docs_only {
        let all_docs = changed_files.iter().all(|f| {
            let lower = f.to_lowercase();
            lower.ends_with(".md")
                || lower.ends_with(".txt")
                || lower.ends_with(".rst")
                || lower.ends_with(".adoc")
                || lower.starts_with("docs/")
                || lower.starts_with("doc/")
                || lower == "license"
                || lower == "license.md"
                || lower == "license.txt"
                || lower == "changelog"
                || lower == "changelog.md"
                || lower == "changelog.txt"
                || lower == "changes"
                || lower == "changes.md"
                || lower == "changes.txt"
                || lower == "readme"
                || lower == "readme.md"
        });
        if all_docs {
            return Some("docs-only");
        }
    }

    None
}

const UI_EXTENSIONS: &[&str] = &[
    ".css", ".scss", ".less", ".svelte", ".tsx", ".jsx", ".vue", ".html",
];

fn detect_ui_files(changed_files: &[String]) -> Vec<String> {
    changed_files
        .iter()
        .filter(|f| {
            let lower = f.to_lowercase();
            UI_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
        })
        .cloned()
        .collect()
}

fn has_screenshot_references(pr_body: Option<&str>) -> bool {
    let Some(body) = pr_body else {
        return false;
    };
    let lower = body.to_lowercase();
    lower.contains("![")
        || lower.contains("<img")
        || lower.contains(".png")
        || lower.contains(".jpg")
        || lower.contains(".jpeg")
        || lower.contains(".gif")
        || lower.contains(".webp")
        || lower.contains("screenshot")
}

fn verdict_label(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::NoIssues | ReviewVerdict::Approve => "NO_ISSUES",
        ReviewVerdict::Comment => "COMMENT",
        ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
    }
}

#[cfg(test)]
mod tests {
    use crate::github::types::{CreateReviewComment, PullRequestReview, ReviewComment, User};
    use crate::review::parser::{
        CommentSeverity, ConfidenceLevel, ReviewComment as ParsedReviewComment,
    };

    use super::{
        blocker_has_sufficient_evidence, build_direct_pushback_reply,
        build_in_progress_comment_fallback, build_prior_reviews_context, build_retry_review_body,
        build_review_memory, build_thread_history, extract_in_progress_comment, finding_key,
        infer_pr_focus, line_is_authored_by_bot, login_matches_bot,
        looks_like_harness_error_output, looks_like_harness_transport_output,
        looks_like_reintroduction, normalize_in_progress_comment, pr_reviewer_project_url,
        resolve_project_url, should_suppress_finding, truncate_github_comment_body,
        FindingStatus, ReviewMemory, GITHUB_COMMENT_MAX_CHARS, GITHUB_COMMENT_TRUNCATION_NOTE,
    };

    fn test_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: None,
        }
    }

    fn test_bot_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: Some("Bot".to_string()),
        }
    }

    fn review_comment(
        id: u64,
        in_reply_to: Option<u64>,
        login: &str,
        path: &str,
        line: Option<u32>,
        body: &str,
    ) -> ReviewComment {
        ReviewComment {
            id,
            body: body.to_string(),
            path: path.to_string(),
            line,
            user: test_user(login),
            in_reply_to_id: in_reply_to,
            created_at: format!("2026-03-30T00:{:02}:00Z", id),
        }
    }

    #[test]
    fn first_touch_comment_mentions_repo_link() {
        let message = build_in_progress_comment_fallback(
            "contributor",
            "fix race condition in queue",
            "527fae59abcde",
            false,
        );

        assert!(message.contains("automated code reviewer"));
        assert!(message.contains("Hi @contributor"));
        assert!(message.contains(&format!("[pr-reviewer]({})", pr_reviewer_project_url())));
        assert!(message.contains("commit `527fae59`"));
    }

    #[test]
    fn follow_up_comment_is_conversational_without_reintro() {
        let message = build_in_progress_comment_fallback(
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
        let message =
            build_in_progress_comment_fallback("contributor", "fix \"the\" bug", "527fae59", false);
        assert!(message.contains("`fix 'the' bug`"));
    }

    #[test]
    fn fallback_comment_sanitizes_backticks_in_title() {
        let message =
            build_in_progress_comment_fallback("contributor", "fix `foo` crash", "527fae59", false);
        assert!(message.contains("`fix 'foo' crash`"));
    }

    #[test]
    fn fallback_comment_neutralizes_markdown_links_in_title() {
        let message = build_in_progress_comment_fallback(
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
    fn rejects_unstructured_raw_in_progress_comment_output() {
        assert!(extract_in_progress_comment("just some raw text", "").is_none());
    }

    #[test]
    fn normalizes_generated_comment_whitespace() {
        let normalized = normalize_in_progress_comment("  hi there \n  doing a pass  ").unwrap();
        assert_eq!(normalized, "hi there doing a pass");
    }

    #[test]
    fn truncates_github_comment_body_to_limit() {
        let body = "a".repeat(GITHUB_COMMENT_MAX_CHARS + 100);
        let truncated = truncate_github_comment_body(&body);

        assert_eq!(truncated.chars().count(), GITHUB_COMMENT_MAX_CHARS);
        assert!(truncated.ends_with(GITHUB_COMMENT_TRUNCATION_NOTE));
    }

    #[test]
    fn retry_review_body_omits_extra_inline_comments_to_fit_limit() {
        let base = "intro\n".repeat(5000);
        let inline_comments = vec![
            CreateReviewComment {
                path: "src/lib.rs".to_string(),
                line: 10,
                side: "RIGHT".to_string(),
                body: "x".repeat(20_000),
            },
            CreateReviewComment {
                path: "src/lib.rs".to_string(),
                line: 20,
                side: "RIGHT".to_string(),
                body: "y".repeat(20_000),
            },
            CreateReviewComment {
                path: "src/lib.rs".to_string(),
                line: 30,
                side: "RIGHT".to_string(),
                body: "z".repeat(20_000),
            },
        ];

        let retry_body = build_retry_review_body(&base, &inline_comments);

        assert!(retry_body.chars().count() <= GITHUB_COMMENT_MAX_CHARS);
        assert!(retry_body.contains("Inline comments (could not post as line comments):"));
        assert!(retry_body.contains("Omitted"));
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
            "Error: command returned exit status 1"
        ));
    }

    #[test]
    fn harness_transport_output_is_rejected() {
        assert!(looks_like_harness_transport_output(
            "{\"type\":\"thread.started\",\"thread_id\":\"abc\"}\n{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\"}}"
        ));
        assert!(looks_like_harness_transport_output(
            "/home/nicholai/.config/signet/bin/codex: line 6: HOME: unbound variable"
        ));
    }

    #[test]
    fn login_match_checks_configured_and_authenticated_names() {
        assert!(login_matches_bot(
            "pr-reviewer",
            "pr-reviewer",
            Some("NicholaiVogel")
        ));
        assert!(login_matches_bot(
            "NicholaiVogel",
            "pr-reviewer",
            Some("NicholaiVogel")
        ));
        assert!(!login_matches_bot(
            "someone-else",
            "pr-reviewer",
            Some("NicholaiVogel")
        ));
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
        assert_eq!(infer_pr_focus("resolve review comments"), "the changes");
    }

    #[test]
    fn prior_reviews_context_uses_alias_match() {
        let reviews = vec![PullRequestReview {
            id: 1,
            body: Some("looks good".to_string()),
            user: test_user("NicholaiVogel"),
            state: Some("COMMENTED".to_string()),
            commit_id: Some("previoussha".to_string()),
            submitted_at: Some("2026-03-17T00:00:00Z".to_string()),
        }];

        let context = build_prior_reviews_context(
            &reviews,
            &[],
            &[],
            "pr-reviewer",
            Some("NicholaiVogel"),
            "currentsha",
        );
        assert!(context.contains("looks good"));
    }

    #[test]
    fn build_review_memory_marks_explicit_scope_boundaries() {
        let review_comments = vec![
            ReviewComment {
                id: 10,
                body: "Blocking: task ownership is wrong here".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(42),
                user: test_user("pr-reviewer"),
                in_reply_to_id: None,
                created_at: "2026-03-30T00:00:00Z".to_string(),
            },
            ReviewComment {
                id: 11,
                body: "not taking further task ownership feedback in this PR".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(42),
                user: test_user("NicholaiVogel"),
                in_reply_to_id: Some(10),
                created_at: "2026-03-30T00:01:00Z".to_string(),
            },
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head",
            None,
        );
        assert_eq!(memory.findings.len(), 1);
        assert_eq!(memory.findings[0].status, FindingStatus::OutOfScopeForPr);
        assert_eq!(memory.explicit_boundaries.len(), 1);
    }

    #[test]
    fn build_review_memory_marks_unmentioned_findings_as_likely_addressed() {
        let review_comments = vec![ReviewComment {
            id: 10,
            body: "Potential blocking issue in task cleanup".to_string(),
            path: "src/task.rs".to_string(),
            line: Some(42),
            user: test_user("pr-reviewer"),
            in_reply_to_id: None,
            created_at: "2026-03-30T00:00:00Z".to_string(),
        }];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head",
            Some(&["src/other.rs".to_string()]),
        );

        assert_eq!(memory.findings[0].status, FindingStatus::LikelyAddressed);
    }

    #[test]
    fn suppresses_repeated_out_of_scope_finding_without_new_evidence() {
        let memory = ReviewMemory {
            findings: vec![super::FindingRecord {
                key: finding_key(
                    "src/task.rs",
                    Some(42),
                    "Blocking: task ownership is wrong here",
                ),
                path: "src/task.rs".to_string(),
                line: Some(42),
                body: "Blocking: task ownership is wrong here".to_string(),
                token_set: super::token_set("Blocking: task ownership is wrong here"),
                status: FindingStatus::OutOfScopeForPr,
            }],
            already_noted_screenshot: false,
            has_prior_bot_review: true,
            explicit_boundaries: vec![
                "not taking further task ownership feedback in this PR".to_string()
            ],
        };

        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 42,
            body: "Blocking: task ownership is wrong here".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(should_suppress_finding(&comment, "src/task.rs", &memory));
    }

    #[test]
    fn allows_reopening_rebutted_finding_with_evidence_note() {
        let memory = ReviewMemory {
            findings: vec![super::FindingRecord {
                key: finding_key("src/task.rs", Some(42), "task ownership is wrong here"),
                path: "src/task.rs".to_string(),
                line: Some(42),
                body: "task ownership is wrong here".to_string(),
                token_set: super::token_set("task ownership is wrong here"),
                status: FindingStatus::RejectedWithRationale,
            }],
            already_noted_screenshot: false,
            has_prior_bot_review: true,
            explicit_boundaries: vec![],
        };

        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 50,
            body: "Blocking: task ownership is still wrong on the new trigger path".to_string(),
            evidence_note: Some(
                "new trigger path at src/task.rs:50 now routes cross-agent writes".to_string(),
            ),
            severity: CommentSeverity::Blocking,
        };

        assert!(!should_suppress_finding(&comment, "src/task.rs", &memory));
    }

    #[test]
    fn weak_blocker_is_downgraded_without_evidence() {
        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 10,
            body: "Blocking: this might be a problem later.".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(!blocker_has_sufficient_evidence(
            &comment,
            "src/task.rs",
            ConfidenceLevel::Low
        ));
    }

    #[test]
    fn direct_pushback_reply_acknowledges_boundary() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: i'm not taking further task ownership feedback in this PR",
            "pr-reviewer",
            None,
        )
        .expect("reply");
        assert!(reply.contains("won't keep pushing"));
    }

    #[test]
    fn direct_pushback_reply_detects_bot_mention_format() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: thanks @pr-reviewer",
            "pr-reviewer",
            None,
        );

        assert!(reply.is_none());
    }

    // --- Regression tests: blocker evidence gate ---

    #[test]
    fn blocker_with_causal_phrase_passes_evidence_gate() {
        let comment = ParsedReviewComment {
            file: "src/engine.rs".to_string(),
            line: 100,
            body: "This will panic when the input is empty because unwrap() is called on None".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/engine.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_with_bare_will_no_consequence_fails_evidence_gate() {
        // "will" alone without a consequence verb should NOT pass
        let comment = ParsedReviewComment {
            file: "src/engine.rs".to_string(),
            line: 100,
            body: "This function will need refactoring eventually for maintainability.".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(!blocker_has_sufficient_evidence(
            &comment,
            "src/engine.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_referencing_file_path_passes_evidence_gate() {
        let comment = ParsedReviewComment {
            file: "src/config.rs".to_string(),
            line: 55,
            body: "The default in src/config.rs contradicts the documented behavior".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/config.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_with_evidence_note_always_passes() {
        let comment = ParsedReviewComment {
            file: "src/task.rs".to_string(),
            line: 10,
            body: "something vague".to_string(),
            evidence_note: Some("line 10 assigns None then unwraps at line 12".to_string()),
            severity: CommentSeverity::Blocking,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/task.rs",
            ConfidenceLevel::Low,
        ));
    }

    #[test]
    fn blocker_with_sql_injection_phrase_passes() {
        let comment = ParsedReviewComment {
            file: "src/db.rs".to_string(),
            line: 30,
            body: "User input is interpolated directly into the query, creating a sql injection vector".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(blocker_has_sufficient_evidence(
            &comment,
            "src/db.rs",
            ConfidenceLevel::Medium,
        ));
    }

    #[test]
    fn blocker_low_confidence_no_evidence_fails() {
        let comment = ParsedReviewComment {
            file: "src/db.rs".to_string(),
            line: 30,
            body: "This will panic and crash and burn".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Blocking,
        };

        assert!(!blocker_has_sufficient_evidence(
            &comment,
            "src/db.rs",
            ConfidenceLevel::Low,
        ));
    }

    // --- Regression tests: bot-mention guard ---

    #[test]
    fn bot_mention_guard_ignores_bots_own_text() {
        // Bot's own comment references @pr-reviewer, but a human said "not in scope"
        // The guard should NOT bail just because the bot mentioned itself
        let history = "- [2026-03-30] pr-reviewer: see @pr-reviewer docs for details\n\
                        - [2026-03-30] NicholaiVogel: this is out of scope for this PR";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_some(), "should produce a pushback reply");
        assert!(reply.unwrap().contains("won't keep pushing"));
    }

    #[test]
    fn bot_mention_guard_fires_on_human_mention() {
        // Human explicitly @-mentions the bot, expecting a substantive response
        let history = "- [2026-03-30] pr-reviewer: this code has a bug\n\
                        - [2026-03-30] NicholaiVogel: @pr-reviewer can you re-check this?";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_none(), "should bail when human mentions the bot");
    }

    #[test]
    fn bot_mention_guard_checks_alias() {
        let history = "- [2026-03-30] NicholaiVogel: @PR-Reviewer-Ant please look again";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", Some("PR-Reviewer-Ant"));
        assert!(reply.is_none(), "should bail when human mentions the bot alias");
    }

    #[test]
    fn bot_mention_guard_alias_in_bot_line_ignored() {
        let history = "- [2026-03-30] PR-Reviewer-Ant: reviewed by @PR-Reviewer-Ant\n\
                        - [2026-03-30] NicholaiVogel: nah, out of scope";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", Some("PR-Reviewer-Ant"));
        assert!(reply.is_some(), "bot's own alias mention should not trigger the guard");
    }

    // --- Regression tests: status precedence in build_review_memory ---

    #[test]
    fn status_precedence_keeps_strongest_signal() {
        // RejectedWithRationale (3) > DismissedByHuman (2)
        // If reply #1 is rationale rejection and reply #2 is dismissal,
        // the final status should remain RejectedWithRationale
        use super::status_precedence;

        assert!(
            status_precedence(&FindingStatus::RejectedWithRationale)
                > status_precedence(&FindingStatus::DismissedByHuman)
        );
        assert!(
            status_precedence(&FindingStatus::OutOfScopeForPr)
                > status_precedence(&FindingStatus::RejectedWithRationale)
        );
        assert!(
            status_precedence(&FindingStatus::DismissedByHuman)
                > status_precedence(&FindingStatus::LikelyAddressed)
        );
    }

    #[test]
    fn build_review_memory_rationale_not_downgraded_by_later_dismissal() {
        // Simulates: reply #1 has rationale rejection, reply #2 is a soft dismissal.
        // Final status must be RejectedWithRationale (stronger), not DismissedByHuman.
        let review_comments = vec![
            review_comment(1, None, "pr-reviewer", "src/task.rs", Some(10), "Bug: wrong logic here"),
            review_comment(2, Some(1), "NicholaiVogel", "src/task.rs", Some(10), "this is a false positive, the spec requires this behavior"),
            review_comment(3, Some(1), "AnotherDev", "src/task.rs", Some(10), "yeah just leave it, it's fine"),
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head-sha",
            Some(&["src/task.rs".to_string()]),
        );

        assert_eq!(memory.findings.len(), 1);
        assert_eq!(
            memory.findings[0].status,
            FindingStatus::RejectedWithRationale,
            "stronger rationale rejection should not be downgraded by a later soft dismissal"
        );
    }

    // --- Regression tests: LikelyAddressed ---

    #[test]
    fn finding_on_unchanged_file_is_likely_addressed() {
        let review_comments = vec![
            review_comment(1, None, "pr-reviewer", "src/old.rs", Some(10), "Bug found here"),
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "new-sha",
            Some(&["src/new.rs".to_string()]),
        );

        assert_eq!(memory.findings.len(), 1);
        assert_eq!(memory.findings[0].status, FindingStatus::LikelyAddressed);
    }

    #[test]
    fn likely_addressed_finding_is_suppressed() {
        let memory = ReviewMemory {
            findings: vec![super::FindingRecord {
                key: finding_key("src/old.rs", Some(10), "Bug found here"),
                path: "src/old.rs".to_string(),
                line: Some(10),
                body: "Bug found here".to_string(),
                token_set: super::token_set("Bug found here"),
                status: FindingStatus::LikelyAddressed,
            }],
            already_noted_screenshot: false,
            has_prior_bot_review: true,
            explicit_boundaries: vec![],
        };

        let comment = ParsedReviewComment {
            file: "src/old.rs".to_string(),
            line: 10,
            body: "Bug found here".to_string(),
            evidence_note: None,
            severity: CommentSeverity::Warning,
        };

        assert!(should_suppress_finding(&comment, "src/old.rs", &memory));
    }

    // --- Regression test: line_is_authored_by_bot ---

    #[test]
    fn line_is_authored_by_bot_parses_thread_format() {
        use super::line_is_authored_by_bot;

        assert!(line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] pr-reviewer: some review text",
            "pr-reviewer",
            None,
        ));
        assert!(!line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] nicholaivogel: some comment",
            "pr-reviewer",
            None,
        ));
        assert!(line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] pr-reviewer-ant: automated review",
            "pr-reviewer",
            Some("pr-reviewer-ant"),
        ));
        assert!(!line_is_authored_by_bot(
            "no bracket line",
            "pr-reviewer",
            None,
        ));
    }

    // --- Regression test: pushback reply for dismissal signal ---

    #[test]
    fn direct_pushback_reply_acknowledges_dismissal() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: nah, this is intentional, leave it",
            "pr-reviewer",
            None,
        )
        .expect("should produce reply");
        assert!(reply.contains("treating that as intentional"));
    }

    #[test]
    fn direct_pushback_reply_returns_none_for_neutral_comment() {
        let reply = build_direct_pushback_reply(
            "- [2026-03-30] NicholaiVogel: interesting, let me think about it",
            "pr-reviewer",
            None,
        );
        assert!(reply.is_none(), "neutral comments should not trigger a canned reply");
    }

    // --- Multi-bot safeguard tests ---

    #[test]
    fn user_is_bot_detects_github_bot_accounts() {
        let bot = test_bot_user("BusyBee3333");
        assert!(bot.is_bot());

        let human = test_user("NicholaiVogel");
        assert!(!human.is_bot());

        let unknown = User {
            login: "someone".to_string(),
            account_type: None,
        };
        assert!(!unknown.is_bot(), "None account_type should not be treated as bot");
    }

    #[test]
    fn build_thread_history_labels_bot_comments() {
        let comments = vec![
            ReviewComment {
                id: 1,
                body: "Bug: wrong logic here".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(10),
                user: test_bot_user("BusyBee3333"),
                in_reply_to_id: None,
                created_at: "2026-03-30T04:16:00Z".to_string(),
            },
            ReviewComment {
                id: 2,
                body: "I disagree, this is intentional".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(10),
                user: test_user("NicholaiVogel"),
                in_reply_to_id: Some(1),
                created_at: "2026-03-30T04:17:00Z".to_string(),
            },
        ];

        let history = build_thread_history(&comments, 1);
        assert!(
            history.contains("BusyBee3333 (bot):"),
            "bot comments should be labeled: {history}"
        );
        assert!(
            !history.contains("NicholaiVogel (bot)"),
            "human comments should not be labeled as bot: {history}"
        );
        assert!(history.contains("NicholaiVogel:"));
    }

    #[test]
    fn line_is_authored_by_bot_handles_bot_label_suffix() {
        // The new format includes "(bot)" for bot accounts
        assert!(line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] pr-reviewer (bot): some review text",
            "pr-reviewer",
            None,
        ));
        assert!(!line_is_authored_by_bot(
            "- [2026-03-30t04:14:20z] nicholaivogel: some comment",
            "pr-reviewer",
            None,
        ));
    }

    #[test]
    fn bot_mention_guard_ignores_bot_labeled_lines() {
        // Bot's own comment has (bot) label and references @pr-reviewer,
        // but a human said "out of scope". Guard should not bail.
        let history = "- [2026-03-30] pr-reviewer (bot): see @pr-reviewer docs for details\n\
                        - [2026-03-30] NicholaiVogel: this is out of scope for this PR";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_some(), "should produce a pushback reply despite bot's own @mention");
    }

    #[test]
    fn build_review_memory_treats_other_bot_as_non_human() {
        // When another bot (BusyBee3333) replies to our finding, it should not
        // be treated as a human dismissal. Only this bot's own comments and
        // genuine human comments should influence finding status.
        //
        // Note: currently build_review_memory uses login_matches_bot which only
        // knows about *this* bot's identity. Other bots' comments are treated as
        // human replies. This test documents the current behavior and would need
        // updating if we add peer-bot awareness to build_review_memory.
        let review_comments = vec![
            review_comment(1, None, "pr-reviewer", "src/task.rs", Some(10), "Bug here"),
            // Another bot says "fine" (a dismissal signal)
            ReviewComment {
                id: 2,
                body: "fine, this looks acceptable".to_string(),
                path: "src/task.rs".to_string(),
                line: Some(10),
                user: test_bot_user("BusyBee3333"),
                in_reply_to_id: Some(1),
                created_at: "2026-03-30T00:02:00Z".to_string(),
            },
        ];

        let memory = build_review_memory(
            &[],
            &review_comments,
            &[],
            "pr-reviewer",
            None,
            "head-sha",
            Some(&["src/task.rs".to_string()]),
        );

        // Currently other bots ARE treated as human. This test documents
        // that behavior so we notice if/when we change it.
        assert_eq!(memory.findings.len(), 1);
        assert_eq!(
            memory.findings[0].status,
            FindingStatus::DismissedByHuman,
            "other bot comments are currently classified as human replies (known limitation)"
        );
    }

    #[test]
    fn signal_scan_ignores_bots_own_review_text() {
        // The bot's previous review body might contain phrases like "not correct"
        // or "on purpose" as part of its analysis. These should NOT trigger
        // a canned pushback reply when a human makes a neutral comment afterward.
        let history = "- [2026-03-30] pr-reviewer: this logic is not correct and the pattern is intentional duplication\n\
                        - [2026-03-30] NicholaiVogel: interesting, let me look at this more closely";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(
            reply.is_none(),
            "bot's own text containing 'not correct' or 'intentional' should not trigger a canned reply"
        );
    }

    #[test]
    fn signal_scan_still_fires_on_human_dismissal_with_bot_noise() {
        // Even when the bot's own text contains signal words, a genuine human
        // dismissal should still be detected.
        let history = "- [2026-03-30] pr-reviewer: this is not correct behavior\n\
                        - [2026-03-30] NicholaiVogel: nah, it's fine, leave it";
        let reply = build_direct_pushback_reply(history, "pr-reviewer", None);
        assert!(reply.is_some(), "human dismissal should still be detected");
        assert!(reply.unwrap().contains("treating that as intentional"));
    }
}

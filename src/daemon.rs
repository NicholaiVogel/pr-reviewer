use std::collections::HashSet;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::config::{AppConfig, RepoConfig};
use crate::github::client::{GitHubClient, ListPullsResult};
use crate::github::pr;
use crate::github::types::{IssueComment, PullRequest};
use crate::review::engine::{ReviewEngine, ReviewOptions};
use crate::store::db::{Database, WorkItem, WorkItemInsert};

const RATE_LIMIT_TOTAL: u32 = 5000;
const WORK_KIND_REVIEW_PR: &str = "review_pr";
const WORK_KIND_RESPOND_TO_COMMAND: &str = "respond_to_command";
const WORK_KIND_REPAIR_PR: &str = "repair_pr";

pub async fn start(
    config: AppConfig,
    db: Database,
    github: GitHubClient,
    daemonize: bool,
) -> Result<()> {
    if daemonize && std::env::var("PR_REVIEWER_INTERNAL_DAEMON").is_err() {
        let exe = std::env::current_exe().context("failed to resolve current executable")?;
        let child = tokio::process::Command::new(exe)
            .arg("start")
            .env("PR_REVIEWER_INTERNAL_DAEMON", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("failed to spawn daemon process")?;

        println!("daemon started with pid {}", child.id().unwrap_or_default());
        return Ok(());
    }

    let pid_file = AppConfig::pid_file()?;
    write_pid(&pid_file)?;
    let started_at = chrono::Utc::now().to_rfc3339();
    db.set_daemon_started(&started_at).await?;

    let mut engine = ReviewEngine::new(Arc::new(config.clone()), github.clone(), db.clone());
    engine.init().await;
    let worker_limit = config.daemon.max_concurrent_reviews.max(1);
    let semaphore = Arc::new(Semaphore::new(worker_limit));
    let mut workers: JoinSet<()> = JoinSet::new();
    let bot_mention = format!("@{}", config.defaults.bot_name);

    let base_interval = config.daemon.poll_interval_secs.max(1);
    let max_interval = config.daemon.max_poll_interval_secs.max(base_interval);
    let mut current_interval = base_interval;
    let mut no_change_rounds = 0u32;

    let stale_age = config.harness.timeout_secs + 30;
    let _ = db.sweep_stale_claims(stale_age).await?;
    let stale_work = db.sweep_stale_work_items(stale_age).await?;
    if stale_work > 0 {
        tracing::warn!(
            count = stale_work,
            stale_age_secs = stale_age,
            "requeued stale claimed work items"
        );
    }

    // Shared flag so finalization workers can signal that a PR was actually
    // archived (i.e. confirmed closed/merged) back to the main poll loop.
    // swap(false) at the top of each iteration atomically reads-and-resets, so
    // any `Ok(true)` set by a worker that completed during the previous cycle
    // (or during the current one before we evaluate backoff) is captured once.
    let finalization_detected = Arc::new(AtomicBool::new(false));
    let auto_fix_detected = Arc::new(AtomicBool::new(false));

    loop {
        while workers.try_join_next().is_some() {}

        // Seed changes_detected from confirmed finalizations that workers
        // completed since the last time we evaluated backoff.
        let mut changes_detected = finalization_detected.swap(false, Ordering::Acquire)
            || auto_fix_detected.swap(false, Ordering::Acquire);
        let rate_state = github.rate_state();
        let rate_limit_budget = rate_state.limit.unwrap_or(RATE_LIMIT_TOTAL);
        let remaining = rate_state.remaining;
        let reset_epoch = rate_state.reset_epoch;

        if let Some(rem) = remaining {
            if rem <= (rate_limit_budget as f32 * 0.05) as u32 {
                if let Some(reset) = reset_epoch {
                    let now = now_epoch();
                    if reset > now {
                        let wait_secs = reset - now + 1;
                        tokio::time::sleep(Duration::from_secs(wait_secs)).await;
                        continue;
                    }
                }
            }
        }

        for repo_cfg in config.repos.clone() {
            let repo_name = repo_cfg.full_name();
            if repo_cfg.auto_fix.enabled && repo_cfg.auto_fix.scan_default_branch {
                if let Some(permit) = try_acquire_worker_permit(&semaphore) {
                    let config_for_auto_fix = config.clone();
                    let repo_for_auto_fix = repo_cfg.clone();
                    let github_for_auto_fix = github.clone();
                    let db_for_auto_fix = db.clone();
                    let auto_fix_detected = auto_fix_detected.clone();
                    let auto_fix_repo_name = repo_name.clone();
                    workers.spawn(async move {
                        let _permit = permit;
                        match crate::auto_fix::scan_and_open_pr(
                            &config_for_auto_fix,
                            &repo_for_auto_fix,
                            &github_for_auto_fix,
                            &db_for_auto_fix,
                        )
                        .await
                        {
                            Ok(Some(outcome)) => {
                                auto_fix_detected.store(true, Ordering::Release);
                                tracing::info!(
                                    repo = %auto_fix_repo_name,
                                    sha = %outcome.scanned_sha,
                                    pr = ?outcome.pr_number,
                                    changed_files = outcome.changed_files,
                                    duration_secs = outcome.duration_secs,
                                    "auto-fix scan completed"
                                );
                            }
                            Ok(None) => {}
                            Err(err) => {
                                tracing::warn!(
                                    repo = %auto_fix_repo_name,
                                    error = %err,
                                    "auto-fix scan failed"
                                );
                            }
                        }
                    });
                } else {
                    tracing::debug!(
                        repo = %repo_name,
                        "auto-fix scan deferred because worker pool is saturated"
                    );
                }
            }

            let etag = db.get_repo_etag(&repo_name).await?;
            let pulls = github
                .list_open_prs(&repo_cfg.owner, &repo_cfg.name, etag.as_deref())
                .await;

            let pulls = match pulls {
                Ok(value) => value,
                Err(err) => {
                    tracing::warn!(repo = %repo_name, error = %err, "failed to poll repo");
                    continue;
                }
            };

            match pulls {
                ListPullsResult::NotModified { etag } => {
                    db.set_repo_etag(&repo_name, etag.as_deref()).await?;
                }
                ListPullsResult::Updated {
                    prs,
                    etag,
                    complete,
                } => {
                    db.set_repo_etag(&repo_name, etag.as_deref()).await?;
                    let open_pr_numbers: HashSet<u64> = prs.iter().map(|pr| pr.number).collect();
                    // Only skip per-PR finalization probes when list_open_prs
                    // confirmed it walked the complete open-PR set. If pagination
                    // hit the safety cap, stay conservative and ask GitHub for the
                    // current state before archiving anything missing from the set.
                    let can_trust_open_set = complete;
                    for pr in prs {
                        let state = db.get_pr_state(&repo_name, pr.number as i64).await?;
                        let already = state.as_ref().and_then(|s| s.last_reviewed_sha.clone());
                        if already.as_deref() != Some(pr.head.sha.as_str()) {
                            changes_detected |=
                                enqueue_review_work_item(&db, &repo_name, &pr).await?;
                        }

                        let last_comment_check =
                            state.as_ref().and_then(|s| s.last_comment_check.as_deref());
                        if let Ok(review_comments) = github
                            .get_review_comments(
                                &repo_cfg.owner,
                                &repo_cfg.name,
                                pr.number,
                                last_comment_check,
                            )
                            .await
                        {
                            for comment in review_comments {
                                // Skip our own comments
                                if comment
                                    .user
                                    .login
                                    .eq_ignore_ascii_case(config.defaults.bot_name.as_str())
                                {
                                    continue;
                                }
                                if engine
                                    .authenticated_user()
                                    .is_some_and(|u| comment.user.login.eq_ignore_ascii_case(u))
                                {
                                    continue;
                                }
                                // Skip comments from any GitHub Bot account (other
                                // pr-reviewer instances, CI bots, etc.)
                                if comment.user.is_bot() {
                                    tracing::debug!(
                                        repo = %repo_name,
                                        pr = pr.number,
                                        comment_id = comment.id,
                                        login = %comment.user.login,
                                        "skipping comment from bot account"
                                    );
                                    continue;
                                }
                                if !comment.body.contains(&bot_mention) {
                                    continue;
                                }

                                // Circuit breaker: if we've replied to this PR more
                                // than 5 times in the last 10 minutes, stop. This
                                // catches runaway loops that bypass other guards.
                                // Checked BEFORE claim_reply so we don't burn slots
                                // in the reply_log when the breaker is active.
                                const COOLDOWN_WINDOW_SECS: i64 = 600;
                                const COOLDOWN_MAX_REPLIES: i64 = 5;
                                let recent = db
                                    .recent_reply_count(
                                        &repo_name,
                                        pr.number as i64,
                                        COOLDOWN_WINDOW_SECS,
                                    )
                                    .await?;
                                if recent >= COOLDOWN_MAX_REPLIES {
                                    tracing::warn!(
                                        repo = %repo_name,
                                        pr = pr.number,
                                        recent_replies = recent,
                                        "reply cooldown active, skipping comment {}",
                                        comment.id
                                    );
                                    continue;
                                }

                                // Deduplication: skip if we already replied to this comment
                                let claimed = db
                                    .claim_reply(&repo_name, pr.number as i64, comment.id as i64)
                                    .await?;
                                if !claimed {
                                    tracing::debug!(
                                        repo = %repo_name,
                                        pr = pr.number,
                                        comment_id = comment.id,
                                        "skipping already-replied comment"
                                    );
                                    continue;
                                }

                                changes_detected = true;
                                let permit = semaphore.clone().acquire_owned().await?;
                                let engine = engine.clone();
                                let repo_cfg = repo_cfg.clone();
                                let pr_for_reply = pr.clone();
                                let comment_for_reply = comment.clone();
                                workers.spawn(async move {
                                    let _permit = permit;
                                    let res = engine
                                        .reply_to_comment(&repo_cfg, &pr_for_reply, &comment_for_reply)
                                        .await;
                                    if let Err(err) = res {
                                        tracing::error!(repo = %repo_cfg.full_name(), pr = pr_for_reply.number, comment_id = comment_for_reply.id, error = %err, "reply failed");
                                    }
                                });
                            }
                        }

                        if let Ok(issue_comments) = github
                            .get_issue_comments(&repo_cfg.owner, &repo_cfg.name, pr.number)
                            .await
                        {
                            if repo_cfg.auto_fix.enabled
                                && crate::auto_fix::has_matching_repair_marker(
                                    &pr,
                                    &issue_comments,
                                    config.defaults.bot_name.as_str(),
                                    engine.authenticated_user(),
                                )
                            {
                                changes_detected |=
                                    enqueue_repair_work_item(&db, &repo_name, &pr, None).await?;
                            }

                            for comment in issue_comments {
                                if should_skip_command_comment(
                                    &comment,
                                    config.defaults.bot_name.as_str(),
                                    engine.authenticated_user(),
                                ) {
                                    continue;
                                }
                                let Some(command) = parse_pr_reviewer_command(
                                    &comment.body,
                                    &config.defaults.bot_name,
                                ) else {
                                    continue;
                                };
                                if !is_maintainer_comment(&comment) {
                                    tracing::info!(
                                        repo = %repo_name,
                                        pr = pr.number,
                                        comment_id = comment.id,
                                        login = %comment.user.login,
                                        association = ?comment.author_association,
                                        "ignoring pr-reviewer command from non-maintainer"
                                    );
                                    continue;
                                }
                                let claimed = db
                                    .claim_reply(&repo_name, pr.number as i64, comment.id as i64)
                                    .await?;
                                if !claimed {
                                    continue;
                                }

                                let outcome = apply_maintainer_command(
                                    &db, &repo_cfg, &repo_name, &pr, &comment, command,
                                )
                                .await?;
                                changes_detected |= outcome.changed;
                                changes_detected |= enqueue_command_work_item(
                                    &db,
                                    &repo_name,
                                    &pr,
                                    &comment,
                                    command,
                                    outcome.orchestration_note,
                                )
                                .await?;
                            }
                        }

                        // Only update comment check timestamp here. The review worker
                        // updates last_reviewed_sha when the review completes — doing it
                        // here would race with in-flight workers and cause spurious
                        // duplicate claim attempts on every poll cycle.
                        db.update_comment_check(
                            &repo_name,
                            pr.number as i64,
                            &chrono::Utc::now().to_rfc3339(),
                        )
                        .await?;
                    }

                    let pending_finalizations =
                        db.list_prs_pending_finalization(&repo_name).await?;
                    for pending in pending_finalizations {
                        if can_trust_open_set
                            && open_pr_numbers.contains(&(pending.pr_number as u64))
                        {
                            continue;
                        }

                        let permit = semaphore.clone().acquire_owned().await?;
                        let engine = engine.clone();
                        let repo_cfg = repo_cfg.clone();
                        let repo_name = repo_name.clone();
                        let finalization_flag = finalization_detected.clone();
                        // NOTE: do NOT set changes_detected here — we don't
                        // know whether the PR is actually closed until the
                        // worker calls GitHub.  Setting it speculatively would
                        // defeat adaptive backoff for repos with many
                        // pending-finalization entries whose PRs are still open.
                        // Instead the worker signals back via finalization_flag.
                        workers.spawn(async move {
                            let _permit = permit;
                            match engine
                                .finalize_closed_pr_review(
                                    &repo_cfg,
                                    pending.pr_number as u64,
                                    &pending.last_reviewed_sha,
                                )
                                .await
                            {
                                Ok(true) => {
                                    // PR was confirmed closed/merged and archive was written.
                                    // Signal the main loop so adaptive backoff resets.
                                    finalization_flag.store(true, Ordering::Release);
                                    tracing::info!(
                                        repo = %repo_name,
                                        pr = pending.pr_number,
                                        "archived final review transcript and summary"
                                    );
                                }
                                Ok(false) => {
                                    tracing::debug!(
                                        repo = %repo_name,
                                        pr = pending.pr_number,
                                        "skipping final archive because PR is still open"
                                    );
                                }
                                Err(err) => {
                                    tracing::error!(
                                        repo = %repo_name,
                                        pr = pending.pr_number,
                                        error = %err,
                                        "final review archive failed"
                                    );
                                }
                            }
                        });
                    }
                }
            }
        }

        spawn_pending_work_items(
            &config,
            &db,
            &github,
            &engine,
            &semaphore,
            &mut workers,
            &auto_fix_detected,
        )
        .await?;

        while workers.try_join_next().is_some() {}

        let rate_state = github.rate_state();
        let rate_limit_total = rate_state.limit.map(i64::from);
        let rate_remaining = rate_state.remaining.map(i64::from);
        let rate_reset_epoch = rate_state.reset_epoch.map(|epoch| epoch as i64);
        db.set_daemon_status(
            &chrono::Utc::now().to_rfc3339(),
            rate_limit_total,
            rate_remaining,
            rate_reset_epoch,
            workers.len() as i64,
        )
        .await?;

        if changes_detected {
            no_change_rounds = 0;
            current_interval = base_interval;
        } else {
            no_change_rounds = no_change_rounds.saturating_add(1);
            if no_change_rounds >= 5 {
                current_interval = (current_interval * 2).min(max_interval);
            }
        }

        if let Some(rem) = remaining {
            if rem <= (rate_limit_budget as f32 * 0.20) as u32 {
                current_interval = (current_interval * 2).min(max_interval);
            }
        }

        let jitter_secs = rand::rng().random_range(0..=5_u64);
        let sleep_duration = Duration::from_secs(current_interval + jitter_secs);

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("shutdown signal received; waiting for workers");
                break;
            }
            _ = tokio::time::sleep(sleep_duration) => {}
        }
    }

    while workers.join_next().await.is_some() {}
    remove_pid(&pid_file)?;
    Ok(())
}

pub fn stop() -> Result<()> {
    let pid_file = AppConfig::pid_file()?;
    let pid = read_pid(&pid_file)?;
    kill(Pid::from_raw(pid as i32), Signal::SIGTERM)
        .with_context(|| format!("failed to signal daemon pid {pid}"))?;
    remove_pid(&pid_file)?;
    println!("stopped daemon pid {pid}");
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RateLimitSnapshot {
    pub limit: Option<i64>,
    pub remaining: Option<i64>,
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct DaemonSnapshot {
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub uptime_secs: Option<i64>,
    pub last_heartbeat_at: Option<String>,
    pub heartbeat_age_secs: Option<i64>,
    pub active_workers: i64,
    pub pending_work: i64,
    pub claimed_work: i64,
    pub failed_work: i64,
    pub claimed_reviews: i64,
    pub watched_repos: Vec<String>,
    pub rate_limit: RateLimitSnapshot,
}

pub async fn collect_status(db: &Database, config: &AppConfig) -> Result<DaemonSnapshot> {
    let pid_file = AppConfig::pid_file()?;
    let pid = read_pid(&pid_file).ok();
    let running = pid.is_some_and(|pid| is_pid_alive(pid as i32));

    let daemon = db.get_daemon_status().await?;
    let claimed_reviews = db.get_claimed_reviews().await?;
    let work_queue = db.get_work_queue_counts().await?;
    let now = chrono::Utc::now();

    let uptime_secs = if running {
        daemon
            .started_at
            .as_deref()
            .and_then(|started| parse_rfc3339_to_utc(started))
            .map(|started| (now - started).num_seconds().max(0))
    } else {
        None
    };

    let heartbeat_age_secs = daemon
        .last_poll_at
        .as_deref()
        .and_then(parse_rfc3339_to_utc)
        .map(|last| (now - last).num_seconds().max(0));

    let reset_at = daemon
        .rate_reset_epoch
        .and_then(|epoch| chrono::DateTime::from_timestamp(epoch, 0))
        .map(|dt| dt.to_rfc3339());

    Ok(DaemonSnapshot {
        running,
        pid,
        started_at: daemon.started_at,
        uptime_secs,
        last_heartbeat_at: daemon.last_poll_at,
        heartbeat_age_secs,
        active_workers: daemon.active_reviews,
        pending_work: work_queue.pending,
        claimed_work: work_queue.claimed,
        failed_work: work_queue.failed,
        claimed_reviews,
        watched_repos: config.repos.iter().map(|repo| repo.full_name()).collect(),
        rate_limit: RateLimitSnapshot {
            limit: daemon.rate_limit_total,
            remaining: daemon.rate_remaining,
            reset_at,
        },
    })
}

fn try_acquire_worker_permit(semaphore: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    semaphore.clone().try_acquire_owned().ok()
}

pub fn format_status(snapshot: &DaemonSnapshot) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "pr-reviewer {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(out, "self-hosted PR review daemon");
    let _ = writeln!(out);
    let _ = writeln!(out, "Status");
    if snapshot.running {
        let _ = writeln!(out, "  running");
    } else {
        let _ = writeln!(out, "  stopped");
    }
    if let Some(pid) = snapshot.pid {
        if snapshot.running {
            let _ = writeln!(out, "  pid: {pid}");
        } else {
            let _ = writeln!(out, "  last pid: {pid}");
        }
    }
    if let Some(started_at) = snapshot.started_at.as_deref() {
        let _ = writeln!(out, "  started: {started_at}");
    }
    if let Some(uptime_secs) = snapshot.uptime_secs {
        let _ = writeln!(out, "  uptime: {}", format_duration(uptime_secs));
    }
    if let Some(last_heartbeat_at) = snapshot.last_heartbeat_at.as_deref() {
        if let Some(age) = snapshot.heartbeat_age_secs {
            let _ = writeln!(
                out,
                "  last heartbeat: {last_heartbeat_at} ({})",
                format_duration(age)
            );
        } else {
            let _ = writeln!(out, "  last heartbeat: {last_heartbeat_at}");
        }
    } else {
        let _ = writeln!(out, "  last heartbeat: never");
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "Queue");
    let _ = writeln!(out, "  pending work: {}", snapshot.pending_work);
    let _ = writeln!(out, "  claimed work: {}", snapshot.claimed_work);
    let _ = writeln!(out, "  failed work: {}", snapshot.failed_work);
    let _ = writeln!(out, "  claimed: {}", snapshot.claimed_reviews);
    let _ = writeln!(out, "  active workers: {}", snapshot.active_workers);

    let _ = writeln!(out);
    let _ = writeln!(out, "Rate limit");
    match (snapshot.rate_limit.remaining, snapshot.rate_limit.limit) {
        (Some(remaining), Some(limit)) => {
            let _ = writeln!(out, "  remaining: {remaining} / {limit}");
        }
        (Some(remaining), None) => {
            let _ = writeln!(out, "  remaining: {remaining}");
        }
        _ => {
            let _ = writeln!(out, "  remaining: unknown");
        }
    }
    if let Some(reset_at) = snapshot.rate_limit.reset_at.as_deref() {
        let _ = writeln!(out, "  resets at: {reset_at}");
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "Watched repos ({})", snapshot.watched_repos.len());
    if snapshot.watched_repos.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for repo in &snapshot.watched_repos {
            let _ = writeln!(out, "  - {repo}");
        }
    }

    out
}

fn write_pid(path: &PathBuf) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, std::process::id().to_string())
        .with_context(|| format!("failed to write pid file {}", path.display()))?;
    Ok(())
}

fn read_pid(path: &PathBuf) -> Result<u32> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read pid file {}", path.display()))?;
    data.trim()
        .parse::<u32>()
        .map_err(|e| anyhow!("invalid pid file {}: {e}", path.display()))
}

fn parse_rfc3339_to_utc(raw: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

fn format_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{hours}h {minutes:02}m {seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

async fn enqueue_review_work_item(
    db: &Database,
    repo_name: &str,
    pr: &PullRequest,
) -> Result<bool> {
    db.enqueue_work_item(WorkItemInsert {
        repo: repo_name.to_string(),
        pr_number: pr.number as i64,
        head_sha: pr.head.sha.clone(),
        task_kind: WORK_KIND_REVIEW_PR.to_string(),
        dedupe_key: format!("review:{repo_name}:{}:{}", pr.number, pr.head.sha),
        payload: "{}".to_string(),
        source_comment_id: None,
    })
    .await
}

async fn enqueue_repair_work_item(
    db: &Database,
    repo_name: &str,
    pr: &PullRequest,
    payload: Option<String>,
) -> Result<bool> {
    db.enqueue_work_item(WorkItemInsert {
        repo: repo_name.to_string(),
        pr_number: pr.number as i64,
        head_sha: pr.head.sha.clone(),
        task_kind: WORK_KIND_REPAIR_PR.to_string(),
        dedupe_key: format!("repair:{repo_name}:{}:{}", pr.number, pr.head.sha),
        payload: payload.unwrap_or_else(|| "{}".to_string()),
        source_comment_id: None,
    })
    .await
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CommandWorkPayload {
    command: String,
    source_comment_id: u64,
    source_author: String,
    source_body: String,
    source_created_at: String,
    author_association: Option<String>,
    orchestration_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RepairWorkPayload {
    source: String,
    source_comment_id: Option<u64>,
    source_author: Option<String>,
    source_body: Option<String>,
}

async fn enqueue_command_work_item(
    db: &Database,
    repo_name: &str,
    pr: &PullRequest,
    comment: &IssueComment,
    command: MaintainerCommand,
    orchestration_note: Option<String>,
) -> Result<bool> {
    let payload = serde_json::to_string(&CommandWorkPayload {
        command: command.as_str().to_string(),
        source_comment_id: comment.id,
        source_author: comment.user.login.clone(),
        source_body: comment.body.clone(),
        source_created_at: comment.created_at.clone(),
        author_association: comment.author_association.clone(),
        orchestration_note,
    })?;

    db.enqueue_work_item(WorkItemInsert {
        repo: repo_name.to_string(),
        pr_number: pr.number as i64,
        head_sha: pr.head.sha.clone(),
        task_kind: WORK_KIND_RESPOND_TO_COMMAND.to_string(),
        dedupe_key: format!("command:{repo_name}:{}:{}", pr.number, comment.id),
        payload,
        source_comment_id: Some(comment.id as i64),
    })
    .await
}

#[derive(Debug, Clone, Default)]
struct CommandActionOutcome {
    changed: bool,
    orchestration_note: Option<String>,
}

async fn apply_maintainer_command(
    db: &Database,
    repo_cfg: &RepoConfig,
    repo_name: &str,
    pr: &PullRequest,
    comment: &IssueComment,
    command: MaintainerCommand,
) -> Result<CommandActionOutcome> {
    match command {
        MaintainerCommand::Status | MaintainerCommand::Explain => {
            Ok(CommandActionOutcome::default())
        }
        MaintainerCommand::Fix => {
            if !repo_cfg.auto_fix.enabled {
                return Ok(CommandActionOutcome {
                    changed: false,
                    orchestration_note: Some(
                        "Repair was requested, but auto_fix.enabled is false for this repository."
                            .to_string(),
                    ),
                });
            }

            let payload = serde_json::to_string(&RepairWorkPayload {
                source: "maintainer_command".to_string(),
                source_comment_id: Some(comment.id),
                source_author: Some(comment.user.login.clone()),
                source_body: Some(comment.body.clone()),
            })?;
            let queued = enqueue_repair_work_item(db, repo_name, pr, Some(payload)).await?;
            let note = if queued {
                format!(
                    "Queued repair_pr for {repo_name}#{} at {} from maintainer command comment {}.",
                    pr.number,
                    short_sha(&pr.head.sha),
                    comment.id
                )
            } else {
                format!(
                    "A repair_pr task already exists for {repo_name}#{} at {}.",
                    pr.number,
                    short_sha(&pr.head.sha)
                )
            };
            Ok(CommandActionOutcome {
                changed: queued,
                orchestration_note: Some(note),
            })
        }
        MaintainerCommand::Retry { id } => {
            let retried = if let Some(id) = id {
                match db.get_work_item(id).await? {
                    Some(item)
                        if item.repo.eq_ignore_ascii_case(repo_name)
                            && item.pr_number == pr.number as i64 =>
                    {
                        if db.retry_work_item(id).await? {
                            Some(id)
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            } else {
                db.retry_latest_failed_work_item_for_pr(repo_name, pr.number as i64)
                    .await?
            };

            let note = retried
                .map(|id| format!("Requeued failed work item #{id}."))
                .unwrap_or_else(|| {
                    "No matching failed work item was available to retry.".to_string()
                });
            Ok(CommandActionOutcome {
                changed: retried.is_some(),
                orchestration_note: Some(note),
            })
        }
        MaintainerCommand::Cancel { id } => {
            let canceled = if let Some(id) = id {
                match db.get_work_item(id).await? {
                    Some(item)
                        if item.repo.eq_ignore_ascii_case(repo_name)
                            && item.pr_number == pr.number as i64 =>
                    {
                        if db.cancel_work_item(id).await? {
                            1
                        } else {
                            0
                        }
                    }
                    _ => 0,
                }
            } else {
                db.cancel_work_items_for_pr(repo_name, pr.number as i64)
                    .await?
            };
            let note = if canceled > 0 {
                format!("Canceled {canceled} pending or failed work item(s) for this PR.")
            } else {
                "No cancelable pending or failed work items matched this PR.".to_string()
            };
            Ok(CommandActionOutcome {
                changed: canceled > 0,
                orchestration_note: Some(note),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct WorkItemOutcome {
    auto_fix_changed: bool,
}

async fn spawn_pending_work_items(
    config: &AppConfig,
    db: &Database,
    github: &GitHubClient,
    engine: &ReviewEngine,
    semaphore: &Arc<Semaphore>,
    workers: &mut JoinSet<()>,
    auto_fix_detected: &Arc<AtomicBool>,
) -> Result<()> {
    loop {
        let Some(permit) = try_acquire_worker_permit(semaphore) else {
            break;
        };
        let mut items = db.claim_pending_work_items(1).await?;
        let Some(item) = items.pop() else {
            drop(permit);
            break;
        };

        let config = config.clone();
        let db = db.clone();
        let github = github.clone();
        let engine = engine.clone();
        let auto_fix_detected = auto_fix_detected.clone();
        workers.spawn(async move {
            let _permit = permit;
            let item_id = item.id;
            let repo = item.repo.clone();
            let pr_number = item.pr_number;
            let task_kind = item.task_kind.clone();
            let result = process_work_item(&config, &db, &github, &engine, item).await;
            match result {
                Ok(outcome) => {
                    if outcome.auto_fix_changed {
                        auto_fix_detected.store(true, Ordering::Release);
                    }
                    if let Err(err) = db.complete_work_item(item_id).await {
                        tracing::warn!(
                            repo = %repo,
                            pr = pr_number,
                            task_kind = %task_kind,
                            error = %err,
                            "failed to mark work item completed"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        repo = %repo,
                        pr = pr_number,
                        task_kind = %task_kind,
                        error = %err,
                        "work item failed"
                    );
                    let _ = db.fail_work_item(item_id, &format!("{err:#}")).await;
                }
            }
        });
    }
    Ok(())
}

async fn process_work_item(
    config: &AppConfig,
    db: &Database,
    github: &GitHubClient,
    engine: &ReviewEngine,
    item: WorkItem,
) -> Result<WorkItemOutcome> {
    let repo_cfg = config
        .repos
        .iter()
        .find(|repo| repo.full_name().eq_ignore_ascii_case(&item.repo))
        .cloned()
        .ok_or_else(|| anyhow!("work item repo {} is no longer configured", item.repo))?;

    match item.task_kind.as_str() {
        WORK_KIND_REVIEW_PR => {
            let pr_data = pr::get_pull_request(
                github,
                &repo_cfg.owner,
                &repo_cfg.name,
                item.pr_number as u64,
            )
            .await?;
            if pr_data.head.sha != item.head_sha {
                tracing::debug!(
                    repo = %item.repo,
                    pr = item.pr_number,
                    queued_sha = %item.head_sha,
                    current_sha = %pr_data.head.sha,
                    "skipping stale review work item"
                );
                return Ok(WorkItemOutcome::default());
            }
            engine
                .review_existing_pr(&repo_cfg, &pr_data, ReviewOptions::default())
                .await?;
            Ok(WorkItemOutcome::default())
        }
        WORK_KIND_RESPOND_TO_COMMAND => {
            let payload: CommandWorkPayload = serde_json::from_str(&item.payload)
                .with_context(|| format!("invalid command work payload for item {}", item.id))?;
            let pr_data = pr::get_pull_request(
                github,
                &repo_cfg.owner,
                &repo_cfg.name,
                item.pr_number as u64,
            )
            .await?;
            let last_reviewed_sha = db
                .get_pr_state(&item.repo, item.pr_number)
                .await?
                .and_then(|state| state.last_reviewed_sha);
            engine
                .respond_to_maintainer_command(
                    &repo_cfg,
                    &pr_data,
                    &payload.command,
                    payload.source_comment_id,
                    &payload.source_author,
                    &payload.source_body,
                    payload.orchestration_note.as_deref(),
                    last_reviewed_sha.as_deref(),
                )
                .await?;
            Ok(WorkItemOutcome::default())
        }
        WORK_KIND_REPAIR_PR => {
            let repair_payload = serde_json::from_str::<RepairWorkPayload>(&item.payload).ok();
            let pr_data = pr::get_pull_request(
                github,
                &repo_cfg.owner,
                &repo_cfg.name,
                item.pr_number as u64,
            )
            .await?;
            if pr_data.head.sha != item.head_sha {
                tracing::debug!(
                    repo = %item.repo,
                    pr = item.pr_number,
                    queued_sha = %item.head_sha,
                    current_sha = %pr_data.head.sha,
                    "skipping stale repair work item"
                );
                return Ok(WorkItemOutcome::default());
            }
            let issue_comments = github
                .get_issue_comments(&repo_cfg.owner, &repo_cfg.name, pr_data.number)
                .await?;
            let outcome = crate::auto_fix::repair_marked_pr(
                config,
                &repo_cfg,
                github,
                db,
                &pr_data,
                &issue_comments,
                engine.authenticated_user(),
                repair_payload
                    .as_ref()
                    .and_then(|payload| payload.source_body.as_deref()),
            )
            .await?;
            Ok(WorkItemOutcome {
                auto_fix_changed: outcome.is_some(),
            })
        }
        _ => Err(anyhow!("unknown work item kind {}", item.task_kind)),
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MaintainerCommand {
    Status,
    Explain,
    Fix,
    Retry { id: Option<i64> },
    Cancel { id: Option<i64> },
}

fn should_skip_command_comment(
    comment: &IssueComment,
    bot_name: &str,
    authenticated_user: Option<&str>,
) -> bool {
    if comment.user.login.eq_ignore_ascii_case(bot_name) {
        return true;
    }
    if authenticated_user.is_some_and(|login| comment.user.login.eq_ignore_ascii_case(login)) {
        return true;
    }
    comment.user.is_bot()
}

fn is_maintainer_comment(comment: &IssueComment) -> bool {
    matches!(
        comment
            .author_association
            .as_deref()
            .unwrap_or_default()
            .to_ascii_uppercase()
            .as_str(),
        "OWNER" | "MEMBER" | "COLLABORATOR"
    )
}

fn parse_pr_reviewer_command(body: &str, bot_name: &str) -> Option<MaintainerCommand> {
    let mention = format!("@{}", bot_name.to_ascii_lowercase());
    let bot_suffix = format!("{mention}[bot]");
    for line in body.lines() {
        let lower = line.trim_start().to_ascii_lowercase();
        let command_text = if let Some(rest) = lower.strip_prefix(&bot_suffix) {
            rest
        } else if let Some(rest) = lower.strip_prefix(&mention) {
            rest
        } else {
            continue;
        };
        if command_text
            .chars()
            .next()
            .is_some_and(|ch| !ch.is_whitespace() && !matches!(ch, ':' | ',' | '-'))
        {
            continue;
        }
        let command = command_text
            .trim_start_matches(|ch: char| ch.is_whitespace() || matches!(ch, ':' | ',' | '-'))
            .trim();
        let (verb, rest) = split_command_verb(command);
        if verb == "status" {
            return Some(MaintainerCommand::Status);
        }
        if verb == "explain" || verb == "why" {
            return Some(MaintainerCommand::Explain);
        }
        if verb == "fix" {
            return Some(MaintainerCommand::Fix);
        }
        if verb == "retry" {
            return Some(MaintainerCommand::Retry {
                id: parse_command_id(rest),
            });
        }
        if verb == "cancel" {
            return Some(MaintainerCommand::Cancel {
                id: parse_command_id(rest),
            });
        }
    }
    None
}

fn split_command_verb(command: &str) -> (&str, &str) {
    let trimmed = command.trim_start();
    let split_at = trimmed
        .find(|ch: char| ch.is_whitespace() || matches!(ch, ':' | ',' | '#'))
        .unwrap_or(trimmed.len());
    let (verb, rest) = trimmed.split_at(split_at);
    (verb, rest)
}

fn parse_command_id(rest: &str) -> Option<i64> {
    rest.trim_start_matches(|ch: char| ch.is_whitespace() || matches!(ch, '#' | ':' | ','))
        .split_whitespace()
        .next()
        .and_then(|raw| raw.trim_start_matches('#').parse::<i64>().ok())
}

impl MaintainerCommand {
    fn as_str(self) -> &'static str {
        match self {
            MaintainerCommand::Status => "status",
            MaintainerCommand::Explain => "explain",
            MaintainerCommand::Fix => "fix",
            MaintainerCommand::Retry { .. } => "retry",
            MaintainerCommand::Cancel { .. } => "cancel",
        }
    }
}

fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(8)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::types::User;

    fn test_user(login: &str) -> User {
        User {
            login: login.to_string(),
            account_type: None,
        }
    }

    fn test_issue_comment(association: Option<&str>) -> IssueComment {
        IssueComment {
            id: 1,
            body: "@pr-reviewer status".to_string(),
            user: test_user("maintainer"),
            author_association: association.map(ToString::to_string),
            created_at: "2026-04-29T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn format_status_includes_key_fields() {
        let snapshot = DaemonSnapshot {
            running: true,
            pid: Some(1234),
            started_at: Some("2026-03-23T06:00:00Z".to_string()),
            uptime_secs: Some(3723),
            last_heartbeat_at: Some("2026-03-23T06:14:30Z".to_string()),
            heartbeat_age_secs: Some(30),
            active_workers: 2,
            pending_work: 3,
            claimed_work: 1,
            failed_work: 0,
            claimed_reviews: 5,
            watched_repos: vec!["owner/repo".to_string()],
            rate_limit: RateLimitSnapshot {
                limit: Some(5000),
                remaining: Some(4321),
                reset_at: Some("2026-03-23T07:00:00Z".to_string()),
            },
        };

        let out = format_status(&snapshot);

        assert!(out.contains("pr-reviewer"));
        assert!(out.contains("running"));
        assert!(out.contains("pid: 1234"));
        assert!(out.contains("uptime: 1h 02m 03s"));
        assert!(out.contains("last heartbeat: 2026-03-23T06:14:30Z (30s)"));
        assert!(out.contains("pending work: 3"));
        assert!(out.contains("claimed work: 1"));
        assert!(out.contains("claimed: 5"));
        assert!(out.contains("remaining: 4321 / 5000"));
        assert!(out.contains("owner/repo"));
    }

    #[test]
    fn auto_fix_permit_acquisition_is_non_blocking_when_saturated() {
        let semaphore = Arc::new(Semaphore::new(1));
        let _held = try_acquire_worker_permit(&semaphore).expect("first permit");

        assert!(try_acquire_worker_permit(&semaphore).is_none());
    }

    #[test]
    fn parses_pr_reviewer_status_command() {
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer status please", "pr-reviewer"),
            Some(MaintainerCommand::Status)
        );
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer explain", "pr-reviewer"),
            Some(MaintainerCommand::Explain)
        );
        assert_eq!(
            parse_pr_reviewer_command("plain status", "pr-reviewer"),
            None
        );
    }

    #[test]
    fn parses_pr_reviewer_queue_control_commands() {
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer fix", "pr-reviewer"),
            Some(MaintainerCommand::Fix)
        );
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer retry #123", "pr-reviewer"),
            Some(MaintainerCommand::Retry { id: Some(123) })
        );
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer cancel", "pr-reviewer"),
            Some(MaintainerCommand::Cancel { id: None })
        );
    }

    #[test]
    fn pr_reviewer_commands_require_explicit_command_boundaries() {
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer fixture setup", "pr-reviewer"),
            None
        );
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer fixing this manually", "pr-reviewer"),
            None
        );
        assert_eq!(
            parse_pr_reviewer_command("example: @pr-reviewer fix", "pr-reviewer"),
            None
        );
        assert_eq!(
            parse_pr_reviewer_command("@pr-reviewer: retry #123", "pr-reviewer"),
            Some(MaintainerCommand::Retry { id: Some(123) })
        );
    }

    #[test]
    fn command_authorization_uses_author_association() {
        assert!(is_maintainer_comment(&test_issue_comment(Some("OWNER"))));
        assert!(is_maintainer_comment(&test_issue_comment(Some(
            "COLLABORATOR"
        ))));
        assert!(!is_maintainer_comment(&test_issue_comment(Some(
            "CONTRIBUTOR"
        ))));
        assert!(!is_maintainer_comment(&test_issue_comment(None)));
    }
}

fn remove_pid(path: &PathBuf) -> Result<()> {
    if path.exists() {
        std::fs::remove_file(path)
            .with_context(|| format!("failed to remove pid file {}", path.display()))?;
    }
    Ok(())
}

fn is_pid_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

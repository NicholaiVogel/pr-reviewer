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
use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::config::AppConfig;
use crate::github::client::{GitHubClient, ListPullsResult};
use crate::review::engine::{ReviewEngine, ReviewOptions};
use crate::store::db::Database;

const RATE_LIMIT_TOTAL: u32 = 5000;

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
    let semaphore = Arc::new(Semaphore::new(config.daemon.max_concurrent_reviews));
    let mut workers: JoinSet<()> = JoinSet::new();
    let bot_mention = format!("@{}", config.defaults.bot_name);

    let base_interval = config.daemon.poll_interval_secs.max(1);
    let max_interval = config.daemon.max_poll_interval_secs.max(base_interval);
    let mut current_interval = base_interval;
    let mut no_change_rounds = 0u32;

    let stale_age = config.harness.timeout_secs + 30;
    let _ = db.sweep_stale_claims(stale_age).await?;

    // Shared flag so finalization workers can signal that a PR was actually
    // archived (i.e. confirmed closed/merged) back to the main poll loop.
    // swap(false) at the top of each iteration atomically reads-and-resets, so
    // any `Ok(true)` set by a worker that completed during the previous cycle
    // (or during the current one before we evaluate backoff) is captured once.
    let finalization_detected = Arc::new(AtomicBool::new(false));

    loop {
        while workers.try_join_next().is_some() {}

        // Seed changes_detected from confirmed finalizations that workers
        // completed since the last time we evaluated backoff.
        let mut changes_detected = finalization_detected.swap(false, Ordering::Acquire);
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
                            changes_detected = true;

                            let permit = semaphore.clone().acquire_owned().await?;
                            let engine = engine.clone();
                            let repo_cfg = repo_cfg.clone();
                            let pr_for_review = pr.clone();
                            workers.spawn(async move {
                                let _permit = permit;
                                let res = engine
                                    .review_existing_pr(
                                        &repo_cfg,
                                        &pr_for_review,
                                        ReviewOptions::default(),
                                    )
                                    .await;
                                if let Err(err) = res {
                                    tracing::error!(repo = %repo_cfg.full_name(), pr = pr_for_review.number, error = %err, "review failed");
                                }
                            });
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
        claimed_reviews,
        watched_repos: config.repos.iter().map(|repo| repo.full_name()).collect(),
        rate_limit: RateLimitSnapshot {
            limit: daemon.rate_limit_total,
            remaining: daemon.rate_remaining,
            reset_at,
        },
    })
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(out.contains("claimed: 5"));
        assert!(out.contains("remaining: 4321 / 5000"));
        assert!(out.contains("owner/repo"));
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

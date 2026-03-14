use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use rand::Rng;
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

    loop {
        while workers.try_join_next().is_some() {}

        let mut changes_detected = false;
        let rate_state = github.rate_state();
        let remaining = rate_state.remaining;
        let reset_epoch = rate_state.reset_epoch;

        if let Some(rem) = remaining {
            if rem <= (RATE_LIMIT_TOTAL as f32 * 0.05) as u32 {
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
                ListPullsResult::Updated { prs, etag } => {
                    db.set_repo_etag(&repo_name, etag.as_deref()).await?;
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
                                if comment
                                    .user
                                    .login
                                    .eq_ignore_ascii_case(config.defaults.bot_name.as_str())
                                {
                                    continue;
                                }
                                if !comment.body.contains(&bot_mention) {
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
                }
            }
        }

        while workers.try_join_next().is_some() {}

        let rate_remaining = github.rate_state().remaining.map(i64::from);
        db.set_daemon_status(
            &chrono::Utc::now().to_rfc3339(),
            rate_remaining,
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
            if rem <= (RATE_LIMIT_TOTAL as f32 * 0.20) as u32 {
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

pub async fn status(db: &Database) -> Result<String> {
    let pid_file = AppConfig::pid_file()?;
    let running = match read_pid(&pid_file) {
        Ok(pid) => is_pid_alive(pid as i32),
        Err(_) => false,
    };

    let daemon = db.get_daemon_status().await?;

    let mut out = String::new();
    out.push_str(&format!(
        "Daemon running: {}\n",
        if running { "yes" } else { "no" }
    ));
    out.push_str(&format!("Last poll: {:?}\n", daemon.last_poll_at));
    out.push_str(&format!("Rate remaining: {:?}\n", daemon.rate_remaining));
    out.push_str(&format!("Active reviews: {}\n", daemon.active_reviews));
    Ok(out)
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

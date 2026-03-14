use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone)]
pub struct Database {
    path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PrState {
    pub last_reviewed_sha: Option<String>,
    pub last_comment_check: Option<String>,
    pub review_count: i64,
}

#[derive(Debug, Clone)]
pub struct ReviewClaim {
    pub dedupe_key: String,
    pub repo: String,
    pub pr_number: i64,
    pub sha: String,
    pub harness: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewLogEntry {
    pub id: i64,
    pub repo: String,
    pub pr_number: i64,
    pub sha: String,
    pub harness: String,
    pub model: Option<String>,
    pub status: String,
    pub comments_posted: Option<i64>,
    pub verdict: Option<String>,
    pub duration_secs: Option<f64>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LogsFilter {
    pub repo: Option<String>,
    pub since: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
    pub avg_duration_secs: f64,
    pub by_repo: Vec<(String, i64)>,
    pub by_model: Vec<(String, i64, f64)>,
    pub verdicts: Vec<(String, i64)>,
}

#[derive(Debug, Clone, Default)]
pub struct DaemonStatus {
    pub last_poll_at: Option<String>,
    pub rate_remaining: Option<i64>,
    pub active_reviews: i64,
}

impl Database {
    pub async fn new(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create db dir {}", parent.display()))?;
        }

        let db = Self { path };
        db.migrate().await?;
        Ok(db)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    pub async fn migrate(&self) -> Result<()> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS schema_version (
                    version INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS pr_state (
                    repo TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    last_reviewed_sha TEXT,
                    last_comment_check TEXT,
                    review_count INTEGER DEFAULT 0,
                    PRIMARY KEY (repo, pr_number)
                );

                CREATE TABLE IF NOT EXISTS repo_state (
                    repo TEXT PRIMARY KEY,
                    etag TEXT
                );

                CREATE TABLE IF NOT EXISTS review_log (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    dedupe_key TEXT UNIQUE NOT NULL,
                    repo TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    sha TEXT NOT NULL,
                    harness TEXT NOT NULL,
                    model TEXT,
                    status TEXT NOT NULL DEFAULT 'claimed',
                    comments_posted INTEGER,
                    verdict TEXT,
                    duration_secs REAL,
                    files_reviewed INTEGER,
                    diff_lines INTEGER,
                    error_message TEXT,
                    created_at TEXT DEFAULT (datetime('now')),
                    completed_at TEXT
                );

                CREATE TABLE IF NOT EXISTS daemon_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    last_poll_at TEXT,
                    rate_remaining INTEGER,
                    active_reviews INTEGER DEFAULT 0
                );

                INSERT OR IGNORE INTO daemon_state (id, last_poll_at, rate_remaining, active_reviews)
                VALUES (1, NULL, NULL, 0);
                "#,
            )?;

            let count: i64 = conn.query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))?;
            if count == 0 {
                conn.execute("INSERT INTO schema_version(version) VALUES (1)", [])?;
            }

            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn claim_review(&self, claim: ReviewClaim) -> Result<bool> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            let result = conn.execute(
                "INSERT INTO review_log (dedupe_key, repo, pr_number, sha, harness, model, status) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'claimed')",
                params![
                    claim.dedupe_key,
                    claim.repo,
                    claim.pr_number,
                    claim.sha,
                    claim.harness,
                    claim.model
                ],
            );

            match result {
                Ok(_) => Ok(true),
                Err(rusqlite::Error::SqliteFailure(err, _)) if err.extended_code == 2067 => Ok(false),
                Err(e) => Err(anyhow::Error::new(e)),
            }
        })
        .await?
    }

    pub async fn complete_review(
        &self,
        dedupe_key: &str,
        comments_posted: i64,
        verdict: Option<&str>,
        duration_secs: f64,
        files_reviewed: i64,
        diff_lines: i64,
    ) -> Result<()> {
        let path = self.path.clone();
        let dedupe_key = dedupe_key.to_string();
        let verdict = verdict.map(ToString::to_string);

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute(
                "UPDATE review_log SET status='completed', comments_posted=?2, verdict=?3, duration_secs=?4, files_reviewed=?5, diff_lines=?6, completed_at=datetime('now') WHERE dedupe_key=?1",
                params![dedupe_key, comments_posted, verdict, duration_secs, files_reviewed, diff_lines],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn fail_review(
        &self,
        dedupe_key: &str,
        message: &str,
        duration_secs: f64,
    ) -> Result<()> {
        let path = self.path.clone();
        let dedupe_key = dedupe_key.to_string();
        let message = message.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute(
                "UPDATE review_log SET status='failed', error_message=?2, duration_secs=?3, completed_at=datetime('now') WHERE dedupe_key=?1",
                params![dedupe_key, message, duration_secs],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn get_pr_state(&self, repo: &str, pr_number: i64) -> Result<Option<PrState>> {
        let path = self.path.clone();
        let repo = repo.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            let row = conn
                .query_row(
                    "SELECT last_reviewed_sha, last_comment_check, review_count FROM pr_state WHERE repo=?1 AND pr_number=?2",
                    params![repo, pr_number],
                    |r| {
                        Ok(PrState {
                            last_reviewed_sha: r.get(0)?,
                            last_comment_check: r.get(1)?,
                            review_count: r.get(2)?,
                        })
                    },
                )
                .optional()?;
            Ok::<Option<PrState>, anyhow::Error>(row)
        })
        .await?
    }

    pub async fn upsert_pr_state(
        &self,
        repo: &str,
        pr_number: i64,
        last_reviewed_sha: Option<&str>,
        last_comment_check: Option<&str>,
    ) -> Result<()> {
        let path = self.path.clone();
        let repo = repo.to_string();
        let last_reviewed_sha = last_reviewed_sha.map(ToString::to_string);
        let last_comment_check = last_comment_check.map(ToString::to_string);

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute(
                r#"
                INSERT INTO pr_state (repo, pr_number, last_reviewed_sha, last_comment_check, review_count)
                VALUES (?1, ?2, ?3, ?4, CASE WHEN ?3 IS NULL THEN 0 ELSE 1 END)
                ON CONFLICT(repo, pr_number) DO UPDATE SET
                  last_reviewed_sha=excluded.last_reviewed_sha,
                  last_comment_check=COALESCE(excluded.last_comment_check, pr_state.last_comment_check),
                  review_count=CASE
                    WHEN excluded.last_reviewed_sha IS NOT NULL
                      AND excluded.last_reviewed_sha != pr_state.last_reviewed_sha
                    THEN pr_state.review_count + 1
                    ELSE pr_state.review_count
                  END
                "#,
                params![repo, pr_number, last_reviewed_sha, last_comment_check],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn update_comment_check(
        &self,
        repo: &str,
        pr_number: i64,
        last_comment_check: &str,
    ) -> Result<()> {
        let path = self.path.clone();
        let repo = repo.to_string();
        let last_comment_check = last_comment_check.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute(
                r#"
                INSERT INTO pr_state (repo, pr_number, last_comment_check, review_count)
                VALUES (?1, ?2, ?3, 0)
                ON CONFLICT(repo, pr_number) DO UPDATE SET
                  last_comment_check=excluded.last_comment_check
                "#,
                params![repo, pr_number, last_comment_check],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn set_repo_etag(&self, repo: &str, etag: Option<&str>) -> Result<()> {
        let path = self.path.clone();
        let repo = repo.to_string();
        let etag = etag.map(ToString::to_string);

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute(
                "INSERT INTO repo_state(repo, etag) VALUES (?1, ?2) ON CONFLICT(repo) DO UPDATE SET etag=excluded.etag",
                params![repo, etag],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn get_repo_etag(&self, repo: &str) -> Result<Option<String>> {
        let path = self.path.clone();
        let repo = repo.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            let etag = conn
                .query_row(
                    "SELECT etag FROM repo_state WHERE repo=?1",
                    params![repo],
                    |r| r.get(0),
                )
                .optional()?;
            Ok::<Option<String>, anyhow::Error>(etag)
        })
        .await?
    }

    pub async fn sweep_stale_claims(&self, max_age_secs: u64) -> Result<u64> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            let changed = conn.execute(
                "UPDATE review_log SET status='failed', error_message='stale claim swept', completed_at=datetime('now') WHERE status='claimed' AND created_at < datetime('now', ?1)",
                params![format!("-{} seconds", max_age_secs)],
            )?;
            Ok::<u64, anyhow::Error>(changed as u64)
        })
        .await?
    }

    pub async fn list_logs(&self, filter: LogsFilter) -> Result<Vec<ReviewLogEntry>> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;

            let mut sql = String::from(
                "SELECT id, repo, pr_number, sha, harness, model, status, comments_posted, verdict, duration_secs, error_message, created_at, completed_at FROM review_log WHERE 1=1",
            );
            let mut args: Vec<String> = Vec::new();

            if let Some(repo) = filter.repo.as_deref() {
                sql.push_str(" AND repo = ?");
                args.push(repo.to_string());
            }
            if let Some(since) = filter.since.as_deref() {
                sql.push_str(" AND created_at >= ?");
                args.push(since.to_string());
            }
            if let Some(harness) = filter.harness.as_deref() {
                sql.push_str(" AND harness = ?");
                args.push(harness.to_string());
            }
            if let Some(model) = filter.model.as_deref() {
                sql.push_str(" AND model = ?");
                args.push(model.to_string());
            }

            sql.push_str(" ORDER BY created_at DESC LIMIT ?");
            let limit = if filter.limit == 0 { 100 } else { filter.limit };
            args.push(limit.to_string());

            let mut stmt = conn.prepare(&sql)?;
            let params = rusqlite::params_from_iter(args.iter());
            let rows = stmt.query_map(params, |row| {
                Ok(ReviewLogEntry {
                    id: row.get(0)?,
                    repo: row.get(1)?,
                    pr_number: row.get(2)?,
                    sha: row.get(3)?,
                    harness: row.get(4)?,
                    model: row.get(5)?,
                    status: row.get(6)?,
                    comments_posted: row.get(7)?,
                    verdict: row.get(8)?,
                    duration_secs: row.get(9)?,
                    error_message: row.get(10)?,
                    created_at: row.get(11)?,
                    completed_at: row.get(12)?,
                })
            })?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok::<Vec<ReviewLogEntry>, anyhow::Error>(out)
        })
        .await?
    }

    pub async fn usage_stats(&self, since: Option<&str>, repo: Option<&str>) -> Result<UsageStats> {
        let path = self.path.clone();
        let since = since.map(ToString::to_string);
        let repo = repo.map(ToString::to_string);

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            let (where_clause, values) = build_where_clause(since.as_deref(), repo.as_deref());

            let mut stats = UsageStats::default();
            let mut args = values.clone();

            let mut stmt = conn.prepare(&format!(
                "SELECT COUNT(*), SUM(CASE WHEN status='completed' THEN 1 ELSE 0 END), SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END), AVG(duration_secs) FROM review_log {where_clause}"
            ))?;
            let row = stmt.query_row(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                ))
            })?;
            stats.total = row.0;
            stats.completed = row.1;
            stats.failed = row.2;
            stats.avg_duration_secs = row.3;

            let mut by_repo = conn.prepare(&format!(
                "SELECT repo, COUNT(*) FROM review_log {where_clause} GROUP BY repo ORDER BY COUNT(*) DESC"
            ))?;
            args = values.clone();
            let rows = by_repo.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                stats.by_repo.push(row?);
            }

            let mut by_model = conn.prepare(&format!(
                "SELECT COALESCE(model, 'unknown'), COUNT(*), AVG(duration_secs) FROM review_log {where_clause} GROUP BY COALESCE(model, 'unknown') ORDER BY COUNT(*) DESC"
            ))?;
            args = values.clone();
            let rows = by_model.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                ))
            })?;
            for row in rows {
                stats.by_model.push(row?);
            }

            let mut verdicts = conn.prepare(&format!(
                "SELECT COALESCE(verdict, 'unknown'), COUNT(*) FROM review_log {where_clause} GROUP BY COALESCE(verdict, 'unknown') ORDER BY COUNT(*) DESC"
            ))?;
            args = values;
            let rows = verdicts.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                stats.verdicts.push(row?);
            }

            Ok::<UsageStats, anyhow::Error>(stats)
        })
        .await?
    }

    pub async fn set_daemon_status(
        &self,
        last_poll_at: &str,
        rate_remaining: Option<i64>,
        active_reviews: i64,
    ) -> Result<()> {
        let path = self.path.clone();
        let last_poll_at = last_poll_at.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            conn.execute(
                "UPDATE daemon_state SET last_poll_at=?1, rate_remaining=?2, active_reviews=?3 WHERE id=1",
                params![last_poll_at, rate_remaining, active_reviews],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn get_daemon_status(&self) -> Result<DaemonStatus> {
        let path = self.path.clone();

        tokio::task::spawn_blocking(move || {
            let conn = open_conn(&path)?;
            let row = conn.query_row(
                "SELECT last_poll_at, rate_remaining, active_reviews FROM daemon_state WHERE id=1",
                [],
                |r| {
                    Ok(DaemonStatus {
                        last_poll_at: r.get(0)?,
                        rate_remaining: r.get(1)?,
                        active_reviews: r.get(2)?,
                    })
                },
            )?;
            Ok::<DaemonStatus, anyhow::Error>(row)
        })
        .await?
    }
}

fn build_where_clause(since: Option<&str>, repo: Option<&str>) -> (String, Vec<String>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut values = Vec::new();

    if let Some(since) = since {
        clauses.push("created_at >= ?");
        values.push(since.to_string());
    }
    if let Some(repo) = repo {
        clauses.push("repo = ?");
        values.push(repo.to_string());
    }

    if clauses.is_empty() {
        ("".to_string(), values)
    } else {
        (format!("WHERE {}", clauses.join(" AND ")), values)
    }
}

fn open_conn(path: &PathBuf) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open sqlite db {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

pub fn dedupe_key(repo: &str, pr_number: u64, sha: &str, harness: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(b":");
    hasher.update(pr_number.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(sha.as_bytes());
    hasher.update(b":");
    hasher.update(harness.as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn format_stats(stats: &UsageStats) -> String {
    let mut out = String::new();
    out.push_str("Usage Summary\n");
    out.push_str("----------------------------------------\n");
    out.push_str(&format!("Reviews:   {} total\n", stats.total));
    out.push_str(&format!("Completed: {}\n", stats.completed));
    out.push_str(&format!("Failed:    {}\n", stats.failed));
    out.push_str(&format!("Avg time:  {:.1}s\n\n", stats.avg_duration_secs));

    out.push_str("By Repo:\n");
    for (repo, count) in &stats.by_repo {
        out.push_str(&format!("  {repo:<30} {count}\n"));
    }

    out.push_str("\nBy Model:\n");
    for (model, count, avg) in &stats.by_model {
        out.push_str(&format!("  {model:<24} {count:>4} avg {:.1}s\n", avg));
    }

    out.push_str("\nVerdicts:\n");
    for (verdict, count) in &stats.verdicts {
        out.push_str(&format!("  {verdict:<18} {count}\n"));
    }

    out
}

pub fn aggregate_verdicts(entries: &[ReviewLogEntry]) -> HashMap<String, i64> {
    let mut map = HashMap::new();
    for entry in entries {
        let key = entry
            .verdict
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        *map.entry(key).or_insert(0) += 1;
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dedupe_claim_prevents_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let claim = ReviewClaim {
            dedupe_key: "same".to_string(),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(claim.clone()).await.expect("first claim"));
        assert!(!db.claim_review(claim).await.expect("second claim"));
    }
}

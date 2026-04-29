use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

type DbOp = Box<dyn FnOnce(&Connection) + Send + 'static>;

#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    tx: std::sync::mpsc::Sender<DbOp>,
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .finish()
    }
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
pub struct RecurringFindingExample {
    pub pr_number: i64,
    pub author: String,
    pub path: String,
    pub line: Option<i64>,
    pub severity: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct RecurringFindingCandidate {
    pub fingerprint: String,
    pub summary: String,
    pub distinct_prs: i64,
    pub distinct_authors: i64,
    pub examples: Vec<RecurringFindingExample>,
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
    pub gitnexus_used: Option<bool>,
    pub gitnexus_latency_ms: Option<i64>,
    pub gitnexus_hit_count: Option<i64>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReviewAttemptRecord {
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

#[derive(Debug, Clone)]
pub struct ReviewFindingRecord {
    pub repo: String,
    pub pr_number: i64,
    pub author: Option<String>,
    pub fingerprint: String,
    pub recurrence_fingerprint: Option<String>,
    pub first_sha: String,
    pub last_seen_sha: String,
    pub status: String,
    pub severity: String,
    pub finding_kind: String,
    pub path: String,
    pub line: Option<i64>,
    pub body: String,
    pub evidence_note: Option<String>,
    pub github_comment_id: Option<i64>,
    pub resolved_by_sha: Option<String>,
    pub resolution_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ReviewFindingUpsert {
    pub repo: String,
    pub pr_number: i64,
    pub author: Option<String>,
    pub fingerprint: String,
    pub recurrence_fingerprint: Option<String>,
    pub sha: String,
    pub status: String,
    pub severity: String,
    pub finding_kind: String,
    pub path: String,
    pub line: Option<i64>,
    pub body: String,
    pub evidence_note: Option<String>,
    pub github_comment_id: Option<i64>,
    pub resolved_by_sha: Option<String>,
    pub resolution_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PendingFinalization {
    pub pr_number: i64,
    pub last_reviewed_sha: String,
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
    pub gitnexus_used_reviews: i64,
    pub gitnexus_avg_latency_ms: f64,
    pub gitnexus_avg_hit_count: f64,
}

#[derive(Debug, Clone, Default)]
pub struct DaemonStatus {
    pub started_at: Option<String>,
    pub last_poll_at: Option<String>,
    pub rate_limit_total: Option<i64>,
    pub rate_remaining: Option<i64>,
    pub rate_reset_epoch: Option<i64>,
    pub active_reviews: i64,
}

impl Database {
    pub async fn new(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("failed to create db dir {}", parent.display()))?;
        }

        let (tx, rx) = std::sync::mpsc::channel::<DbOp>();
        let (init_tx, init_rx) = tokio::sync::oneshot::channel::<Result<()>>();

        let db_path = path.clone();
        std::thread::Builder::new()
            .name("pr-reviewer-db".into())
            .spawn(move || {
                let conn = match open_conn(&db_path) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = init_tx.send(Err(e));
                        return;
                    }
                };
                let _ = init_tx.send(Ok(()));

                while let Ok(op) = rx.recv() {
                    op(&conn);
                }
                // All senders dropped, connection drops here (WAL checkpoint runs).
            })
            .context("failed to spawn database thread")?;

        init_rx
            .await
            .context("database thread exited before init completed")??;

        let db = Self { path, tx };
        db.migrate().await?;
        Ok(db)
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        let op: DbOp = Box::new(move |conn| {
            let _ = resp_tx.send(f(conn));
        });
        self.tx
            .send(op)
            .map_err(|_| anyhow::anyhow!("database thread has exited"))?;
        resp_rx
            .await
            .context("database thread dropped response channel")?
    }

    pub async fn migrate(&self) -> Result<()> {
        self.run(move |conn| {
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
                    gitnexus_used INTEGER,
                    gitnexus_latency_ms INTEGER,
                    gitnexus_hit_count INTEGER,
                    files_reviewed INTEGER,
                    diff_lines INTEGER,
                    error_message TEXT,
                    created_at TEXT DEFAULT (datetime('now')),
                    completed_at TEXT
                );

                CREATE TABLE IF NOT EXISTS review_archive (
                    repo TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    reviewed_sha TEXT NOT NULL,
                    terminal_state TEXT NOT NULL,
                    closed_at TEXT,
                    transcript TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    generated_at TEXT DEFAULT (datetime('now')),
                    PRIMARY KEY (repo, pr_number)
                );

                CREATE TABLE IF NOT EXISTS review_findings (
                    repo TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    author TEXT,
                    fingerprint TEXT NOT NULL,
                    recurrence_fingerprint TEXT,
                    first_sha TEXT NOT NULL,
                    last_seen_sha TEXT NOT NULL,
                    status TEXT NOT NULL,
                    severity TEXT NOT NULL,
                    finding_kind TEXT NOT NULL DEFAULT 'correctness',
                    path TEXT NOT NULL,
                    line INTEGER,
                    body TEXT NOT NULL,
                    evidence_note TEXT,
                    github_comment_id INTEGER,
                    resolved_by_sha TEXT,
                    resolution_reason TEXT,
                    created_at TEXT DEFAULT (datetime('now')),
                    updated_at TEXT DEFAULT (datetime('now')),
                    PRIMARY KEY (repo, pr_number, fingerprint)
                );

                CREATE TABLE IF NOT EXISTS daemon_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    started_at TEXT,
                    last_poll_at TEXT,
                    rate_limit_total INTEGER,
                    rate_remaining INTEGER,
                    rate_reset_epoch INTEGER,
                    active_reviews INTEGER DEFAULT 0
                );

                CREATE TABLE IF NOT EXISTS instruction_suggestion_prs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    repo TEXT NOT NULL,
                    fingerprint TEXT NOT NULL,
                    mode TEXT NOT NULL,
                    branch TEXT NOT NULL,
                    pr_url TEXT NOT NULL,
                    created_at TEXT DEFAULT (datetime('now')),
                    UNIQUE(repo, fingerprint)
                );
                "#,
            )?;

            let count: i64 =
                conn.query_row("SELECT COUNT(*) FROM schema_version", [], |row| row.get(0))?;
            if count == 0 {
                conn.execute("INSERT INTO schema_version(version) VALUES (1)", [])?;
            }

            // Column migrations must run BEFORE the seed INSERT below,
            // because existing databases have daemon_state without the new columns.
            // SQLite's OR IGNORE only suppresses constraint violations, not schema errors.
            ensure_column_exists(conn, "review_log", "gitnexus_used", "INTEGER")?;
            ensure_column_exists(conn, "review_log", "gitnexus_latency_ms", "INTEGER")?;
            ensure_column_exists(conn, "review_log", "gitnexus_hit_count", "INTEGER")?;
            ensure_column_exists(conn, "daemon_state", "started_at", "TEXT")?;
            ensure_column_exists(conn, "daemon_state", "rate_limit_total", "INTEGER")?;
            ensure_column_exists(conn, "daemon_state", "rate_reset_epoch", "INTEGER")?;
            ensure_column_exists(
                conn,
                "review_findings",
                "finding_kind",
                "TEXT NOT NULL DEFAULT 'correctness'",
            )?;
            ensure_column_exists(conn, "review_findings", "author", "TEXT")?;
            ensure_column_exists(conn, "review_findings", "recurrence_fingerprint", "TEXT")?;

            // Reply deduplication: prevents replying to the same comment twice
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS reply_log (
                    repo TEXT NOT NULL,
                    pr_number INTEGER NOT NULL,
                    comment_id INTEGER NOT NULL,
                    replied_at TEXT DEFAULT (datetime('now')),
                    PRIMARY KEY (repo, pr_number, comment_id)
                );
                "#,
            )?;

            conn.execute(
                "INSERT OR IGNORE INTO daemon_state (id, started_at, last_poll_at, rate_limit_total, rate_remaining, rate_reset_epoch, active_reviews) VALUES (1, NULL, NULL, NULL, NULL, NULL, 0)",
                [],
            )?;

            Ok(())
        })
        .await
    }

    /// Deletes a non-completed review claim to allow re-review.
    /// Only removes rows with status 'claimed' or 'failed' — completed entries are left intact
    /// so audit history is preserved and the GitHub-side duplicate guard still fires.
    /// Returns the status of the deleted row, or None if nothing was deleted.
    pub async fn delete_review_claim(&self, dedupe_key: &str) -> Result<Option<String>> {
        let key = dedupe_key.to_string();
        self.run(move |conn| {
            // SELECT before DELETE so we can inspect the status without relying on
            // RETURNING (requires SQLite >= 3.35.0). The dedicated DB thread serializes
            // all operations so nothing can race between the two statements.
            let status: Option<String> = conn
                .query_row(
                    "SELECT status FROM review_log WHERE dedupe_key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;

            let deletable = matches!(status.as_deref(), Some("claimed") | Some("failed"));
            if deletable {
                conn.execute("DELETE FROM review_log WHERE dedupe_key = ?1", params![key])?;
                Ok(status)
            } else {
                Ok(None)
            }
        })
        .await
    }

    pub async fn claim_review(&self, claim: ReviewClaim) -> Result<bool> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO review_log (dedupe_key, repo, pr_number, sha, harness, model, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'claimed')
                 ON CONFLICT(dedupe_key) DO NOTHING",
                params![
                    claim.dedupe_key,
                    claim.repo,
                    claim.pr_number,
                    claim.sha,
                    claim.harness,
                    claim.model
                ],
            )?;
            Ok(conn.changes() > 0)
        })
        .await
    }

    pub async fn complete_review(
        &self,
        dedupe_key: &str,
        comments_posted: i64,
        verdict: Option<&str>,
        duration_secs: f64,
        files_reviewed: i64,
        diff_lines: i64,
        gitnexus_used: Option<bool>,
        gitnexus_latency_ms: Option<i64>,
        gitnexus_hit_count: Option<i64>,
    ) -> Result<()> {
        let dedupe_key = dedupe_key.to_string();
        let verdict = verdict.map(ToString::to_string);
        let gitnexus_used = gitnexus_used.map(|v| if v { 1_i64 } else { 0_i64 });

        self.run(move |conn| {
            conn.execute(
                "UPDATE review_log SET status='completed', comments_posted=?2, verdict=?3, duration_secs=?4, files_reviewed=?5, diff_lines=?6, gitnexus_used=?7, gitnexus_latency_ms=?8, gitnexus_hit_count=?9, completed_at=datetime('now') WHERE dedupe_key=?1",
                params![
                    dedupe_key,
                    comments_posted,
                    verdict,
                    duration_secs,
                    files_reviewed,
                    diff_lines,
                    gitnexus_used,
                    gitnexus_latency_ms,
                    gitnexus_hit_count
                ],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn fail_review(
        &self,
        dedupe_key: &str,
        message: &str,
        duration_secs: f64,
    ) -> Result<()> {
        let dedupe_key = dedupe_key.to_string();
        let message = message.to_string();

        self.run(move |conn| {
            conn.execute(
                "UPDATE review_log SET status='failed', error_message=?2, duration_secs=?3, completed_at=datetime('now') WHERE dedupe_key=?1",
                params![dedupe_key, message, duration_secs],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_pr_review_attempts(
        &self,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<ReviewAttemptRecord>> {
        let repo = repo.to_string();

        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT sha, harness, model, status, comments_posted, verdict, duration_secs, error_message, created_at, completed_at FROM review_log WHERE repo = ?1 AND pr_number = ?2 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map(params![repo, pr_number], |row| {
                Ok(ReviewAttemptRecord {
                    sha: row.get(0)?,
                    harness: row.get(1)?,
                    model: row.get(2)?,
                    status: row.get(3)?,
                    comments_posted: row.get(4)?,
                    verdict: row.get(5)?,
                    duration_secs: row.get(6)?,
                    error_message: row.get(7)?,
                    created_at: row.get(8)?,
                    completed_at: row.get(9)?,
                })
            })?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn list_prs_pending_finalization(
        &self,
        repo: &str,
    ) -> Result<Vec<PendingFinalization>> {
        let repo = repo.to_string();

        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT p.pr_number, p.last_reviewed_sha FROM pr_state p LEFT JOIN review_archive a ON a.repo = p.repo AND a.pr_number = p.pr_number WHERE p.repo = ?1 AND p.last_reviewed_sha IS NOT NULL AND (a.reviewed_sha IS NULL OR a.reviewed_sha != p.last_reviewed_sha) ORDER BY p.pr_number ASC",
            )?;
            let rows = stmt.query_map(params![repo], |row| {
                Ok(PendingFinalization {
                    pr_number: row.get(0)?,
                    last_reviewed_sha: row.get(1)?,
                })
            })?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn upsert_review_archive(
        &self,
        repo: &str,
        pr_number: i64,
        reviewed_sha: &str,
        terminal_state: &str,
        closed_at: Option<&str>,
        transcript: &str,
        summary: &str,
    ) -> Result<()> {
        let repo = repo.to_string();
        let reviewed_sha = reviewed_sha.to_string();
        let terminal_state = terminal_state.to_string();
        let closed_at = closed_at.map(ToString::to_string);
        let transcript = transcript.to_string();
        let summary = summary.to_string();

        self.run(move |conn| {
            conn.execute(
                "INSERT INTO review_archive (repo, pr_number, reviewed_sha, terminal_state, closed_at, transcript, summary, generated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now')) ON CONFLICT(repo, pr_number) DO UPDATE SET reviewed_sha = excluded.reviewed_sha, terminal_state = excluded.terminal_state, closed_at = excluded.closed_at, transcript = excluded.transcript, summary = excluded.summary, generated_at = datetime('now')",
                params![repo, pr_number, reviewed_sha, terminal_state, closed_at, transcript, summary],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn list_review_findings(
        &self,
        repo: &str,
        pr_number: i64,
    ) -> Result<Vec<ReviewFindingRecord>> {
        let repo = repo.to_string();

        self.run(move |conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT repo, pr_number, author, fingerprint, recurrence_fingerprint,
                       first_sha, last_seen_sha, status, severity,
                       finding_kind, path, line, body, evidence_note, github_comment_id,
                       resolved_by_sha, resolution_reason, created_at, updated_at
                  FROM review_findings
                 WHERE repo = ?1 AND pr_number = ?2
                 ORDER BY updated_at ASC, rowid ASC
                "#,
            )?;
            let rows = stmt.query_map(params![repo, pr_number], |row| {
                Ok(ReviewFindingRecord {
                    repo: row.get(0)?,
                    pr_number: row.get(1)?,
                    author: row.get(2)?,
                    fingerprint: row.get(3)?,
                    recurrence_fingerprint: row.get(4)?,
                    first_sha: row.get(5)?,
                    last_seen_sha: row.get(6)?,
                    status: row.get(7)?,
                    severity: row.get(8)?,
                    finding_kind: row.get(9)?,
                    path: row.get(10)?,
                    line: row.get(11)?,
                    body: row.get(12)?,
                    evidence_note: row.get(13)?,
                    github_comment_id: row.get(14)?,
                    resolved_by_sha: row.get(15)?,
                    resolution_reason: row.get(16)?,
                    created_at: row.get(17)?,
                    updated_at: row.get(18)?,
                })
            })?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn upsert_review_findings(&self, findings: Vec<ReviewFindingUpsert>) -> Result<()> {
        self.run(move |conn| {
            let tx = conn.unchecked_transaction()?;
            {
                let mut stmt = tx.prepare(
                    r#"
                    INSERT INTO review_findings (
                        repo, pr_number, author, fingerprint, recurrence_fingerprint,
                        first_sha, last_seen_sha, status, severity,
                        finding_kind, path, line, body, evidence_note, github_comment_id,
                        resolved_by_sha, resolution_reason, created_at, updated_at
                    )
                    VALUES (
                        ?1, ?2, ?3, ?4, ?5,
                        ?6, ?6, ?7, ?8,
                        ?9, ?10, ?11, ?12, ?13, ?14,
                        ?15, ?16, datetime('now'), datetime('now')
                    )
                    ON CONFLICT(repo, pr_number, fingerprint) DO UPDATE SET
                        last_seen_sha = excluded.last_seen_sha,
                        author = COALESCE(excluded.author, review_findings.author),
                        recurrence_fingerprint = COALESCE(excluded.recurrence_fingerprint, review_findings.recurrence_fingerprint),
                        status = excluded.status,
                        severity = excluded.severity,
                        finding_kind = excluded.finding_kind,
                        path = excluded.path,
                        line = excluded.line,
                        body = excluded.body,
                        evidence_note = excluded.evidence_note,
                        github_comment_id = COALESCE(excluded.github_comment_id, review_findings.github_comment_id),
                        resolved_by_sha = excluded.resolved_by_sha,
                        resolution_reason = excluded.resolution_reason,
                        updated_at = datetime('now')
                    "#,
                )?;

                for finding in findings {
                    stmt.execute(params![
                        finding.repo,
                        finding.pr_number,
                        finding.author,
                        finding.fingerprint,
                        finding.recurrence_fingerprint,
                        finding.sha,
                        finding.status,
                        finding.severity,
                        finding.finding_kind,
                        finding.path,
                        finding.line,
                        finding.body,
                        finding.evidence_note,
                        finding.github_comment_id,
                        finding.resolved_by_sha,
                        finding.resolution_reason,
                    ])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
        .await
    }

    pub async fn get_pr_state(&self, repo: &str, pr_number: i64) -> Result<Option<PrState>> {
        let repo = repo.to_string();

        self.run(move |conn| {
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
            Ok(row)
        })
        .await
    }

    pub async fn upsert_pr_state(
        &self,
        repo: &str,
        pr_number: i64,
        last_reviewed_sha: Option<&str>,
        last_comment_check: Option<&str>,
    ) -> Result<()> {
        let repo = repo.to_string();
        let last_reviewed_sha = last_reviewed_sha.map(ToString::to_string);
        let last_comment_check = last_comment_check.map(ToString::to_string);

        self.run(move |conn| {
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
            Ok(())
        })
        .await
    }

    pub async fn update_comment_check(
        &self,
        repo: &str,
        pr_number: i64,
        last_comment_check: &str,
    ) -> Result<()> {
        let repo = repo.to_string();
        let last_comment_check = last_comment_check.to_string();

        self.run(move |conn| {
            conn.execute(
                r#"
                INSERT INTO pr_state (repo, pr_number, last_comment_check, review_count)
                VALUES (?1, ?2, ?3, 0)
                ON CONFLICT(repo, pr_number) DO UPDATE SET
                  last_comment_check=excluded.last_comment_check
                "#,
                params![repo, pr_number, last_comment_check],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn recurring_finding_candidates(
        &self,
        repo: &str,
        min_distinct_prs: u32,
        limit: usize,
    ) -> Result<Vec<RecurringFindingCandidate>> {
        let repo = repo.to_string();
        let min_distinct_prs = i64::from(min_distinct_prs.max(2));
        let limit = limit.max(1) as i64;

        self.run(move |conn| {
            let mut stmt = conn.prepare(
                r#"
                SELECT
                  COALESCE(f.recurrence_fingerprint, f.fingerprint) AS recurrence_key,
                  MIN(f.body),
                  COUNT(DISTINCT f.pr_number) AS distinct_prs,
                  COUNT(DISTINCT f.author) AS distinct_authors
                FROM review_findings f
                WHERE f.repo = ?1
                  AND f.status IN ('open', 'still_blocking')
                  AND NOT EXISTS (
                    SELECT 1
                    FROM instruction_suggestion_prs p
                    WHERE p.repo = f.repo
                      AND p.fingerprint = COALESCE(f.recurrence_fingerprint, f.fingerprint)
                  )
                GROUP BY recurrence_key
                HAVING COUNT(DISTINCT f.pr_number) >= ?2
                ORDER BY MAX(f.updated_at) DESC
                LIMIT ?3
                "#,
            )?;

            let rows = stmt.query_map(params![repo, min_distinct_prs, limit], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;

            let mut candidates = Vec::new();
            for row in rows {
                let (recurrence_key, summary, distinct_prs, distinct_authors) = row?;
                let mut examples_stmt = conn.prepare(
                    r#"
                    SELECT pr_number, COALESCE(author, 'unknown'), path, line, severity, body
                    FROM review_findings
                    WHERE repo = ?1
                      AND COALESCE(recurrence_fingerprint, fingerprint) = ?2
                      AND status IN ('open', 'still_blocking')
                    ORDER BY updated_at DESC
                    LIMIT 4
                    "#,
                )?;
                let examples_rows =
                    examples_stmt.query_map(params![repo, recurrence_key.as_str()], |row| {
                        Ok(RecurringFindingExample {
                            pr_number: row.get(0)?,
                            author: row.get(1)?,
                            path: row.get(2)?,
                            line: row.get(3)?,
                            severity: row.get(4)?,
                            body: row.get(5)?,
                        })
                    })?;
                let mut examples = Vec::new();
                for example in examples_rows {
                    examples.push(example?);
                }

                candidates.push(RecurringFindingCandidate {
                    fingerprint: recurrence_key,
                    summary,
                    distinct_prs,
                    distinct_authors,
                    examples,
                });
            }

            Ok(candidates)
        })
        .await
    }

    pub async fn record_instruction_suggestion_pr(
        &self,
        repo: &str,
        fingerprint: &str,
        mode: &str,
        branch: &str,
        pr_url: &str,
    ) -> Result<()> {
        let repo = repo.to_string();
        let fingerprint = fingerprint.to_string();
        let mode = mode.to_string();
        let branch = branch.to_string();
        let pr_url = pr_url.to_string();

        self.run(move |conn| {
            conn.execute(
                r#"
                INSERT INTO instruction_suggestion_prs
                  (repo, fingerprint, mode, branch, pr_url)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(repo, fingerprint) DO UPDATE SET
                  mode=excluded.mode,
                  branch=excluded.branch,
                  pr_url=excluded.pr_url
                "#,
                params![repo, fingerprint, mode, branch, pr_url],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn claim_instruction_suggestion_pr(
        &self,
        repo: &str,
        fingerprint: &str,
        mode: &str,
    ) -> Result<bool> {
        let repo = repo.to_string();
        let fingerprint = fingerprint.to_string();
        let mode = mode.to_string();

        self.run(move |conn| {
            conn.execute(
                r#"
                INSERT INTO instruction_suggestion_prs
                  (repo, fingerprint, mode, branch, pr_url)
                VALUES (?1, ?2, ?3, '', '')
                ON CONFLICT(repo, fingerprint) DO NOTHING
                "#,
                params![repo, fingerprint, mode],
            )?;
            Ok(conn.changes() > 0)
        })
        .await
    }

    pub async fn release_instruction_suggestion_claim(
        &self,
        repo: &str,
        fingerprint: &str,
    ) -> Result<()> {
        let repo = repo.to_string();
        let fingerprint = fingerprint.to_string();

        self.run(move |conn| {
            conn.execute(
                r#"
                DELETE FROM instruction_suggestion_prs
                WHERE repo = ?1
                  AND fingerprint = ?2
                  AND branch = ''
                  AND pr_url = ''
                "#,
                params![repo, fingerprint],
            )?;
            Ok(())
        })
        .await
    }

    /// Attempt to claim a reply slot for a comment. Returns true if this is the
    /// first reply (inserted), false if we already replied (unique constraint).
    pub async fn claim_reply(&self, repo: &str, pr_number: i64, comment_id: i64) -> Result<bool> {
        let repo = repo.to_string();
        self.run(move |conn| {
            let result = conn.execute(
                "INSERT OR IGNORE INTO reply_log (repo, pr_number, comment_id) VALUES (?1, ?2, ?3)",
                params![repo, pr_number, comment_id],
            )?;
            Ok(result > 0)
        })
        .await
    }

    /// Returns the number of replies posted to this PR within the last `window_secs` seconds.
    /// Used as a circuit breaker to prevent runaway reply loops.
    pub async fn recent_reply_count(
        &self,
        repo: &str,
        pr_number: i64,
        window_secs: i64,
    ) -> Result<i64> {
        let repo = repo.to_string();
        self.run(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM reply_log WHERE repo = ?1 AND pr_number = ?2 AND replied_at > datetime('now', ?3)",
                params![repo, pr_number, format!("-{window_secs} seconds")],
                |row| row.get(0),
            )?;
            Ok(count)
        })
        .await
    }

    pub async fn set_repo_etag(&self, repo: &str, etag: Option<&str>) -> Result<()> {
        let repo = repo.to_string();
        let etag = etag.map(ToString::to_string);

        self.run(move |conn| {
            conn.execute(
                "INSERT INTO repo_state(repo, etag) VALUES (?1, ?2) ON CONFLICT(repo) DO UPDATE SET etag=excluded.etag",
                params![repo, etag],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_repo_etag(&self, repo: &str) -> Result<Option<String>> {
        let repo = repo.to_string();

        self.run(move |conn| {
            let etag = conn
                .query_row(
                    "SELECT etag FROM repo_state WHERE repo=?1",
                    params![repo],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(etag)
        })
        .await
    }

    pub async fn sweep_stale_claims(&self, max_age_secs: u64) -> Result<u64> {
        self.run(move |conn| {
            let changed = conn.execute(
                "UPDATE review_log SET status='failed', error_message='stale claim swept', completed_at=datetime('now') WHERE status='claimed' AND created_at < datetime('now', ?1)",
                params![format!("-{} seconds", max_age_secs)],
            )?;
            Ok(changed as u64)
        })
        .await
    }

    pub async fn list_logs(&self, filter: LogsFilter) -> Result<Vec<ReviewLogEntry>> {
        self.run(move |conn| {
            let mut sql = String::from(
                "SELECT id, repo, pr_number, sha, harness, model, status, comments_posted, verdict, duration_secs, gitnexus_used, gitnexus_latency_ms, gitnexus_hit_count, error_message, created_at, completed_at FROM review_log WHERE 1=1",
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
                    gitnexus_used: row.get(10)?,
                    gitnexus_latency_ms: row.get(11)?,
                    gitnexus_hit_count: row.get(12)?,
                    error_message: row.get(13)?,
                    created_at: row.get(14)?,
                    completed_at: row.get(15)?,
                })
            })?;

            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
        .await
    }

    pub async fn usage_stats(&self, since: Option<&str>, repo: Option<&str>) -> Result<UsageStats> {
        let since = since.map(ToString::to_string);
        let repo = repo.map(ToString::to_string);

        self.run(move |conn| {
            let (where_clause, values) = build_where_clause(since.as_deref(), repo.as_deref());

            let mut stats = UsageStats::default();
            let mut args = values.clone();

            let mut stmt = conn.prepare(&format!(
                "SELECT COUNT(*), SUM(CASE WHEN status='completed' THEN 1 ELSE 0 END), SUM(CASE WHEN status='failed' THEN 1 ELSE 0 END), AVG(duration_secs), SUM(CASE WHEN gitnexus_used = 1 THEN 1 ELSE 0 END), AVG(CASE WHEN gitnexus_used = 1 THEN gitnexus_latency_ms END), AVG(CASE WHEN gitnexus_used = 1 THEN gitnexus_hit_count END) FROM review_log {where_clause}"
            ))?;
            let row = stmt.query_row(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    r.get::<_, Option<f64>>(3)?.unwrap_or(0.0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, Option<f64>>(5)?.unwrap_or(0.0),
                    r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                ))
            })?;
            stats.total = row.0;
            stats.completed = row.1;
            stats.failed = row.2;
            stats.avg_duration_secs = row.3;
            stats.gitnexus_used_reviews = row.4;
            stats.gitnexus_avg_latency_ms = row.5;
            stats.gitnexus_avg_hit_count = row.6;

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

            Ok(stats)
        })
        .await
    }

    pub async fn set_daemon_started(&self, started_at: &str) -> Result<()> {
        let started_at = started_at.to_string();

        self.run(move |conn| {
            conn.execute(
                "UPDATE daemon_state SET started_at=?1, last_poll_at=NULL, rate_limit_total=NULL, rate_remaining=NULL, rate_reset_epoch=NULL, active_reviews=0 WHERE id=1",
                params![started_at],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn set_daemon_status(
        &self,
        last_poll_at: &str,
        rate_limit_total: Option<i64>,
        rate_remaining: Option<i64>,
        rate_reset_epoch: Option<i64>,
        active_reviews: i64,
    ) -> Result<()> {
        let last_poll_at = last_poll_at.to_string();

        self.run(move |conn| {
            conn.execute(
                "UPDATE daemon_state SET last_poll_at=?1, rate_limit_total=?2, rate_remaining=?3, rate_reset_epoch=?4, active_reviews=?5 WHERE id=1",
                params![last_poll_at, rate_limit_total, rate_remaining, rate_reset_epoch, active_reviews],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn get_daemon_status(&self) -> Result<DaemonStatus> {
        self.run(move |conn| {
            let row = conn.query_row(
                "SELECT started_at, last_poll_at, rate_limit_total, rate_remaining, rate_reset_epoch, active_reviews FROM daemon_state WHERE id=1",
                [],
                |r| {
                    Ok(DaemonStatus {
                        started_at: r.get(0)?,
                        last_poll_at: r.get(1)?,
                        rate_limit_total: r.get(2)?,
                        rate_remaining: r.get(3)?,
                        rate_reset_epoch: r.get(4)?,
                        active_reviews: r.get(5)?,
                    })
                },
            )?;
            Ok(row)
        })
        .await
    }

    pub async fn get_claimed_reviews(&self) -> Result<i64> {
        self.run(move |conn| {
            let depth = conn.query_row(
                "SELECT COUNT(*) FROM review_log WHERE status='claimed'",
                [],
                |r| r.get(0),
            )?;
            Ok(depth)
        })
        .await
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

fn ensure_column_exists(
    conn: &Connection,
    table: &str,
    column: &str,
    column_type: &str,
) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(());
        }
    }

    let alter = format!("ALTER TABLE {table} ADD COLUMN {column} {column_type}");
    conn.execute(&alter, [])?;
    Ok(())
}

fn open_conn(path: &PathBuf) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open sqlite db {}", path.display()))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(conn)
}

pub fn dedupe_key(repo: &str, pr_number: u64, sha: &str, harness: &str, dry_run: bool) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(repo.as_bytes());
    hasher.update(b":");
    hasher.update(pr_number.to_string().as_bytes());
    hasher.update(b":");
    hasher.update(sha.as_bytes());
    hasher.update(b":");
    hasher.update(harness.as_bytes());
    hasher.update(b":");
    hasher.update(if dry_run {
        &b"dry-run"[..]
    } else {
        &b"live"[..]
    });
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

    out.push_str("\nGitNexus:\n");
    out.push_str(&format!(
        "  used in reviews: {}\n",
        stats.gitnexus_used_reviews
    ));
    out.push_str(&format!(
        "  avg latency:     {:.1} ms\n",
        stats.gitnexus_avg_latency_ms
    ));
    out.push_str(&format!(
        "  avg hit count:   {:.1}\n",
        stats.gitnexus_avg_hit_count
    ));

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

    #[test]
    fn dedupe_key_distinguishes_dry_run_from_live() {
        let live = dedupe_key("o/r", 42, "abc123", "codex", false);
        let dry_run = dedupe_key("o/r", 42, "abc123", "codex", true);

        assert_ne!(live, dry_run);
    }

    #[tokio::test]
    async fn dry_run_and_live_claims_do_not_block_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let dry_run_claim = ReviewClaim {
            dedupe_key: dedupe_key("o/r", 1, "abc", "codex", true),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };
        let live_claim = ReviewClaim {
            dedupe_key: dedupe_key("o/r", 1, "abc", "codex", false),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(dry_run_claim).await.expect("dry-run claim"));
        assert!(db.claim_review(live_claim).await.expect("live claim"));
    }

    #[tokio::test]
    async fn migrate_adds_gitnexus_columns_for_existing_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state.db");

        {
            let conn = Connection::open(&path).expect("open sqlite");
            conn.execute_batch(
                r#"
                CREATE TABLE schema_version (version INTEGER NOT NULL);
                INSERT INTO schema_version(version) VALUES (1);

                CREATE TABLE review_log (
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
                "#,
            )
            .expect("seed old schema");
        }

        let db = Database::new(path.clone()).await.expect("migrate");
        let conn = open_conn(db.path()).expect("open migrated db");
        let mut stmt = conn
            .prepare("PRAGMA table_info(review_log)")
            .expect("prepare pragma");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .expect("query table_info");
        let columns: Vec<String> = rows.map(|r| r.expect("col")).collect();

        assert!(columns.contains(&"gitnexus_used".to_string()));
        assert!(columns.contains(&"gitnexus_latency_ms".to_string()));
        assert!(columns.contains(&"gitnexus_hit_count".to_string()));

        let mut stmt = conn
            .prepare("PRAGMA table_info(daemon_state)")
            .expect("prepare daemon pragma");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .expect("query daemon table_info");
        let daemon_columns: Vec<String> = rows.map(|r| r.expect("col")).collect();

        assert!(daemon_columns.contains(&"started_at".to_string()));
        assert!(daemon_columns.contains(&"rate_limit_total".to_string()));
        assert!(daemon_columns.contains(&"rate_reset_epoch".to_string()));
    }

    #[tokio::test]
    async fn migrate_creates_review_archive_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");
        let conn = open_conn(db.path()).expect("open migrated db");
        let mut stmt = conn
            .prepare("PRAGMA table_info(review_archive)")
            .expect("prepare archive pragma");
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .expect("query archive table_info");
        let columns: Vec<String> = rows.map(|r| r.expect("col")).collect();

        assert!(columns.contains(&"reviewed_sha".to_string()));
        assert!(columns.contains(&"terminal_state".to_string()));
        assert!(columns.contains(&"transcript".to_string()));
        assert!(columns.contains(&"summary".to_string()));
    }

    #[tokio::test]
    async fn review_findings_round_trip_and_update_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        db.upsert_review_findings(vec![ReviewFindingUpsert {
            repo: "o/r".to_string(),
            pr_number: 7,
            author: Some("alice".to_string()),
            fingerprint: "fp-1".to_string(),
            recurrence_fingerprint: Some("recurrence-1".to_string()),
            sha: "sha-1".to_string(),
            status: "open".to_string(),
            severity: "blocking".to_string(),
            finding_kind: "data_integrity".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(42),
            body: "drops data".to_string(),
            evidence_note: Some("line 42".to_string()),
            github_comment_id: None,
            resolved_by_sha: None,
            resolution_reason: None,
        }])
        .await
        .expect("insert finding");

        db.upsert_review_findings(vec![ReviewFindingUpsert {
            repo: "o/r".to_string(),
            pr_number: 7,
            author: Some("alice".to_string()),
            fingerprint: "fp-1".to_string(),
            recurrence_fingerprint: Some("recurrence-1".to_string()),
            sha: "sha-2".to_string(),
            status: "likely_fixed".to_string(),
            severity: "blocking".to_string(),
            finding_kind: "data_integrity".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(42),
            body: "drops data".to_string(),
            evidence_note: Some("line 42".to_string()),
            github_comment_id: None,
            resolved_by_sha: Some("sha-2".to_string()),
            resolution_reason: Some("not re-emitted".to_string()),
        }])
        .await
        .expect("update finding");

        let findings = db
            .list_review_findings("o/r", 7)
            .await
            .expect("list findings");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].first_sha, "sha-1");
        assert_eq!(findings[0].last_seen_sha, "sha-2");
        assert_eq!(findings[0].status, "likely_fixed");
        assert_eq!(findings[0].resolved_by_sha.as_deref(), Some("sha-2"));
    }

    #[tokio::test]
    async fn pending_finalization_clears_once_review_archive_is_saved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        db.upsert_pr_state("o/r", 7, Some("sha-1"), None)
            .await
            .expect("upsert pr state");

        let pending = db
            .list_prs_pending_finalization("o/r")
            .await
            .expect("pending before archive");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].pr_number, 7);
        assert_eq!(pending[0].last_reviewed_sha, "sha-1");

        db.upsert_review_archive(
            "o/r",
            7,
            "sha-1",
            "merged",
            Some("2026-04-01T12:00:00Z"),
            "transcript",
            "summary",
        )
        .await
        .expect("save archive");

        let pending = db
            .list_prs_pending_finalization("o/r")
            .await
            .expect("pending after archive");
        assert!(pending.is_empty());

        db.upsert_pr_state("o/r", 7, Some("sha-2"), None)
            .await
            .expect("advance reviewed sha");
        let pending = db
            .list_prs_pending_finalization("o/r")
            .await
            .expect("pending after new sha");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].last_reviewed_sha, "sha-2");
    }

    #[tokio::test]
    async fn pending_finalization_requires_archive_sha_to_match_last_reviewed_sha() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        db.upsert_pr_state("o/r", 7, Some("last-reviewed-sha"), None)
            .await
            .expect("upsert pr state");

        db.upsert_review_archive(
            "o/r",
            7,
            "closed-head-sha",
            "merged",
            Some("2026-04-01T12:00:00Z"),
            "transcript",
            "summary",
        )
        .await
        .expect("save archive with wrong sha");

        let pending = db
            .list_prs_pending_finalization("o/r")
            .await
            .expect("pending after mismatched archive sha");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].last_reviewed_sha, "last-reviewed-sha");

        db.upsert_review_archive(
            "o/r",
            7,
            "last-reviewed-sha",
            "merged",
            Some("2026-04-01T12:00:00Z"),
            "transcript",
            "summary",
        )
        .await
        .expect("save archive with matching sha");

        let pending = db
            .list_prs_pending_finalization("o/r")
            .await
            .expect("pending after matching archive sha");
        assert!(pending.is_empty());
    }

    #[tokio::test]
    async fn list_pr_review_attempts_returns_attempts_in_created_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let first = ReviewClaim {
            dedupe_key: "attempt-1".to_string(),
            repo: "o/r".to_string(),
            pr_number: 9,
            sha: "sha-1".to_string(),
            harness: "codex".to_string(),
            model: Some("model-a".to_string()),
        };
        let second = ReviewClaim {
            dedupe_key: "attempt-2".to_string(),
            repo: "o/r".to_string(),
            pr_number: 9,
            sha: "sha-2".to_string(),
            harness: "claude-code".to_string(),
            model: Some("model-b".to_string()),
        };

        assert!(db.claim_review(first).await.expect("claim first"));
        db.complete_review(
            "attempt-1",
            1,
            Some("COMMENT"),
            1.5,
            1,
            10,
            None,
            None,
            None,
        )
        .await
        .expect("complete first");
        assert!(db.claim_review(second).await.expect("claim second"));
        db.fail_review("attempt-2", "timeout", 2.0)
            .await
            .expect("fail second");

        let attempts = db
            .list_pr_review_attempts("o/r", 9)
            .await
            .expect("list attempts");
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].sha, "sha-1");
        assert_eq!(attempts[0].status, "completed");
        assert_eq!(attempts[1].sha, "sha-2");
        assert_eq!(attempts[1].status, "failed");
        assert_eq!(attempts[1].error_message.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn delete_review_claim_clears_claimed_and_allows_reclaim() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let claim = ReviewClaim {
            dedupe_key: "force-claimed".to_string(),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(claim.clone()).await.expect("first claim"));
        // second claim is blocked
        assert!(!db.claim_review(claim.clone()).await.expect("blocked"));

        let deleted = db
            .delete_review_claim("force-claimed")
            .await
            .expect("delete");
        assert_eq!(deleted.as_deref(), Some("claimed"));

        // after deletion, claiming again succeeds
        assert!(db.claim_review(claim).await.expect("reclaim"));
    }

    #[tokio::test]
    async fn delete_review_claim_preserves_completed_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let claim = ReviewClaim {
            dedupe_key: "force-completed".to_string(),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(claim.clone()).await.expect("claim"));
        db.complete_review(
            "force-completed",
            3,
            Some("COMMENT"),
            5.0,
            2,
            100,
            None,
            None,
            None,
        )
        .await
        .expect("complete");

        // --force cannot delete a completed entry
        let deleted = db
            .delete_review_claim("force-completed")
            .await
            .expect("delete attempt");
        assert!(deleted.is_none());

        // completed entry still blocks a new claim
        assert!(!db.claim_review(claim).await.expect("still blocked"));
    }

    #[tokio::test]
    async fn delete_review_claim_returns_none_for_missing_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let result = db
            .delete_review_claim("nonexistent-key")
            .await
            .expect("delete");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn claim_review_does_not_reclaim_failed_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let claim = ReviewClaim {
            dedupe_key: "retry-after-fail".to_string(),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(claim.clone()).await.expect("first claim"));
        db.fail_review("retry-after-fail", "harness timeout", 30.0)
            .await
            .expect("fail");

        // failed entries are terminal for the same PR/SHA/harness unless --force deletes them
        assert!(!db
            .claim_review(claim.clone())
            .await
            .expect("blocked after failure"));

        let deleted = db
            .delete_review_claim("retry-after-fail")
            .await
            .expect("delete failed claim");
        assert_eq!(deleted.as_deref(), Some("failed"));

        assert!(db
            .claim_review(claim)
            .await
            .expect("reclaim after force delete"));
    }

    #[tokio::test]
    async fn claim_review_does_not_reclaim_completed_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let claim = ReviewClaim {
            dedupe_key: "no-retry-completed".to_string(),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(claim.clone()).await.expect("claim"));
        db.complete_review(
            "no-retry-completed",
            2,
            Some("COMMENT"),
            5.0,
            2,
            100,
            None,
            None,
            None,
        )
        .await
        .expect("complete");

        // completed entries must not be reclaimed
        assert!(!db.claim_review(claim).await.expect("blocked by completed"));
    }

    #[tokio::test]
    async fn recurring_candidates_require_distinct_prs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let base = ReviewFindingUpsert {
            repo: "o/r".to_string(),
            pr_number: 1,
            author: Some("alice".to_string()),
            fingerprint: "panic-empty-input".to_string(),
            recurrence_fingerprint: Some("panic-empty-input".to_string()),
            sha: "abc".to_string(),
            status: "open".to_string(),
            finding_kind: "correctness".to_string(),
            body: "Empty input can panic".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(10),
            severity: "blocking".to_string(),
            evidence_note: None,
            github_comment_id: None,
            resolved_by_sha: None,
            resolution_reason: None,
        };

        db.upsert_review_findings(vec![
            base.clone(),
            ReviewFindingUpsert {
                sha: "def".to_string(),
                ..base.clone()
            },
        ])
        .await
        .expect("record same pr twice");
        assert!(db
            .recurring_finding_candidates("o/r", 2, 10)
            .await
            .expect("candidates")
            .is_empty());

        db.upsert_review_findings(vec![ReviewFindingUpsert {
            pr_number: 2,
            sha: "ghi".to_string(),
            ..base
        }])
        .await
        .expect("record second pr");
        let candidates = db
            .recurring_finding_candidates("o/r", 2, 10)
            .await
            .expect("candidates");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].distinct_prs, 2);
    }

    #[tokio::test]
    async fn instruction_suggestion_claim_blocks_duplicate_candidates_until_released() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let base = ReviewFindingUpsert {
            repo: "o/r".to_string(),
            pr_number: 1,
            author: Some("alice".to_string()),
            fingerprint: "panic-empty-input-1".to_string(),
            recurrence_fingerprint: Some("panic-empty-input".to_string()),
            sha: "abc".to_string(),
            status: "open".to_string(),
            finding_kind: "correctness".to_string(),
            body: "Empty input can panic".to_string(),
            path: "src/lib.rs".to_string(),
            line: Some(10),
            severity: "blocking".to_string(),
            evidence_note: None,
            github_comment_id: None,
            resolved_by_sha: None,
            resolution_reason: None,
        };

        db.upsert_review_findings(vec![
            base.clone(),
            ReviewFindingUpsert {
                pr_number: 2,
                fingerprint: "panic-empty-input-2".to_string(),
                sha: "def".to_string(),
                ..base
            },
        ])
        .await
        .expect("record recurring finding");

        let candidates = db
            .recurring_finding_candidates("o/r", 2, 10)
            .await
            .expect("candidate before claim");
        assert_eq!(candidates.len(), 1);

        assert!(db
            .claim_instruction_suggestion_pr("o/r", "panic-empty-input", "conservative")
            .await
            .expect("claim"));
        assert!(!db
            .claim_instruction_suggestion_pr("o/r", "panic-empty-input", "conservative")
            .await
            .expect("duplicate claim"));
        assert!(db
            .recurring_finding_candidates("o/r", 2, 10)
            .await
            .expect("candidate after claim")
            .is_empty());

        db.release_instruction_suggestion_claim("o/r", "panic-empty-input")
            .await
            .expect("release");
        assert_eq!(
            db.recurring_finding_candidates("o/r", 2, 10)
                .await
                .expect("candidate after release")
                .len(),
            1
        );

        assert!(db
            .claim_instruction_suggestion_pr("o/r", "panic-empty-input", "conservative")
            .await
            .expect("claim again"));
        db.record_instruction_suggestion_pr(
            "o/r",
            "panic-empty-input",
            "conservative",
            "pr-reviewer/recurring-guardrail",
            "https://github.com/o/r/pull/3",
        )
        .await
        .expect("record pr");
        db.release_instruction_suggestion_claim("o/r", "panic-empty-input")
            .await
            .expect("release completed record is a no-op");
        assert!(db
            .recurring_finding_candidates("o/r", 2, 10)
            .await
            .expect("candidate after record")
            .is_empty());
    }

    #[tokio::test]
    async fn get_claimed_reviews_counts_correctly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        let claim = ReviewClaim {
            dedupe_key: "depth".to_string(),
            repo: "o/r".to_string(),
            pr_number: 1,
            sha: "abc".to_string(),
            harness: "codex".to_string(),
            model: None,
        };

        assert!(db.claim_review(claim).await.expect("claim"));
        assert_eq!(db.get_claimed_reviews().await.expect("depth"), 1);
    }

    #[tokio::test]
    async fn reply_claim_prevents_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        // First claim succeeds
        assert!(db.claim_reply("o/r", 1, 100).await.expect("first claim"));
        // Second claim for same comment fails (already replied)
        assert!(
            !db.claim_reply("o/r", 1, 100).await.expect("second claim"),
            "duplicate reply claim should return false"
        );
        // Different comment on same PR succeeds
        assert!(db
            .claim_reply("o/r", 1, 200)
            .await
            .expect("different comment"));
    }

    #[tokio::test]
    async fn recent_reply_count_tracks_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::new(dir.path().join("state.db"))
            .await
            .expect("db");

        // No replies yet
        let count = db.recent_reply_count("o/r", 1, 600).await.expect("count");
        assert_eq!(count, 0);

        // Add some replies
        db.claim_reply("o/r", 1, 100).await.expect("claim 1");
        db.claim_reply("o/r", 1, 200).await.expect("claim 2");
        db.claim_reply("o/r", 1, 300).await.expect("claim 3");

        let count = db.recent_reply_count("o/r", 1, 600).await.expect("count");
        assert_eq!(count, 3);

        // Different PR should have 0
        let count = db.recent_reply_count("o/r", 2, 600).await.expect("count");
        assert_eq!(count, 0);
    }
}

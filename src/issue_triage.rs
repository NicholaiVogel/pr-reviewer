use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tempfile::tempdir;

use crate::config::{AppConfig, RepoConfig};
use crate::context::file_reader::read_file_safe;
use crate::github::client::GitHubClient;
use crate::github::types::{Issue, IssueComment, Label};
use crate::harness::for_kind;
use crate::harness::spawn::{run_harness, HarnessRunRequest};
use crate::store::db::Database;

const REPO_TRIAGE_FILES: &[&str] = &[
    "AGENTS.md",
    "CLAUDE.md",
    "README.md",
    "CONTRIBUTING.md",
    "docs/specs/INDEX.md",
    "docs/specs/dependencies.yaml",
];
const MAX_SINGLE_CONTEXT_FILE_BYTES: usize = 16 * 1024;
const MAX_LABEL_CATALOG_BYTES: usize = 12 * 1024;
const MAX_COMMENT_CONTEXT_BYTES: usize = 12 * 1024;
const PROMPT_TRUNCATION_NOTE: &str =
    "\n\n[issue triage prompt truncated to fit configured prompt size limit]\n";

#[derive(Clone)]
pub struct IssueTriageEngine {
    config: Arc<AppConfig>,
    github: GitHubClient,
    db: Database,
}

#[derive(Debug, Clone)]
pub struct IssueTriageResult {
    pub labels_added: Vec<String>,
    pub labels_created: Vec<String>,
    pub summary: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct IssueTriageOptions {
    pub dry_run: bool,
    pub already_claimed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProposedLabel {
    pub name: String,
    pub color: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IssueTriageDecision {
    pub summary: String,
    #[serde(default)]
    pub labels_to_add: Vec<String>,
    #[serde(default)]
    pub labels_to_create: Vec<ProposedLabel>,
}

#[derive(Debug, Clone)]
struct LabelPlan {
    labels_to_add: Vec<String>,
    labels_to_create: Vec<ProposedLabel>,
}

impl IssueTriageEngine {
    pub fn new(config: Arc<AppConfig>, github: GitHubClient, db: Database) -> Self {
        Self { config, github, db }
    }

    pub async fn triage_issue(
        &self,
        repo_cfg: &RepoConfig,
        issue: &Issue,
    ) -> Result<IssueTriageResult> {
        self.triage_issue_with_options(
            repo_cfg,
            issue,
            IssueTriageOptions {
                already_claimed: true,
                ..IssueTriageOptions::default()
            },
        )
        .await
    }

    pub async fn triage_issue_with_options(
        &self,
        repo_cfg: &RepoConfig,
        issue: &Issue,
        options: IssueTriageOptions,
    ) -> Result<IssueTriageResult> {
        let repo_name = repo_cfg.full_name();
        if !options.dry_run && !options.already_claimed {
            self.db
                .begin_issue_triage_attempt(&repo_name, issue.number as i64)
                .await?;
        }

        match self.triage_issue_inner(repo_cfg, issue, options).await {
            Ok(result) => {
                if !options.dry_run {
                    self.db
                        .complete_issue_triage(&repo_name, issue.number as i64)
                        .await?;
                }
                Ok(result)
            }
            Err(err) => {
                let message = err.to_string();
                if !options.dry_run {
                    let _ = self
                        .db
                        .fail_issue_triage(&repo_name, issue.number as i64, &message)
                        .await;
                }
                Err(err)
            }
        }
    }

    async fn triage_issue_inner(
        &self,
        repo_cfg: &RepoConfig,
        issue: &Issue,
        options: IssueTriageOptions,
    ) -> Result<IssueTriageResult> {
        let repo_root = self.prepare_repo_root(repo_cfg).await;
        let repo_context = load_repo_triage_context(
            repo_root.as_deref(),
            repo_cfg.issue_triage.max_context_bytes.max(8 * 1024),
        );

        let label_catalog = match self
            .github
            .list_repo_labels(&repo_cfg.owner, &repo_cfg.name)
            .await
        {
            Ok(labels) => labels,
            Err(err) => {
                tracing::warn!(
                    repo = %repo_cfg.full_name(),
                    error = %err,
                    "failed to fetch repo labels for issue triage"
                );
                Vec::new()
            }
        };

        let issue_comments = if issue.comments > 0 {
            match self
                .github
                .get_issue_comments(&repo_cfg.owner, &repo_cfg.name, issue.number)
                .await
            {
                Ok(comments) => comments,
                Err(err) => {
                    tracing::warn!(
                        repo = %repo_cfg.full_name(),
                        issue = issue.number,
                        error = %err,
                        "failed to fetch issue comments for triage"
                    );
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        let mut prompt = build_issue_triage_prompt(
            repo_cfg,
            issue,
            &issue_comments,
            &label_catalog,
            &repo_context,
        );
        let prompt_original_bytes = prompt.len();
        let prompt_truncated =
            apply_prompt_size_limit(&mut prompt, self.config.defaults.max_prompt_bytes);
        let prompt_final_bytes = prompt.len();

        let harness_kind = repo_cfg.resolved_harness(&self.config);
        let harness = for_kind(harness_kind);
        let working_dir = tempdir().context("failed to create issue triage tempdir")?;
        let run = run_harness(
            harness.as_ref(),
            HarnessRunRequest {
                prompt,
                model: repo_cfg.resolved_model(&self.config).to_string(),
                reasoning_effort: repo_cfg.resolved_reasoning_effort(&self.config),
                working_dir: working_dir.path().to_path_buf(),
                timeout_secs: self.config.harness.timeout_secs,
            },
        )
        .await?;

        let decision = parse_issue_triage_output(&run.stdout, &run.stderr)?;
        let plan = build_label_plan(
            issue,
            &label_catalog,
            &decision,
            repo_cfg.issue_triage.create_missing_labels,
        )?;

        let mut created: Vec<String> = if options.dry_run {
            plan.labels_to_create
                .iter()
                .map(|label| label.name.clone())
                .collect()
        } else {
            Vec::new()
        };
        if !options.dry_run {
            for label in &plan.labels_to_create {
                let created_now = self
                    .github
                    .create_label(
                        &repo_cfg.owner,
                        &repo_cfg.name,
                        &label.name,
                        &normalize_color(&label.color),
                        label.description.as_deref(),
                    )
                    .await?;
                if created_now {
                    created.push(label.name.clone());
                }
            }
        }

        if !options.dry_run && !plan.labels_to_add.is_empty() {
            self.github
                .add_issue_labels(
                    &repo_cfg.owner,
                    &repo_cfg.name,
                    issue.number,
                    &plan.labels_to_add,
                )
                .await?;
        }

        tracing::info!(
            repo = %repo_cfg.full_name(),
            issue = issue.number,
            prompt_bytes_before = prompt_original_bytes,
            prompt_bytes_after = prompt_final_bytes,
            prompt_truncated,
            labels_added = ?plan.labels_to_add,
            labels_created = ?created,
            summary = %decision.summary,
            dry_run = options.dry_run,
            "issue triage completed"
        );

        Ok(IssueTriageResult {
            labels_added: plan.labels_to_add,
            labels_created: created,
            summary: decision.summary,
            dry_run: options.dry_run,
        })
    }

    async fn prepare_repo_root(&self, repo_cfg: &RepoConfig) -> Option<PathBuf> {
        let local = match repo_cfg.effective_local_path() {
            Ok(path) => path,
            Err(err) => {
                tracing::debug!(
                    repo = %repo_cfg.full_name(),
                    error = %err,
                    "issue triage repo context unavailable"
                );
                return None;
            }
        };

        if repo_cfg.is_managed() {
            if let Err(err) = crate::repo_manager::fetch_latest(&local, self.github.token()).await {
                tracing::warn!(
                    repo = %repo_cfg.full_name(),
                    error = %err,
                    "failed to fetch latest managed clone before issue triage"
                );
            }
        }

        Some(local)
    }
}

fn build_issue_triage_prompt(
    repo_cfg: &RepoConfig,
    issue: &Issue,
    comments: &[IssueComment],
    labels: &[Label],
    repo_context: &str,
) -> String {
    let mut prompt = String::new();

    prompt.push_str("Triage this GitHub issue using the repository's real conventions, roadmap, and label taxonomy. Do not sort by vibes.\n\n");
    prompt.push_str("Default buckets to reason with: active bugs, ops hardening, roadmap-fit features, needs-spec features, docs/docs-adjacent, backlog/discussion.\n");
    prompt.push_str("Default priority rules: P0 for broken core behavior/security/data loss/runaway cost, P1 for important regressions or high-value aligned features, P2 for worthwhile non-urgent improvements/docs, P3 for backlog and discussion-grade asks.\n\n");
    prompt.push_str("Prefer existing labels from the repository label list. Only propose new labels when the repo lacks an equivalent and only if the config allows label creation.\n\n");
    prompt.push_str(
        "Output a JSON object inside a fenced block tagged exactly `issue-triage-json`.\n",
    );
    prompt.push_str("Schema:\n");
    prompt.push_str("- summary: string, concise triage rationale\n");
    prompt.push_str("- labels_to_add: array of existing or desired labels to apply\n");
    prompt.push_str("- labels_to_create: array of {name, color, description}; leave empty unless truly needed\n\n");

    if !repo_cfg.issue_triage.create_missing_labels {
        prompt.push_str("Label creation is DISABLED for this repo. labels_to_create must be empty, and labels_to_add must only contain labels that already exist in the repo label catalog below.\n\n");
    } else {
        prompt.push_str("Label creation is ENABLED for this repo. When you need to create labels, prefer the repo's existing naming style. If the repo lacks a taxonomy, you may fall back to labels like bug, enhancement, documentation, question, priority: P0-P3, spec: planned, spec: needs-spec, bucket: ops-hardening, bucket: backlog. Use six-digit GitHub hex colors without '#'.\n\n");
    }

    prompt.push_str(&format!(
        "Repository: {}/{}\n",
        repo_cfg.owner, repo_cfg.name
    ));
    prompt.push_str(&format!("Issue: #{}\n", issue.number));
    prompt.push_str(&format!("Title: {}\n", issue.title));
    prompt.push_str(&format!("Author: {}\n", issue.user.login));
    if let Some(url) = issue.html_url.as_deref() {
        prompt.push_str(&format!("URL: {}\n", url));
    }
    if let Some(created_at) = issue.created_at.as_deref() {
        prompt.push_str(&format!("Created: {}\n", created_at));
    }
    if let Some(updated_at) = issue.updated_at.as_deref() {
        prompt.push_str(&format!("Updated: {}\n", updated_at));
    }

    if !issue.labels.is_empty() {
        let current = issue
            .labels
            .iter()
            .map(|label| label.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        prompt.push_str(&format!("Current labels: {}\n", current));
    }

    if let Some(custom) = repo_cfg.custom_instructions.as_deref() {
        if !custom.trim().is_empty() {
            prompt.push_str("\n## Custom Repository Instructions\n");
            prompt.push_str(custom);
            prompt.push('\n');
        }
    }

    if let Some(custom) = repo_cfg.issue_triage.instructions.as_deref() {
        if !custom.trim().is_empty() {
            prompt.push_str("\n## Issue Triage Instructions\n");
            prompt.push_str(custom);
            prompt.push('\n');
        }
    }

    prompt.push_str("\n## Repository Label Catalog\n");
    prompt.push_str(&format_label_catalog(labels));

    prompt.push_str("\n## Repository Context\n");
    prompt.push_str(repo_context);

    prompt.push_str("\n## Issue Body\n");
    prompt.push_str(issue.body.as_deref().unwrap_or("(no body provided)"));
    prompt.push('\n');

    prompt.push_str("\n## Recent Issue Comments\n");
    prompt.push_str(&format_issue_comments(comments));

    prompt
}

fn load_repo_triage_context(repo_root: Option<&Path>, max_bytes: usize) -> String {
    let Some(repo_root) = repo_root else {
        return "Local repository context unavailable. Triage using the issue and label catalog only.".to_string();
    };

    let mut out = String::new();
    let mut used = 0usize;

    for path in REPO_TRIAGE_FILES {
        if used >= max_bytes {
            break;
        }

        let remaining = max_bytes
            .saturating_sub(used)
            .min(MAX_SINGLE_CONTEXT_FILE_BYTES);
        if remaining == 0 {
            break;
        }

        match read_file_safe(repo_root, Path::new(path), remaining) {
            Ok(Some(content)) if !content.trim().is_empty() => {
                out.push_str(&format!("### {}\n{}\n\n", path, content.trim()));
                used = out.len();
            }
            Ok(_) => {}
            Err(err) => {
                tracing::debug!(file = path, error = %err, "failed to load repo triage file");
            }
        }
    }

    if out.is_empty() {
        "No repo instruction, roadmap, or spec files were found locally.".to_string()
    } else {
        out
    }
}

fn format_label_catalog(labels: &[Label]) -> String {
    if labels.is_empty() {
        return "(no existing labels found)".to_string();
    }

    let mut out = String::new();
    for label in labels {
        let desc = label.description.as_deref().unwrap_or("no description");
        out.push_str("- ");
        out.push_str(&label.name);
        out.push_str(": ");
        out.push_str(desc);
        if let Some(color) = label.color.as_deref() {
            out.push_str(" [");
            out.push_str(color);
            out.push(']');
        }
        out.push('\n');

        if out.len() >= MAX_LABEL_CATALOG_BYTES {
            truncate_utf8_to_max_bytes(&mut out, MAX_LABEL_CATALOG_BYTES);
            out.push_str("\n[label catalog truncated]");
            break;
        }
    }

    out
}

fn format_issue_comments(comments: &[IssueComment]) -> String {
    if comments.is_empty() {
        return "(no comments)".to_string();
    }

    let mut out = String::new();
    let tail = if comments.len() > 5 {
        &comments[comments.len() - 5..]
    } else {
        comments
    };

    for comment in tail {
        out.push_str(&format!(
            "- {} at {}\n{}\n\n",
            comment.user.login,
            comment.created_at,
            comment.body.trim()
        ));
        if out.len() >= MAX_COMMENT_CONTEXT_BYTES {
            truncate_utf8_to_max_bytes(&mut out, MAX_COMMENT_CONTEXT_BYTES);
            out.push_str("\n[comments truncated]");
            break;
        }
    }

    out
}

fn build_label_plan(
    issue: &Issue,
    existing_labels: &[Label],
    decision: &IssueTriageDecision,
    allow_create: bool,
) -> Result<LabelPlan> {
    let mut existing_by_key: BTreeMap<String, String> = existing_labels
        .iter()
        .map(|label| (normalize_label_key(&label.name), label.name.clone()))
        .collect();
    let current_issue_labels: BTreeSet<String> = issue
        .labels
        .iter()
        .map(|label| normalize_label_key(&label.name))
        .collect();
    let mut proposed_create: BTreeMap<String, ProposedLabel> = decision
        .labels_to_create
        .iter()
        .cloned()
        .map(|label| (normalize_label_key(&label.name), label))
        .collect();

    let mut labels_to_add = Vec::new();
    let mut labels_to_create = Vec::new();
    let mut seen_add = BTreeSet::new();
    let mut seen_create = BTreeSet::new();
    let mut unknown_requested = Vec::new();

    for raw in &decision.labels_to_add {
        let key = normalize_label_key(raw);
        if key.is_empty() || current_issue_labels.contains(&key) || !seen_add.insert(key.clone()) {
            continue;
        }

        if let Some(existing) = existing_by_key.get(&key) {
            labels_to_add.push(existing.clone());
            continue;
        }

        if allow_create {
            if let Some(created) = proposed_create.remove(&key) {
                if seen_create.insert(key.clone()) {
                    existing_by_key.insert(key.clone(), created.name.clone());
                    labels_to_add.push(created.name.clone());
                    labels_to_create.push(created);
                }
                continue;
            }
        }

        unknown_requested.push(raw.trim().to_string());
    }

    if !unknown_requested.is_empty() {
        return Err(anyhow!(
            "triage requested unknown labels without a valid creation plan: {}",
            unknown_requested.join(", ")
        ));
    }

    Ok(LabelPlan {
        labels_to_add,
        labels_to_create,
    })
}

fn parse_issue_triage_output(stdout: &str, stderr: &str) -> Result<IssueTriageDecision> {
    let combined = normalize_output(stdout, stderr);
    if combined.trim().is_empty() {
        return Err(anyhow!("issue triage harness returned empty output"));
    }

    if let Some(marked) = extract_marked_json(&combined, "issue-triage-json") {
        if let Ok(parsed) = parse_and_validate_issue_triage(&marked) {
            return Ok(parsed);
        }
    }

    let mut candidates = extract_json_objects(&combined);
    candidates.reverse();
    for candidate in candidates {
        if let Ok(parsed) = parse_and_validate_issue_triage(&candidate) {
            return Ok(parsed);
        }
    }

    Err(anyhow!(
        "failed to parse issue triage JSON from harness output"
    ))
}

fn parse_and_validate_issue_triage(json: &str) -> Result<IssueTriageDecision> {
    let parsed: IssueTriageDecision =
        serde_json::from_str(json).map_err(|e| anyhow!("issue triage JSON parse failed: {e}"))?;

    if parsed.summary.trim().is_empty() {
        return Err(anyhow!("issue triage summary cannot be empty"));
    }

    for label in &parsed.labels_to_add {
        if label.trim().is_empty() {
            return Err(anyhow!("labels_to_add cannot contain empty labels"));
        }
    }

    for label in &parsed.labels_to_create {
        if label.name.trim().is_empty() {
            return Err(anyhow!("labels_to_create entries need a name"));
        }
        let color = normalize_color(&label.color);
        if color.len() != 6 || !color.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(anyhow!(
                "labels_to_create colors must be six hex characters"
            ));
        }
    }

    Ok(parsed)
}

fn normalize_output(stdout: &str, stderr: &str) -> String {
    let out = stdout.trim();
    let err = stderr.trim();

    if !out.is_empty() && !err.is_empty() {
        format!("{out}\n\n[stderr]\n{err}")
    } else if !out.is_empty() {
        out.to_string()
    } else {
        err.to_string()
    }
}

fn extract_marked_json(text: &str, marker: &str) -> Option<String> {
    let fence = format!("```{marker}");
    let start = text.find(&fence)? + fence.len();
    let rest = &text[start..];
    let end = rest.find("```")?;
    Some(rest[..end].trim().to_string())
}

fn extract_json_objects(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(begin) = start.take() {
                        out.push(text[begin..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    out
}

fn normalize_label_key(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn normalize_color(raw: &str) -> String {
    raw.trim().trim_start_matches('#').to_ascii_lowercase()
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

fn apply_prompt_size_limit(prompt: &mut String, max_bytes: usize) -> bool {
    if prompt.len() <= max_bytes {
        return false;
    }

    if max_bytes > PROMPT_TRUNCATION_NOTE.len() {
        truncate_utf8_to_max_bytes(prompt, max_bytes - PROMPT_TRUNCATION_NOTE.len());
        prompt.push_str(PROMPT_TRUNCATION_NOTE);
    } else {
        truncate_utf8_to_max_bytes(prompt, max_bytes);
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_marked_issue_triage_json() {
        let parsed = parse_issue_triage_output(
            "```issue-triage-json\n{\"summary\":\"bug\",\"labels_to_add\":[\"bug\",\"priority: P1\"],\"labels_to_create\":[]}\n```",
            "",
        )
        .expect("parse");

        assert_eq!(parsed.summary, "bug");
        assert_eq!(parsed.labels_to_add.len(), 2);
    }

    #[test]
    fn build_label_plan_prefers_existing_labels() {
        let issue = Issue {
            number: 1,
            title: "title".to_string(),
            body: Some("body".to_string()),
            user: crate::github::types::User {
                login: "alice".to_string(),
            },
            labels: vec![],
            comments: 0,
            html_url: None,
            created_at: None,
            updated_at: None,
            pull_request: None,
        };
        let existing = vec![Label {
            name: "bug".to_string(),
            color: Some("d73a4a".to_string()),
            description: Some("Bug".to_string()),
        }];
        let decision = IssueTriageDecision {
            summary: "summary".to_string(),
            labels_to_add: vec!["bug".to_string(), "priority: P1".to_string()],
            labels_to_create: vec![ProposedLabel {
                name: "priority: P1".to_string(),
                color: "d93f0b".to_string(),
                description: Some("High priority".to_string()),
            }],
        };

        let plan = build_label_plan(&issue, &existing, &decision, true).expect("plan");
        assert_eq!(
            plan.labels_to_add,
            vec!["bug".to_string(), "priority: P1".to_string()]
        );
        assert_eq!(plan.labels_to_create.len(), 1);
    }

    #[test]
    fn build_label_plan_rejects_unknown_labels_when_creation_disabled() {
        let issue = Issue {
            number: 1,
            title: "title".to_string(),
            body: Some("body".to_string()),
            user: crate::github::types::User {
                login: "alice".to_string(),
            },
            labels: vec![],
            comments: 0,
            html_url: None,
            created_at: None,
            updated_at: None,
            pull_request: None,
        };
        let existing = vec![Label {
            name: "bug".to_string(),
            color: Some("d73a4a".to_string()),
            description: Some("Bug".to_string()),
        }];
        let decision = IssueTriageDecision {
            summary: "summary".to_string(),
            labels_to_add: vec!["priority: P1".to_string()],
            labels_to_create: vec![],
        };

        let err =
            build_label_plan(&issue, &existing, &decision, false).expect_err("should fail closed");
        assert!(err.to_string().contains("priority: P1"));
    }

    #[test]
    fn prompt_size_limit_truncates_with_note() {
        let mut prompt = "a".repeat(PROMPT_TRUNCATION_NOTE.len() + 32);

        let truncated = apply_prompt_size_limit(&mut prompt, PROMPT_TRUNCATION_NOTE.len() + 8);

        assert!(truncated);
        assert!(prompt.len() <= PROMPT_TRUNCATION_NOTE.len() + 8);
        assert!(prompt.ends_with(PROMPT_TRUNCATION_NOTE));
    }
}

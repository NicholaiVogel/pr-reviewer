use anyhow::{anyhow, Result};
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::config::{InstructionPrConfig, InstructionPrMode, RepoConfig};
use crate::github::client::GitHubClient;
use crate::store::db::RecurringFindingCandidate;

const AGENTS_PATH: &str = "AGENTS.md";
const SECTION_START: &str = "<!-- pr-reviewer recurring-guardrails:start -->";
const SECTION_END: &str = "<!-- pr-reviewer recurring-guardrails:end -->";

#[derive(Debug, Clone)]
pub struct InstructionPrOutcome {
    pub branch: String,
    pub pr_url: String,
}

pub async fn open_instruction_pr(
    github: &GitHubClient,
    repo_cfg: &RepoConfig,
    base_branch: &str,
    config: &InstructionPrConfig,
    candidate: &RecurringFindingCandidate,
) -> Result<Option<InstructionPrOutcome>> {
    let Some((agents, agents_sha)) = github
        .get_file_content_with_sha(&repo_cfg.owner, &repo_cfg.name, AGENTS_PATH, base_branch)
        .await?
    else {
        return Err(anyhow!(
            "cannot suggest recurring guardrail because {AGENTS_PATH} was not found"
        ));
    };

    let Some(updated_agents) = apply_agents_update(&agents, candidate, config.mode) else {
        return Ok(None);
    };

    let base_sha = github
        .get_branch_head_sha(&repo_cfg.owner, &repo_cfg.name, base_branch)
        .await?;
    let branch = instruction_branch_name(&config.branch_prefix, &candidate.fingerprint);
    github
        .create_branch(&repo_cfg.owner, &repo_cfg.name, &branch, &base_sha)
        .await?;
    github
        .update_file_content(
            &repo_cfg.owner,
            &repo_cfg.name,
            AGENTS_PATH,
            &branch,
            "docs: add recurring review guardrail",
            &updated_agents,
            Some(&agents_sha),
        )
        .await?;
    let pr_url = github
        .create_pull_request(
            &repo_cfg.owner,
            &repo_cfg.name,
            "docs: add recurring review guardrail",
            &instruction_pr_body(candidate, config.mode),
            &branch,
            base_branch,
        )
        .await?;

    Ok(Some(InstructionPrOutcome { branch, pr_url }))
}

pub fn recurring_finding_fingerprint(body: &str) -> String {
    let mut tokens = meaningful_tokens(body);
    tokens.sort();
    tokens.dedup();
    let signature = if tokens.is_empty() {
        normalize_text(body).chars().take(160).collect::<String>()
    } else {
        tokens.into_iter().take(12).collect::<Vec<_>>().join(" ")
    };
    let digest = Sha256::digest(signature.as_bytes());
    format!("{digest:x}").chars().take(16).collect()
}

fn apply_agents_update(
    agents: &str,
    candidate: &RecurringFindingCandidate,
    mode: InstructionPrMode,
) -> Option<String> {
    let fingerprint_marker = fingerprint_marker(&candidate.fingerprint);
    if agents.contains(&fingerprint_marker) {
        return None;
    }

    let entry = guardrail_entry(candidate, mode);
    if let (Some(start), Some(end)) = (agents.find(SECTION_START), agents.find(SECTION_END)) {
        if start >= end {
            return None;
        }
        let mut updated = String::new();
        updated.push_str(&agents[..end]);
        if !updated.ends_with("\n\n") {
            if updated.ends_with('\n') {
                updated.push('\n');
            } else {
                updated.push_str("\n\n");
            }
        }
        updated.push_str(&entry);
        updated.push('\n');
        updated.push_str(&agents[end..]);
        return Some(updated);
    }

    let mut updated = agents.trim_end().to_string();
    updated.push_str("\n\n");
    updated.push_str(SECTION_START);
    updated.push('\n');
    updated.push_str("## Recurring Review Guardrails\n\n");
    updated.push_str(&entry);
    updated.push('\n');
    updated.push_str(SECTION_END);
    updated.push('\n');
    Some(updated)
}

fn guardrail_entry(candidate: &RecurringFindingCandidate, mode: InstructionPrMode) -> String {
    let prs = candidate_pr_list(candidate);
    match mode {
        InstructionPrMode::Conservative => format!(
            "{}\n- Guard against this recurring review finding: {} This has appeared across {} distinct PRs ({} distinct author(s)): {}. Before merging similar code, verify the behavior with a focused test or leave an explicit rationale when the pattern is intentional.\n",
            fingerprint_marker(&candidate.fingerprint),
            candidate.summary.trim_end_matches('.'),
            candidate.distinct_prs,
            candidate.distinct_authors,
            prs,
        ),
        InstructionPrMode::Broad => format!(
            "{}\n### {}\n\nThis finding has recurred across {} distinct PRs ({} distinct author(s)): {}.\n\n- Review expectation: {}\n- Validation: require a focused behavioral test or an explicit rationale when the pattern is intentional.\n- Maintenance: update or remove this guardrail when the repo conventions, APIs, or documentation drift.\n",
            fingerprint_marker(&candidate.fingerprint),
            heading_from_summary(&candidate.summary),
            candidate.distinct_prs,
            candidate.distinct_authors,
            prs,
            candidate.summary.trim_end_matches('.'),
        ),
    }
}

fn instruction_pr_body(candidate: &RecurringFindingCandidate, mode: InstructionPrMode) -> String {
    let mut body = format!(
        "pr-reviewer saw the same finding across {} distinct PRs and is proposing an AGENTS.md guardrail.\n\nMode: `{}`\n\nExamples:\n",
        candidate.distinct_prs,
        mode,
    );
    for example in &candidate.examples {
        let line = example
            .line
            .map(|line| format!(":{line}"))
            .unwrap_or_default();
        body.push_str(&format!(
            "- #{} by @{} in {}{} ({}) - {}\n",
            example.pr_number,
            example.author,
            example.path,
            line,
            example.severity,
            truncate_chars(&example.body.replace('\n', " "), 220),
        ));
    }
    body
}

fn instruction_branch_name(prefix: &str, fingerprint: &str) -> String {
    let short = fingerprint.chars().take(12).collect::<String>();
    let raw = format!("{prefix}-{short}-{}", Utc::now().timestamp());
    raw.chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '/' | '-' | '_' | '.' => ch,
            _ => '-',
        })
        .collect()
}

fn fingerprint_marker(fingerprint: &str) -> String {
    format!("<!-- pr-reviewer:fingerprint={fingerprint} -->")
}

fn candidate_pr_list(candidate: &RecurringFindingCandidate) -> String {
    let mut prs = candidate
        .examples
        .iter()
        .map(|example| format!("#{}", example.pr_number))
        .collect::<Vec<_>>();
    prs.sort();
    prs.dedup();
    prs.join(", ")
}

fn heading_from_summary(summary: &str) -> String {
    let mut heading = summary
        .trim()
        .trim_end_matches('.')
        .trim_start_matches("This ")
        .trim_start_matches("this ")
        .to_string();
    if heading.is_empty() {
        heading = "Recurring Reviewer Finding".to_string();
    }
    let mut chars = heading.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
        None => "Recurring Reviewer Finding".to_string(),
    }
}

fn meaningful_tokens(text: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "this", "that", "with", "from", "into", "when", "will", "would", "should", "could", "also",
        "then", "than", "they", "them", "their", "about", "under", "over", "after", "before",
        "because", "which", "while", "where", "there", "here", "have", "has", "had", "does",
        "doesnt", "dont", "isnt", "cant", "only", "just", "being", "through", "still", "later",
        "every", "other", "again", "across", "agent", "scope", "scoped", "review", "reviewer",
        "blocking", "warning", "comment",
    ];

    normalize_text(text)
        .split_whitespace()
        .filter(|token| token.len() >= 4 && !STOPWORDS.contains(token))
        .map(ToString::to_string)
        .collect()
}

fn normalize_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db::{RecurringFindingCandidate, RecurringFindingExample};

    fn candidate() -> RecurringFindingCandidate {
        RecurringFindingCandidate {
            fingerprint: "abc123".to_string(),
            summary: "This can panic when the input is empty.".to_string(),
            distinct_prs: 2,
            distinct_authors: 1,
            examples: vec![
                RecurringFindingExample {
                    pr_number: 7,
                    author: "alice".to_string(),
                    path: "src/lib.rs".to_string(),
                    line: Some(12),
                    severity: "blocking".to_string(),
                    body: "This can panic when the input is empty.".to_string(),
                },
                RecurringFindingExample {
                    pr_number: 9,
                    author: "alice".to_string(),
                    path: "src/main.rs".to_string(),
                    line: Some(44),
                    severity: "warning".to_string(),
                    body: "Empty input can panic on unwrap.".to_string(),
                },
            ],
        }
    }

    #[test]
    fn fingerprint_is_order_insensitive_for_tokens() {
        let a = recurring_finding_fingerprint("Empty input can panic on unwrap.");
        let b = recurring_finding_fingerprint("unwrap can panic on empty input");
        assert_eq!(a, b);
    }

    #[test]
    fn conservative_update_appends_managed_section() {
        let updated =
            apply_agents_update("# Repo\n", &candidate(), InstructionPrMode::Conservative)
                .expect("update");
        assert!(updated.contains(SECTION_START));
        assert!(updated.contains("<!-- pr-reviewer:fingerprint=abc123 -->"));
        assert!(updated.contains("Guard against this recurring review finding"));
    }

    #[test]
    fn update_is_noop_when_fingerprint_already_exists() {
        let existing = format!(
            "# Repo\n\n{SECTION_START}\n## Recurring Review Guardrails\n\n{}\n{SECTION_END}\n",
            fingerprint_marker("abc123")
        );
        assert!(apply_agents_update(&existing, &candidate(), InstructionPrMode::Broad).is_none());
    }
}

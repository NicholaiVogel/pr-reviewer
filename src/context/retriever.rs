use globset::{Glob, GlobSet, GlobSetBuilder};

use anyhow::Result;

use crate::config::{DefaultsConfig, RepoConfig};
use crate::context::diff_parser::ParsedDiff;
use crate::github::client::GitHubClient;
use crate::github::types::PullRequest;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ContextMode {
    Full,
    Limited,
}

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub text: String,
    pub files_included: usize,
    pub diff_lines: usize,
}

pub async fn assemble_context(
    github: &GitHubClient,
    repo_cfg: &RepoConfig,
    pr: &PullRequest,
    diff_text: &str,
    parsed_diff: &ParsedDiff,
    defaults: &DefaultsConfig,
    mode: ContextMode,
    gitnexus_context: Option<&str>,
) -> Result<AssembledContext> {
    let mut body = String::new();
    let ignore = compile_ignore_globs(&repo_cfg.ignore_paths)?;

    body.push_str("# PR Metadata\n");
    body.push_str(&format!("Repo: {}/{}\n", repo_cfg.owner, repo_cfg.name));
    body.push_str(&format!("PR: #{}\n", pr.number));
    body.push_str(&format!("Title: {}\n", pr.title));
    body.push_str(&format!("Author: {}\n", pr.user.login));
    body.push_str(&format!("Head SHA: {}\n\n", pr.head.sha));

    if let Some(description) = pr.body.as_deref() {
        if !description.trim().is_empty() {
            body.push_str("## Description\n");
            body.push_str(description);
            body.push_str("\n\n");
        }
    }

    body.push_str("## Diff\n");
    body.push_str("```diff\n");
    body.push_str(&truncate_lines(diff_text, defaults.max_diff_lines));
    body.push_str("\n```\n\n");

    let mut files_included = 0usize;
    if mode == ContextMode::Full {
        body.push_str("## Changed File Contents\n");
        for file in &parsed_diff.files {
            let path = normalize_file_path(file);
            if path.is_empty() {
                continue;
            }
            if ignore.is_match(&path) {
                continue;
            }
            if files_included >= defaults.max_files {
                break;
            }

            let content = github
                .get_file_content(&repo_cfg.owner, &repo_cfg.name, &path, &pr.head.sha)
                .await?;

            let Some(content) = content else {
                continue;
            };

            if content.len() > defaults.max_file_size_bytes {
                continue;
            }

            body.push_str(&format!("### {}\n", path));
            body.push_str("```\n");
            body.push_str(&content);
            body.push_str("\n```\n\n");
            files_included += 1;
        }
    } else {
        body.push_str("## Note\nFork PR limited mode enabled: only diff context included.\n\n");
    }

    if let Some(extra) = gitnexus_context {
        if !extra.trim().is_empty() {
            body.push_str("## GitNexus Impact\n");
            body.push_str(extra);
            body.push_str("\n\n");
        }
    }

    if body.len() > defaults.max_prompt_bytes {
        truncate_utf8_to_max_bytes(&mut body, defaults.max_prompt_bytes);
        body.push_str("\n\n[context truncated due to prompt size limit]\n");
    }

    Ok(AssembledContext {
        text: body,
        files_included,
        diff_lines: parsed_diff.total_hunk_lines,
    })
}

fn compile_ignore_globs(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern)?);
    }
    Ok(builder.build()?)
}

fn truncate_lines(input: &str, max_lines: usize) -> String {
    let mut out = String::new();
    for (idx, line) in input.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... [diff truncated]\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn normalize_file_path(file: &crate::context::diff_parser::FileDiff) -> String {
    if file.new_path != "/dev/null" {
        file.new_path.clone()
    } else {
        file.old_path.clone()
    }
}

const CONVENTION_FILES: &[&str] = &["CLAUDE.md", "AGENTS.md", ".claude/instructions.md"];
const MAX_CONVENTION_BYTES: usize = 20 * 1024;

/// Try to fetch repository convention files (CLAUDE.md, AGENTS.md, .claude/instructions.md).
/// Returns the first one found, capped at 20KB.
pub async fn fetch_repo_conventions(
    github: &GitHubClient,
    owner: &str,
    name: &str,
    head_sha: &str,
) -> Option<String> {
    for path in CONVENTION_FILES {
        match github.get_file_content(owner, name, path, head_sha).await {
            Ok(Some(content)) if !content.trim().is_empty() => {
                let mut content = content;
                if content.len() > MAX_CONVENTION_BYTES {
                    truncate_utf8_to_max_bytes(&mut content, MAX_CONVENTION_BYTES);
                    content.push_str("\n[conventions truncated]");
                }
                tracing::info!(file = path, "loaded repo conventions");
                return Some(content);
            }
            _ => continue,
        }
    }
    None
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

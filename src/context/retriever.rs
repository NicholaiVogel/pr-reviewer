use std::collections::HashSet;

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
    pub related_files_included: usize,
    pub diff_lines: usize,
    pub bytes_total: usize,
    pub truncated: bool,
}

const MAX_RELATED_FILES: usize = 8;
const MAX_RELATED_FILE_BYTES: usize = 12 * 1024;
const MAX_RELATED_FILE_LINES: usize = 220;
const MAX_GITNEXUS_CONTEXT_BYTES: usize = 32 * 1024;

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
    let mut truncated = false;
    let mut remaining = defaults.max_prompt_bytes;
    let ignore = compile_ignore_globs(&repo_cfg.ignore_paths)?;
    let mut included_paths: HashSet<String> = HashSet::new();

    push_with_budget(&mut body, "# PR Metadata\n", &mut remaining, &mut truncated);
    push_with_budget(
        &mut body,
        &format!("Repo: {}/{}\n", repo_cfg.owner, repo_cfg.name),
        &mut remaining,
        &mut truncated,
    );
    push_with_budget(
        &mut body,
        &format!("PR: #{}\n", pr.number),
        &mut remaining,
        &mut truncated,
    );
    push_with_budget(
        &mut body,
        &format!("Title: {}\n", pr.title),
        &mut remaining,
        &mut truncated,
    );
    push_with_budget(
        &mut body,
        &format!("Author: {}\n", pr.user.login),
        &mut remaining,
        &mut truncated,
    );
    push_with_budget(
        &mut body,
        &format!("Head SHA: {}\n\n", pr.head.sha),
        &mut remaining,
        &mut truncated,
    );

    if let Some(description) = pr.body.as_deref() {
        if !description.trim().is_empty() {
            push_with_budget(
                &mut body,
                "## Description\n",
                &mut remaining,
                &mut truncated,
            );
            push_with_budget(&mut body, description, &mut remaining, &mut truncated);
            push_with_budget(&mut body, "\n\n", &mut remaining, &mut truncated);
        }
    }

    push_with_budget(&mut body, "## Diff\n", &mut remaining, &mut truncated);
    push_with_budget(&mut body, "```diff\n", &mut remaining, &mut truncated);
    push_with_budget(
        &mut body,
        &truncate_lines(diff_text, defaults.max_diff_lines),
        &mut remaining,
        &mut truncated,
    );
    push_with_budget(&mut body, "\n```\n\n", &mut remaining, &mut truncated);

    let mut files_included = 0usize;
    let mut related_files_included = 0usize;
    if mode == ContextMode::Full {
        push_with_budget(
            &mut body,
            "## Changed File Contents\n",
            &mut remaining,
            &mut truncated,
        );
        for file in &parsed_diff.files {
            if remaining == 0 {
                break;
            }
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

            let snippet = truncate_file_for_prompt(
                &content,
                defaults.max_file_size_bytes,
                defaults.max_diff_lines,
            );
            push_with_budget(
                &mut body,
                &format!("### {}\n", path),
                &mut remaining,
                &mut truncated,
            );
            push_with_budget(&mut body, "```\n", &mut remaining, &mut truncated);
            push_with_budget(&mut body, &snippet, &mut remaining, &mut truncated);
            push_with_budget(&mut body, "\n```\n\n", &mut remaining, &mut truncated);
            files_included += 1;
            included_paths.insert(path);
        }
    } else {
        push_with_budget(
            &mut body,
            "## Note\nFork PR limited mode enabled: only diff context included.\n\n",
            &mut remaining,
            &mut truncated,
        );
    }

    if mode == ContextMode::Full {
        if let Some(extra) = gitnexus_context {
            if !extra.trim().is_empty() {
                let related_paths = extract_related_paths_from_gitnexus(extra);
                if !related_paths.is_empty() {
                    push_with_budget(
                        &mut body,
                        "## Related Code Context (GitNexus)\n",
                        &mut remaining,
                        &mut truncated,
                    );
                }

                for path in related_paths {
                    if remaining == 0 || related_files_included >= MAX_RELATED_FILES {
                        break;
                    }
                    if included_paths.contains(path.as_str()) || ignore.is_match(path.as_str()) {
                        continue;
                    }

                    let content = match github
                        .get_file_content(&repo_cfg.owner, &repo_cfg.name, &path, &pr.head.sha)
                        .await
                    {
                        Ok(content) => content,
                        Err(err) => {
                            tracing::debug!(
                                repo = %repo_cfg.full_name(),
                                pr = pr.number,
                                path = %path,
                                error = %err,
                                "failed to fetch related gitnexus file context; skipping"
                            );
                            continue;
                        }
                    };

                    let Some(content) = content else {
                        continue;
                    };

                    let snippet = truncate_file_for_prompt(
                        &content,
                        MAX_RELATED_FILE_BYTES,
                        MAX_RELATED_FILE_LINES,
                    );
                    if snippet.trim().is_empty() {
                        continue;
                    }

                    push_with_budget(
                        &mut body,
                        &format!("### {}\n", path),
                        &mut remaining,
                        &mut truncated,
                    );
                    push_with_budget(&mut body, "```\n", &mut remaining, &mut truncated);
                    push_with_budget(&mut body, &snippet, &mut remaining, &mut truncated);
                    push_with_budget(&mut body, "\n```\n\n", &mut remaining, &mut truncated);
                    related_files_included += 1;
                    included_paths.insert(path);
                }
            }
        }
    }

    if let Some(extra) = gitnexus_context {
        if !extra.trim().is_empty() {
            let mut section = extra.to_string();
            if section.len() > MAX_GITNEXUS_CONTEXT_BYTES {
                truncate_utf8_to_max_bytes(&mut section, MAX_GITNEXUS_CONTEXT_BYTES);
                section.push_str("\n[gitnexus context truncated]");
            }
            push_with_budget(
                &mut body,
                "## GitNexus Impact\n",
                &mut remaining,
                &mut truncated,
            );
            push_with_budget(&mut body, &section, &mut remaining, &mut truncated);
            push_with_budget(&mut body, "\n\n", &mut remaining, &mut truncated);
        }
    }

    if truncated {
        push_with_budget(
            &mut body,
            "\n\n[context truncated due to prompt size limit]\n",
            &mut remaining,
            &mut truncated,
        );
    }

    tracing::info!(
        repo = %repo_cfg.full_name(),
        pr = pr.number,
        files_included,
        related_files_included,
        context_bytes = body.len(),
        truncated,
        "assembled review context"
    );

    Ok(AssembledContext {
        text: body,
        files_included,
        related_files_included,
        diff_lines: parsed_diff.total_hunk_lines,
        bytes_total: defaults.max_prompt_bytes.saturating_sub(remaining),
        truncated,
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

fn truncate_file_for_prompt(input: &str, max_bytes: usize, max_lines: usize) -> String {
    let mut out = String::new();
    for (idx, line) in input.lines().enumerate() {
        if idx >= max_lines {
            out.push_str("... [file truncated by line budget]\n");
            break;
        }
        out.push_str(line);
        out.push('\n');
    }

    if out.is_empty() {
        return out;
    }

    if out.len() > max_bytes {
        truncate_utf8_to_max_bytes(&mut out, max_bytes);
        out.push_str("\n... [file truncated by byte budget]\n");
    }

    out
}

fn push_with_budget(
    body: &mut String,
    chunk: &str,
    remaining: &mut usize,
    truncated: &mut bool,
) -> bool {
    if *remaining == 0 {
        *truncated = true;
        return false;
    }

    if chunk.len() <= *remaining {
        body.push_str(chunk);
        *remaining -= chunk.len();
        return true;
    }

    let mut partial = chunk.to_string();
    truncate_utf8_to_max_bytes(&mut partial, *remaining);
    body.push_str(&partial);
    *remaining = 0;
    *truncated = true;
    false
}

fn extract_related_paths_from_gitnexus(gitnexus_context: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for raw in gitnexus_context.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }

        let mut candidate: Option<String> = None;

        // Line style: - `symbol` in `path/to/file.rs`
        if let Some(after_in) = line.split(" in `").nth(1) {
            if let Some(path) = after_in.split('`').next() {
                candidate = Some(path.trim().to_string());
            }
        }

        // Line style: #### `symbol` (`path/to/file.rs`)
        if candidate.is_none() {
            if let Some(start) = line.find("(`") {
                let after = &line[start + 2..];
                if let Some(path) = after.split('`').next() {
                    candidate = Some(path.trim().to_string());
                }
            }
        }

        if let Some(path) = candidate {
            if path.is_empty() {
                continue;
            }
            if seen.insert(path.clone()) {
                out.push(path);
            }
        }
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

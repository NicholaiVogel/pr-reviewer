use std::collections::HashSet;
use std::path::Path;
use std::process::Output;
use std::time::Duration;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::timeout;

const QUERY_TIMEOUT_SECS: u64 = 20;
const STATUS_TIMEOUT_SECS: u64 = 8;
const SYMBOL_CONTEXT_TIMEOUT_SECS: u64 = 6;
const SYMBOL_IMPACT_TIMEOUT_SECS: u64 = 4;
const ANALYZE_TIMEOUT_SECS: u64 = 1800;
const MAX_QUERY_PROCESSES: usize = 4;
const MAX_ENRICHED_SYMBOLS: usize = 3;
const MAX_LIST_ITEMS: usize = 6;

#[derive(Debug, Clone, Default)]
pub struct GitNexusQueryContext {
    pub text: Option<String>,
    pub used: bool,
    pub latency_ms: i64,
    pub hit_count: i64,
}

pub fn has_index(repo_root: &Path) -> bool {
    repo_root.join(".gitnexus").exists()
}

pub async fn run_analyze(repo_root: &Path) -> Result<()> {
    let args = vec!["analyze".to_string()];
    let output = run_gitnexus(repo_root, &args, ANALYZE_TIMEOUT_SECS)
        .await
        .context("failed to execute gitnexus analyze")?;
    let status = output.status;

    if !status.success() {
        let details = output_text(&output);
        anyhow::bail!("gitnexus analyze failed with status: {status}; output: {details}");
    }
    Ok(())
}

pub async fn query_context_with_metrics(
    repo_root: &Path,
    repo_name: &str,
    files: &[String],
) -> Result<GitNexusQueryContext> {
    let start = Instant::now();
    if !has_index(repo_root) {
        return Ok(GitNexusQueryContext::default());
    }

    if files.is_empty() {
        return Ok(GitNexusQueryContext::default());
    }

    // Build a search query from the changed file names (strip paths, extensions)
    let file_names: Vec<&str> = files
        .iter()
        .filter_map(|f| Path::new(f).file_stem())
        .filter_map(|s| s.to_str())
        .collect();

    if file_names.is_empty() {
        return Ok(GitNexusQueryContext::default());
    }

    let search_query = format!("changes to {}", file_names.join(", "));
    let query_context = format!("changed files: {}", files.join(", "));

    let args = vec![
        "query".to_string(),
        "-r".to_string(),
        repo_name.to_string(),
        "-l".to_string(),
        "8".to_string(),
        "-c".to_string(),
        query_context,
        "-g".to_string(),
        "identify execution flow and blast radius related to these changes".to_string(),
        search_query,
    ];

    let output = match run_gitnexus(repo_root, &args, QUERY_TIMEOUT_SECS).await {
        Ok(output) => output,
        Err(_) => return Ok(GitNexusQueryContext::default()),
    };

    if !output.status.success() {
        return Ok(GitNexusQueryContext::default());
    }

    let text = output_text(&output);
    if text.is_empty() {
        return Ok(GitNexusQueryContext {
            used: true,
            latency_ms: start.elapsed().as_millis() as i64,
            ..Default::default()
        });
    }

    let hit_count = estimate_hit_count(&text);
    let structured = match build_structured_query_context(repo_root, repo_name, files, &text).await
    {
        Ok(Some(value)) => value,
        Ok(None) => text,
        Err(_) => text,
    };

    Ok(GitNexusQueryContext {
        text: Some(structured),
        used: true,
        latency_ms: start.elapsed().as_millis() as i64,
        hit_count,
    })
}

fn estimate_hit_count(query_text: &str) -> i64 {
    let parsed: GitNexusQueryResponse = match serde_json::from_str(query_text) {
        Ok(value) => value,
        Err(_) => return 0,
    };
    (parsed.processes.len() + parsed.process_symbols.len()) as i64
}

pub async fn is_index_stale(repo_root: &Path) -> Result<Option<bool>> {
    if !has_index(repo_root) {
        return Ok(None);
    }

    let args = vec!["status".to_string()];
    let output = match run_gitnexus(repo_root, &args, STATUS_TIMEOUT_SECS).await {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };

    if !output.status.success() {
        return Ok(None);
    }

    Ok(parse_stale_from_status(&output_text(&output)))
}

async fn run_gitnexus(repo_root: &Path, args: &[String], timeout_secs: u64) -> Result<Output> {
    match run_gitnexus_bin("gitnexus", repo_root, args, timeout_secs).await {
        Ok(output) => Ok(output),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            run_gitnexus_npx(repo_root, args, timeout_secs)
                .await
                .context("failed to execute gitnexus via npx")
        }
        Err(err) => Err(err).context("failed to execute gitnexus"),
    }
}

async fn run_gitnexus_npx(repo_root: &Path, args: &[String], timeout_secs: u64) -> Result<Output> {
    let mut cmd = Command::new("npx");
    cmd.arg("gitnexus");
    for arg in args {
        cmd.arg(arg);
    }
    cmd.current_dir(repo_root);
    Ok(run_command_output(cmd, timeout_secs).await?)
}

async fn run_gitnexus_bin(
    bin: &str,
    repo_root: &Path,
    args: &[String],
    timeout_secs: u64,
) -> std::io::Result<Output> {
    let mut cmd = Command::new(bin);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.current_dir(repo_root);
    run_command_output(cmd, timeout_secs).await
}

async fn run_command_output(mut cmd: Command, timeout_secs: u64) -> std::io::Result<Output> {
    timeout(Duration::from_secs(timeout_secs), cmd.output())
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "command timed out"))?
}

fn output_text(output: &Output) -> String {
    let stderr_text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stderr_text.is_empty() {
        stderr_text
    } else {
        stdout_text
    }
}

fn parse_stale_from_status(status_output: &str) -> Option<bool> {
    let lower = status_output.to_ascii_lowercase();
    if lower.contains("status:") && lower.contains("stale") {
        return Some(true);
    }
    if lower.contains("status:") && (lower.contains("fresh") || lower.contains("up-to-date")) {
        return Some(false);
    }
    None
}

async fn build_structured_query_context(
    repo_root: &Path,
    repo_name: &str,
    changed_files: &[String],
    query_text: &str,
) -> Result<Option<String>> {
    let parsed: GitNexusQueryResponse = match serde_json::from_str(query_text) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };

    let symbols = select_top_changed_symbols(&parsed, changed_files);

    let mut out = String::new();
    if !parsed.processes.is_empty() {
        out.push_str("### Ranked Processes\n");
        for proc in parsed.processes.iter().take(MAX_QUERY_PROCESSES) {
            if proc.summary.trim().is_empty() {
                continue;
            }
            out.push_str(&format!(
                "- {} (priority {:.3}, steps {})\n",
                proc.summary, proc.priority, proc.step_count
            ));
        }
        out.push('\n');
    }

    if !symbols.is_empty() {
        out.push_str("### Top Changed Symbols\n");
        for symbol in &symbols {
            out.push_str(&format!("- `{}` in `{}`\n", symbol.name, symbol.file_path));
        }
        out.push('\n');

        out.push_str("### Symbol Context\n");
        for symbol in symbols {
            let context_json = fetch_symbol_context(repo_root, repo_name, &symbol).await;
            let impact_json = fetch_symbol_impact(repo_root, repo_name, &symbol).await;

            let parsed_context = context_json
                .as_deref()
                .and_then(parse_symbol_context)
                .unwrap_or_default();
            let impact = impact_json
                .as_deref()
                .and_then(parse_symbol_impact)
                .unwrap_or_default();

            out.push_str(&format!(
                "#### `{}` (`{}`)\n",
                symbol.name, symbol.file_path
            ));

            let callers = first_items(&parsed_context.callers, MAX_LIST_ITEMS);
            if callers.is_empty() {
                out.push_str("- Callers: none\n");
            } else {
                out.push_str(&format!("- Callers: {}\n", callers.join(", ")));
            }

            let callees = first_items(&parsed_context.callees, MAX_LIST_ITEMS);
            if callees.is_empty() {
                out.push_str("- Callees: none\n");
            } else {
                out.push_str(&format!("- Callees: {}\n", callees.join(", ")));
            }

            let procs = first_items(&parsed_context.processes, MAX_LIST_ITEMS);
            if procs.is_empty() {
                out.push_str("- Related processes: none\n");
            } else {
                out.push_str(&format!("- Related processes: {}\n", procs.join(" | ")));
            }

            let impact_items = first_items(&impact, MAX_LIST_ITEMS);
            if impact_items.is_empty() {
                out.push_str("- Impact (upstream depth=2): unavailable\n");
            } else {
                out.push_str(&format!(
                    "- Impact (upstream depth=2): {}\n",
                    impact_items.join(", ")
                ));
            }
            out.push('\n');
        }
    }

    if out.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(out.trim().to_string()))
}

async fn fetch_symbol_context(
    repo_root: &Path,
    repo_name: &str,
    symbol: &SymbolCandidate,
) -> Option<String> {
    let mut args = vec![
        "context".to_string(),
        "-r".to_string(),
        repo_name.to_string(),
        symbol.name.clone(),
    ];
    if !symbol.file_path.trim().is_empty() {
        args.push("-f".to_string());
        args.push(symbol.file_path.clone());
    }

    let output = run_gitnexus(repo_root, &args, SYMBOL_CONTEXT_TIMEOUT_SECS)
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = output_text(&output);
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

async fn fetch_symbol_impact(
    repo_root: &Path,
    repo_name: &str,
    symbol: &SymbolCandidate,
) -> Option<String> {
    let args = vec![
        "impact".to_string(),
        "-r".to_string(),
        repo_name.to_string(),
        "--depth".to_string(),
        "2".to_string(),
        symbol.name.clone(),
    ];

    let output = run_gitnexus(repo_root, &args, SYMBOL_IMPACT_TIMEOUT_SECS)
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = output_text(&output);
    if text.trim().is_empty() {
        return None;
    }
    Some(text)
}

fn select_top_changed_symbols(
    parsed: &GitNexusQueryResponse,
    changed_files: &[String],
) -> Vec<SymbolCandidate> {
    let changed: Vec<String> = changed_files.iter().map(|f| normalize_path(f)).collect();
    let mut selected = Vec::new();
    let mut seen = HashSet::new();

    for symbol in &parsed.process_symbols {
        if !path_matches_changed_files(&symbol.file_path, &changed) {
            continue;
        }
        if symbol.name.trim().is_empty() || symbol.file_path.trim().is_empty() {
            continue;
        }
        if symbol.id.starts_with("File:") {
            continue;
        }
        if symbol.file_path.starts_with("references/") {
            continue;
        }

        let key = format!("{}::{}", symbol.name, normalize_path(&symbol.file_path));
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        selected.push(SymbolCandidate {
            name: symbol.name.clone(),
            file_path: symbol.file_path.clone(),
        });
        if selected.len() >= MAX_ENRICHED_SYMBOLS {
            return selected;
        }
    }

    for symbol in &parsed.process_symbols {
        if symbol.name.trim().is_empty() || symbol.file_path.trim().is_empty() {
            continue;
        }
        if symbol.id.starts_with("File:") {
            continue;
        }
        if symbol.file_path.starts_with("references/") {
            continue;
        }
        let key = format!("{}::{}", symbol.name, normalize_path(&symbol.file_path));
        if seen.contains(&key) {
            continue;
        }
        seen.insert(key);
        selected.push(SymbolCandidate {
            name: symbol.name.clone(),
            file_path: symbol.file_path.clone(),
        });
        if selected.len() >= MAX_ENRICHED_SYMBOLS {
            break;
        }
    }

    selected
}

fn parse_symbol_context(text: &str) -> Option<SymbolContextSummary> {
    let parsed: GitNexusContextResponse = serde_json::from_str(text).ok()?;
    if parsed.status.eq_ignore_ascii_case("not_found") {
        return None;
    }

    let callers = parsed
        .incoming
        .calls
        .into_iter()
        .map(|c| format!("`{}` ({})", c.name, c.file_path))
        .collect();
    let callees = parsed
        .outgoing
        .calls
        .into_iter()
        .map(|c| format!("`{}` ({})", c.name, c.file_path))
        .collect();
    let processes = parsed.processes.into_iter().map(|p| p.name).collect();

    Some(SymbolContextSummary {
        callers,
        callees,
        processes,
    })
}

fn parse_symbol_impact(text: &str) -> Option<Vec<String>> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("error").is_some() {
        return None;
    }

    let mut out = Vec::new();
    collect_named_items(value.get("upstream"), &mut out);
    collect_named_items(value.get("downstream"), &mut out);
    collect_named_items(value.get("impacted"), &mut out);
    collect_named_items(value.get("affected"), &mut out);
    collect_named_items(value.get("results"), &mut out);
    collect_named_items(value.get("nodes"), &mut out);
    dedupe_keep_order(&mut out);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn collect_named_items(value: Option<&Value>, out: &mut Vec<String>) {
    let Some(value) = value else {
        return;
    };
    let Some(items) = value.as_array() else {
        return;
    };

    for item in items {
        if let Some(name) = item.get("name").and_then(Value::as_str) {
            out.push(format!("`{name}`"));
            continue;
        }
        if let Some(uid) = item.get("uid").and_then(Value::as_str) {
            out.push(format!("`{uid}`"));
            continue;
        }
        if let Some(target) = item.get("target").and_then(Value::as_str) {
            out.push(format!("`{target}`"));
        }
    }
}

fn path_matches_changed_files(path: &str, changed: &[String]) -> bool {
    let normalized = normalize_path(path);
    if changed.iter().any(|c| c == &normalized) {
        return true;
    }
    changed
        .iter()
        .any(|c| normalized.ends_with(&format!("/{c}")) || c.ends_with(&format!("/{normalized}")))
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .to_ascii_lowercase()
}

fn dedupe_keep_order(values: &mut Vec<String>) {
    let mut seen = HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn first_items(values: &[String], max: usize) -> Vec<String> {
    values.iter().take(max).cloned().collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusQueryResponse {
    #[serde(default)]
    processes: Vec<GitNexusProcess>,
    #[serde(default)]
    process_symbols: Vec<GitNexusProcessSymbol>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusProcess {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    priority: f64,
    #[serde(default)]
    step_count: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusProcessSymbol {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "filePath", default)]
    file_path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusContextResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    incoming: GitNexusContextCalls,
    #[serde(default)]
    outgoing: GitNexusContextCalls,
    #[serde(default)]
    processes: Vec<GitNexusContextProcess>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusContextCalls {
    #[serde(default)]
    calls: Vec<GitNexusCall>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusCall {
    #[serde(default)]
    name: String,
    #[serde(rename = "filePath", default)]
    file_path: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GitNexusContextProcess {
    #[serde(default)]
    name: String,
}

#[derive(Debug, Clone)]
struct SymbolCandidate {
    name: String,
    file_path: String,
}

#[derive(Debug, Clone, Default)]
struct SymbolContextSummary {
    callers: Vec<String>,
    callees: Vec<String>,
    processes: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_path, parse_stale_from_status, parse_symbol_impact, path_matches_changed_files,
    };

    #[test]
    fn parses_stale_status() {
        let text = "Status: stale (re-run gitnexus analyze)";
        assert_eq!(parse_stale_from_status(text), Some(true));
    }

    #[test]
    fn parses_fresh_status() {
        let text = "Status: fresh";
        assert_eq!(parse_stale_from_status(text), Some(false));
    }

    #[test]
    fn returns_none_for_unknown_output() {
        let text = "Repository not indexed";
        assert_eq!(parse_stale_from_status(text), None);
    }

    #[test]
    fn path_matching_handles_relative_and_prefix_paths() {
        let changed = vec![normalize_path("src/review/engine.rs")];
        assert!(path_matches_changed_files(
            "./src/review/engine.rs",
            &changed
        ));
        assert!(path_matches_changed_files(
            "/mnt/work/dev/pr-reviewer/src/review/engine.rs",
            &changed
        ));
        assert!(!path_matches_changed_files("src/main.rs", &changed));
    }

    #[test]
    fn impact_parser_reads_common_array_shapes() {
        let json = r#"{
          "upstream": [{"name":"run_review_pipeline"}, {"uid":"Function:src/a.rs:b:1"}],
          "affected": [{"target":"assemble_context"}]
        }"#;
        let parsed = parse_symbol_impact(json).expect("impact parsed");
        assert_eq!(
            parsed,
            vec![
                "`run_review_pipeline`".to_string(),
                "`Function:src/a.rs:b:1`".to_string(),
                "`assemble_context`".to_string()
            ]
        );
    }
}

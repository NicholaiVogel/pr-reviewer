use std::path::Path;

use anyhow::{Context, Result};
use tokio::process::Command;

pub fn has_index(repo_root: &Path) -> bool {
    repo_root.join(".gitnexus").exists()
}

pub async fn run_analyze(repo_root: &Path) -> Result<()> {
    let status = Command::new("gitnexus")
        .arg("analyze")
        .current_dir(repo_root)
        .status()
        .await
        .context("failed to execute gitnexus analyze")?;

    if !status.success() {
        anyhow::bail!("gitnexus analyze failed with status: {status}");
    }
    Ok(())
}

/// Query GitNexus for execution flow context related to changed files.
///
/// Uses `gitnexus query` to find processes and symbols related to the changed
/// files. GitNexus outputs to stderr (KuzuDB captures stdout at OS level).
pub async fn query_context(repo_root: &Path, files: &[String]) -> Result<Option<String>> {
    if !has_index(repo_root) {
        return Ok(None);
    }

    if files.is_empty() {
        return Ok(None);
    }

    // Build a search query from the changed file names (strip paths, extensions)
    let file_names: Vec<&str> = files
        .iter()
        .filter_map(|f| Path::new(f).file_stem())
        .filter_map(|s| s.to_str())
        .collect();

    if file_names.is_empty() {
        return Ok(None);
    }

    let search_query = format!("changes to {}", file_names.join(", "));

    let output = match Command::new("gitnexus")
        .arg("query")
        .arg(&search_query)
        .current_dir(repo_root)
        .output()
        .await
    {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };

    if !output.status.success() {
        return Ok(None);
    }

    // GitNexus currently outputs to stderr because KuzuDB captures stdout at OS level.
    // Check both streams so this doesn't silently break if that behavior changes.
    let stderr_text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout_text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let text = if !stderr_text.is_empty() {
        stderr_text
    } else {
        stdout_text
    };
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

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

pub async fn query_context(repo_root: &Path, files: &[String]) -> Result<Option<String>> {
    if !has_index(repo_root) {
        return Ok(None);
    }

    let mut cmd = Command::new("gitnexus");
    cmd.arg("query")
        .arg("impact")
        .arg("--format")
        .arg("text")
        .current_dir(repo_root);

    for file in files {
        cmd.arg("--file").arg(file);
    }

    let output = match cmd.output().await {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(text))
}

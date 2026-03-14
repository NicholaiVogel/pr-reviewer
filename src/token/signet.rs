use anyhow::{anyhow, Context, Result};
use tokio::process::Command;
use std::process::Stdio;

const SECRET_NAME: &str = "pr-reviewer/github-token";

/// Check if the `signet` CLI is available on PATH.
pub async fn is_available() -> bool {
    Command::new("signet")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Retrieve the GitHub token from Signet's secret store.
/// Returns `Ok(None)` if signet is not installed or the secret does not exist.
pub async fn get_token() -> Result<Option<String>> {
    if !is_available().await {
        return Ok(None);
    }

    let output = Command::new("signet")
        .args(["secret", "get", SECRET_NAME])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run signet secret get")?;

    if !output.status.success() {
        // Secret doesn't exist or signet errored — not a fatal error
        return Ok(None);
    }

    let token = String::from_utf8(output.stdout)
        .context("signet secret output is not valid UTF-8")?
        .trim()
        .to_string();

    if token.is_empty() {
        Ok(None)
    } else {
        Ok(Some(token))
    }
}

/// Store the GitHub token in Signet's secret store.
pub async fn store_token(token: &str) -> Result<()> {
    if !is_available().await {
        return Err(anyhow!("signet CLI not found on PATH"));
    }

    let output = Command::new("signet")
        .args(["secret", "set", SECRET_NAME, "--value", token])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run signet secret set")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("signet secret set failed: {}", stderr.trim()));
    }

    Ok(())
}

/// Delete the token from Signet's secret store.
pub async fn delete_token() -> Result<()> {
    if !is_available().await {
        return Ok(());
    }

    let output = Command::new("signet")
        .args(["secret", "delete", SECRET_NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .context("failed to run signet secret delete")?;

    if !output.status.success() {
        // Not fatal — secret may not have existed
        tracing::debug!("signet secret delete returned non-zero (secret may not exist)");
    }

    Ok(())
}

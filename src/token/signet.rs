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
/// Pipes the token via stdin to avoid leaking it in /proc/pid/cmdline.
pub async fn store_token(token: &str) -> Result<()> {
    if !is_available().await {
        return Err(anyhow!("signet CLI not found on PATH"));
    }

    let mut child = Command::new("signet")
        .args(["secret", "set", SECRET_NAME, "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn signet secret set")?;

    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        stdin
            .write_all(token.as_bytes())
            .await
            .context("failed to write token to signet stdin")?;
        // Drop stdin to close the pipe and signal EOF
    }

    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for signet secret set")?;

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

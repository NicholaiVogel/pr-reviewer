use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::time::timeout;

use crate::config::ReasoningEffort;
use crate::harness::codex;
use crate::harness::Harness;

const MAX_STDOUT_BYTES: usize = 1_048_576;
const MAX_STDERR_BYTES: usize = 262_144;

#[derive(Debug, Clone)]
pub struct HarnessRunRequest {
    pub prompt: String,
    pub model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub working_dir: PathBuf,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct HarnessRunOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    pub duration_secs: f64,
}

pub async fn run_harness(
    harness: &dyn Harness,
    req: HarnessRunRequest,
) -> Result<HarnessRunOutput> {
    let mut command = harness.build_command(
        &req.prompt,
        &req.model,
        req.reasoning_effort,
        &req.working_dir,
    );
    command.current_dir(&req.working_dir);

    scrub_environment(&mut command);

    let start = Instant::now();

    let output = if harness.uses_stdin() {
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        command.kill_on_drop(true);
        let mut child = command.spawn().context("failed to spawn harness")?;
        if let Some(mut stdin) = child.stdin.take() {
            let prompt = req.prompt.clone();
            tokio::spawn(async move {
                if let Err(e) = stdin.write_all(prompt.as_bytes()).await {
                    tracing::warn!("harness stdin write failed: {e}");
                }
                if let Err(e) = stdin.shutdown().await {
                    tracing::warn!("harness stdin shutdown failed: {e}");
                }
            });
        }
        timeout(
            Duration::from_secs(req.timeout_secs),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| anyhow!("harness timed out after {}s", req.timeout_secs))?
        .context("failed to wait on harness")?
    } else {
        timeout(Duration::from_secs(req.timeout_secs), command.output())
            .await
            .map_err(|_| anyhow!("harness timed out after {}s", req.timeout_secs))?
            .context("failed to spawn harness")?
    };

    let duration = start.elapsed().as_secs_f64();

    let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if let Ok(last_message) =
        tokio::fs::read_to_string(codex::last_message_path(&req.working_dir)).await
    {
        if !last_message.trim().is_empty() {
            stdout = last_message;
        }
    }

    if stdout.len() > MAX_STDOUT_BYTES {
        truncate_utf8_to_max_bytes(&mut stdout, MAX_STDOUT_BYTES);
        stdout.push_str("\n[stdout truncated]\n");
    }

    if stderr.len() > MAX_STDERR_BYTES {
        truncate_utf8_to_max_bytes(&mut stderr, MAX_STDERR_BYTES);
        stderr.push_str("\n[stderr truncated]\n");
    }

    Ok(HarnessRunOutput {
        stdout,
        stderr,
        exit_code: output.status.code(),
        duration_secs: duration,
    })
}

fn scrub_environment(cmd: &mut tokio::process::Command) {
    cmd.env("SIGNET_NO_HOOKS", "1");
    cmd.env_remove("CLAUDECODE");

    let mut remove_exact: HashSet<&str> = HashSet::new();
    remove_exact.insert("HOME");
    remove_exact.insert("SSH_AUTH_SOCK");
    remove_exact.insert("GH_TOKEN");
    remove_exact.insert("GITHUB_TOKEN");
    remove_exact.insert("ANTHROPIC_API_KEY");
    remove_exact.insert("OPENAI_API_KEY");
    remove_exact.insert("GOOGLE_APPLICATION_CREDENTIALS");
    remove_exact.insert("OPENROUTER_API_KEY");

    for key in remove_exact {
        cmd.env_remove(key);
    }

    let mut dynamic_removals: Vec<String> = Vec::new();
    for (key, _) in std::env::vars() {
        if key.starts_with("AWS_") || key.starts_with("GCP_") || key.starts_with("AZURE_") {
            dynamic_removals.push(key);
        }
    }

    for key in dynamic_removals {
        cmd.env_remove(key);
    }
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

use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

pub fn ensure_harness_available(harness: &dyn Harness) -> Result<()> {
    resolve_executable(harness.executable()).with_context(|| {
        format!(
            "harness executable '{}' for {} was not found in PATH",
            harness.executable(),
            harness.name()
        )
    })?;
    Ok(())
}

pub fn resolve_executable(program: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    resolve_executable_in_paths(program, std::env::split_paths(&path))
}

fn resolve_executable_in_paths<I>(program: &str, paths: I) -> Result<PathBuf>
where
    I: IntoIterator<Item = PathBuf>,
{
    let candidate = Path::new(program);
    if candidate.components().count() > 1 {
        return executable_path(candidate)
            .with_context(|| format!("{} is not an executable file", candidate.display()));
    }

    for dir in paths {
        let candidate = dir.join(program);
        if let Ok(path) = executable_path(&candidate) {
            return Ok(path);
        }
    }

    Err(anyhow!("{program} not found in PATH"))
}

fn executable_path(path: &Path) -> Result<PathBuf> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(anyhow!("not a file"));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(anyhow!("not executable"));
        }
    }

    Ok(path.to_path_buf())
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn resolve_executable_finds_program_in_supplied_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("fake-harness");
        fs::write(&bin, "#!/bin/sh\nexit 0\n").expect("write fake harness");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&bin, fs::Permissions::from_mode(0o755))
                .expect("chmod fake harness");
        }

        let resolved = resolve_executable_in_paths("fake-harness", vec![dir.path().to_path_buf()])
            .expect("resolve fake harness");
        assert_eq!(resolved, bin);
    }

    #[test]
    fn resolve_executable_rejects_missing_program_in_supplied_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve_executable_in_paths("definitely-missing", vec![dir.path().to_path_buf()])
            .expect_err("missing binary should error");

        assert!(err.to_string().contains("definitely-missing not found"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_executable_rejects_non_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("not-executable");
        fs::write(&bin, "not runnable").expect("write non-executable");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).expect("chmod non-executable");

        let err = resolve_executable_in_paths("not-executable", vec![dir.path().to_path_buf()])
            .expect_err("non-executable should not resolve");

        assert!(err.to_string().contains("not-executable not found"));
    }
}

use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::config::{HarnessKind, ReasoningEffort};
use crate::harness::Harness;

pub struct CodexHarness;

const REAL_CODEX_BIN: &str = "/usr/bin/codex";
const LAST_MESSAGE_FILE: &str = "codex-last-message.txt";

fn codex_binary() -> &'static str {
    if Path::new(REAL_CODEX_BIN).exists() {
        REAL_CODEX_BIN
    } else {
        "codex"
    }
}

pub fn last_message_path(working_dir: &Path) -> PathBuf {
    working_dir.join(LAST_MESSAGE_FILE)
}

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn name(&self) -> &'static str {
        "codex"
    }

    fn executable(&self) -> &'static str {
        codex_binary()
    }

    fn build_command(
        &self,
        _prompt: &str,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        working_dir: &Path,
    ) -> Command {
        let mut cmd = Command::new(codex_binary());
        cmd.arg("exec").arg("--model").arg(model);
        if let Some(effort) = reasoning_effort {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort=\"{}\"", effort.as_str()));
        }
        cmd.arg("--skip-git-repo-check")
            .arg("--color")
            .arg("never")
            .arg("--output-last-message")
            .arg(last_message_path(working_dir))
            .arg("--json")
            .arg("-");
        cmd
    }

    fn uses_stdin(&self) -> bool {
        true
    }
}

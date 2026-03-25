use std::path::Path;

use tokio::process::Command;

use crate::config::{HarnessKind, ReasoningEffort};
use crate::harness::Harness;

pub struct CodexHarness;

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn name(&self) -> &'static str {
        "codex"
    }

    fn build_command(
        &self,
        _prompt: &str,
        model: &str,
        reasoning_effort: Option<ReasoningEffort>,
        _working_dir: &Path,
    ) -> Command {
        let mut cmd = Command::new("codex");
        cmd.arg("exec").arg("--model").arg(model);
        if let Some(effort) = reasoning_effort {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort=\"{}\"", effort.as_str()));
        }
        cmd.arg("--skip-git-repo-check").arg("--json").arg("-");
        cmd
    }

    fn uses_stdin(&self) -> bool {
        true
    }
}

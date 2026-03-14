use std::path::Path;

use tokio::process::Command;

use crate::config::HarnessKind;
use crate::harness::Harness;

pub struct CodexHarness;

impl Harness for CodexHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Codex
    }

    fn name(&self) -> &'static str {
        "codex"
    }

    fn build_command(&self, prompt: &str, model: &str, _working_dir: &Path) -> Command {
        let mut cmd = Command::new("codex");
        cmd.arg("exec")
            .arg("--model")
            .arg(model)
            .arg("--skip-git-repo-check")
            .arg("--json")
            .arg(prompt);
        cmd
    }
}

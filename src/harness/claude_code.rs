use std::path::Path;

use tokio::process::Command;

use crate::config::HarnessKind;
use crate::harness::Harness;

pub struct ClaudeCodeHarness;

impl Harness for ClaudeCodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    fn name(&self) -> &'static str {
        "claude-code"
    }

    fn build_command(&self, prompt: &str, model: &str, _working_dir: &Path) -> Command {
        let mut cmd = Command::new("claude");
        cmd.arg("--model")
            .arg(model)
            .arg("--dangerously-skip-permissions")
            .arg("-p")
            .arg(prompt);
        cmd
    }
}

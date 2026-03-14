use std::path::Path;

use tokio::process::Command;

use crate::config::HarnessKind;
use crate::harness::Harness;

pub struct OpencodeHarness;

impl Harness for OpencodeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Opencode
    }

    fn name(&self) -> &'static str {
        "opencode"
    }

    fn build_command(&self, prompt: &str, model: &str, _working_dir: &Path) -> Command {
        let mut cmd = Command::new("opencode");
        cmd.arg("run")
            .arg("--model")
            .arg(model)
            .arg("--format")
            .arg("json")
            .arg(prompt);
        cmd
    }
}

use std::path::Path;

use tokio::process::Command;

use crate::config::HarnessKind;

pub mod claude_code;
pub mod codex;
pub mod opencode;
pub mod spawn;

pub trait Harness: Send + Sync {
    fn kind(&self) -> HarnessKind;
    fn name(&self) -> &'static str;
    fn build_command(&self, prompt: &str, model: &str, working_dir: &Path) -> Command;
    /// Whether this harness accepts the prompt via stdin instead of CLI arg.
    fn uses_stdin(&self) -> bool {
        false
    }
}

pub fn for_kind(kind: HarnessKind) -> Box<dyn Harness> {
    match kind {
        HarnessKind::ClaudeCode => Box::new(claude_code::ClaudeCodeHarness),
        HarnessKind::Opencode => Box::new(opencode::OpencodeHarness),
        HarnessKind::Codex => Box::new(codex::CodexHarness),
    }
}

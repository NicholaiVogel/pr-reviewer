use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::safety::canonicalize_within_root;

pub fn read_file_safe(
    repo_root: &Path,
    rel_path: &Path,
    max_bytes: usize,
) -> Result<Option<String>> {
    let canonical = canonicalize_within_root(repo_root, rel_path)?;
    let bytes =
        fs::read(&canonical).with_context(|| format!("failed reading {}", canonical.display()))?;

    let sample = bytes.iter().take(8 * 1024);
    if sample.into_iter().any(|b| *b == 0) {
        return Ok(None);
    }

    let capped = if bytes.len() > max_bytes {
        &bytes[..max_bytes]
    } else {
        &bytes
    };

    let text = String::from_utf8_lossy(capped).to_string();
    Ok(Some(text))
}

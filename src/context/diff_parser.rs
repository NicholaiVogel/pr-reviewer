use std::collections::HashMap;

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum DiffSide {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DiffLineType {
    Added,
    Removed,
    Context,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub content: String,
    pub line_type: DiffLineType,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub diff_position: u32,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub old_path: String,
    pub new_path: String,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, Copy)]
pub struct PositionInfo {
    pub diff_position: u32,
    pub side: DiffSide,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedDiff {
    pub files: Vec<FileDiff>,
    pub positions: HashMap<(String, u32, DiffSide), PositionInfo>,
    pub total_hunk_lines: usize,
}

impl ParsedDiff {
    pub fn position_for(&self, path: &str, line: u32, side: DiffSide) -> Option<PositionInfo> {
        self.positions.get(&(path.to_string(), line, side)).copied()
    }
}

pub fn parse_unified_diff(input: &str) -> Result<ParsedDiff> {
    let mut parsed = ParsedDiff::default();

    let mut current_file: Option<FileDiff> = None;
    let mut current_hunk: Option<Hunk> = None;

    let mut old_cursor = 0u32;
    let mut new_cursor = 0u32;
    let mut file_diff_position = 0u32;

    for line in input.lines() {
        if line.starts_with("diff --git ") {
            if let Some(hunk) = current_hunk.take() {
                if let Some(file) = current_file.as_mut() {
                    file.hunks.push(hunk);
                }
            }
            if let Some(file) = current_file.take() {
                parsed.files.push(file);
            }

            let (old_path, new_path) = parse_diff_git_header(line)?;
            current_file = Some(FileDiff {
                old_path,
                new_path,
                hunks: Vec::new(),
            });
            file_diff_position = 0;
            continue;
        }

        if line.starts_with("--- ") {
            if let Some(file) = current_file.as_mut() {
                file.old_path = normalize_diff_path(line.trim_start_matches("--- "));
            }
            continue;
        }

        if line.starts_with("+++ ") {
            if let Some(file) = current_file.as_mut() {
                file.new_path = normalize_diff_path(line.trim_start_matches("+++ "));
            }
            continue;
        }

        if line.starts_with("@@") {
            if let Some(hunk) = current_hunk.take() {
                if let Some(file) = current_file.as_mut() {
                    file.hunks.push(hunk);
                }
            }
            let (old_start, old_count, new_start, new_count) = parse_hunk_header(line)?;
            old_cursor = old_start;
            new_cursor = new_start;
            current_hunk = Some(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                lines: Vec::new(),
            });
            continue;
        }

        if current_hunk.is_none() {
            continue;
        }

        if line.starts_with("\\ No newline") {
            continue;
        }

        let Some(file) = current_file.as_ref() else {
            continue;
        };

        let Some(hunk) = current_hunk.as_mut() else {
            continue;
        };

        let (line_type, old_line, new_line, side_for_primary) = match line.chars().next() {
            Some('+') => {
                let current = new_cursor;
                new_cursor = new_cursor.saturating_add(1);
                (DiffLineType::Added, None, Some(current), DiffSide::Right)
            }
            Some('-') => {
                let current = old_cursor;
                old_cursor = old_cursor.saturating_add(1);
                (DiffLineType::Removed, Some(current), None, DiffSide::Left)
            }
            Some(' ') => {
                let old_current = old_cursor;
                let new_current = new_cursor;
                old_cursor = old_cursor.saturating_add(1);
                new_cursor = new_cursor.saturating_add(1);
                (
                    DiffLineType::Context,
                    Some(old_current),
                    Some(new_current),
                    DiffSide::Right,
                )
            }
            _ => continue,
        };

        file_diff_position = file_diff_position.saturating_add(1);
        parsed.total_hunk_lines += 1;

        let diff_line = DiffLine {
            content: line.get(1..).unwrap_or_default().to_string(),
            line_type,
            old_line,
            new_line,
            diff_position: file_diff_position,
        };
        hunk.lines.push(diff_line);

        let map_path = if file.new_path == "/dev/null" {
            &file.old_path
        } else {
            &file.new_path
        };

        match line_type {
            DiffLineType::Added => {
                if let Some(nl) = new_line {
                    parsed.positions.insert(
                        (map_path.to_string(), nl, side_for_primary),
                        PositionInfo {
                            diff_position: file_diff_position,
                            side: DiffSide::Right,
                        },
                    );
                }
            }
            DiffLineType::Removed => {
                if let Some(ol) = old_line {
                    parsed.positions.insert(
                        (map_path.to_string(), ol, side_for_primary),
                        PositionInfo {
                            diff_position: file_diff_position,
                            side: DiffSide::Left,
                        },
                    );
                }
            }
            DiffLineType::Context => {
                if let Some(nl) = new_line {
                    parsed.positions.insert(
                        (map_path.to_string(), nl, DiffSide::Right),
                        PositionInfo {
                            diff_position: file_diff_position,
                            side: DiffSide::Right,
                        },
                    );
                }
                if let Some(ol) = old_line {
                    parsed.positions.insert(
                        (map_path.to_string(), ol, DiffSide::Left),
                        PositionInfo {
                            diff_position: file_diff_position,
                            side: DiffSide::Left,
                        },
                    );
                }
            }
        }
    }

    if let Some(hunk) = current_hunk {
        if let Some(file) = current_file.as_mut() {
            file.hunks.push(hunk);
        }
    }

    if let Some(file) = current_file {
        parsed.files.push(file);
    }

    Ok(parsed)
}

fn parse_diff_git_header(line: &str) -> Result<(String, String)> {
    let mut parts = line.split_whitespace();
    let _ = parts.next();
    let _ = parts.next();
    let a = parts
        .next()
        .ok_or_else(|| anyhow!("malformed diff header: missing old path"))?;
    let b = parts
        .next()
        .ok_or_else(|| anyhow!("malformed diff header: missing new path"))?;
    Ok((normalize_diff_path(a), normalize_diff_path(b)))
}

fn parse_hunk_header(line: &str) -> Result<(u32, u32, u32, u32)> {
    let end = line
        .find("@@")
        .and_then(|idx| line[idx + 2..].find("@@").map(|off| idx + 2 + off))
        .unwrap_or(line.len());
    let header = &line[..end + 2.min(line.len().saturating_sub(end))];

    let pieces: Vec<&str> = header.split_whitespace().collect();
    if pieces.len() < 3 {
        return Err(anyhow!("malformed hunk header: {line}"));
    }

    let (old_start, old_count) = parse_hunk_range(pieces[1].trim_start_matches('-'))?;
    let (new_start, new_count) = parse_hunk_range(pieces[2].trim_start_matches('+'))?;

    Ok((old_start, old_count, new_start, new_count))
}

fn parse_hunk_range(range: &str) -> Result<(u32, u32)> {
    let mut parts = range.split(',');
    let start = parts
        .next()
        .ok_or_else(|| anyhow!("missing hunk range start"))?
        .parse::<u32>()?;
    let count = match parts.next() {
        Some(v) => v.parse::<u32>()?,
        None => 1,
    };
    Ok((start, count))
}

fn normalize_diff_path(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("a/")
        .trim_start_matches("b/")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_positions_for_added_removed_and_context_lines() {
        let diff = r#"
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 line1
-line2
+line2 changed
 line3
+line4
"#;
        let parsed = parse_unified_diff(diff).expect("parse diff");
        assert_eq!(parsed.files.len(), 1);

        let p_added = parsed
            .position_for("src/lib.rs", 2, DiffSide::Right)
            .expect("added line mapped");
        assert!(p_added.diff_position >= 1);

        let p_removed = parsed
            .position_for("src/lib.rs", 2, DiffSide::Left)
            .expect("removed line mapped");
        assert!(p_removed.diff_position >= 1);

        let p_context = parsed
            .position_for("src/lib.rs", 1, DiffSide::Right)
            .expect("context line mapped");
        assert_eq!(p_context.side, DiffSide::Right);
    }

    #[test]
    fn supports_single_line_hunk_counts() {
        let diff = r#"
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -10 +10 @@
-old
+new
"#;
        let parsed = parse_unified_diff(diff).expect("parse diff");
        assert_eq!(parsed.files[0].hunks[0].old_count, 1);
        assert_eq!(parsed.files[0].hunks[0].new_count, 1);
    }
}

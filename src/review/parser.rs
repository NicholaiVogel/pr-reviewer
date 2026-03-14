use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Approve,
    Comment,
    RequestChanges,
}

impl ReviewVerdict {
    pub fn as_github_event(self) -> &'static str {
        match self {
            ReviewVerdict::Approve => "APPROVE",
            ReviewVerdict::Comment => "COMMENT",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    pub line: u32,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredReview {
    pub summary: String,
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone)]
pub enum ParseOutcome {
    Structured(StructuredReview),
    RawSummary(String),
    Empty,
}

pub fn parse_review_output(stdout: &str, stderr: &str) -> Result<ParseOutcome> {
    let combined = normalize_output(stdout, stderr);
    if combined.trim().is_empty() {
        return Ok(ParseOutcome::Empty);
    }

    if let Some(marked) = extract_marked_json(&combined) {
        if let Ok(parsed) = parse_and_validate(&marked) {
            return Ok(ParseOutcome::Structured(parsed));
        }
    }

    let mut candidates = extract_json_objects(&combined);
    candidates.reverse();

    for candidate in candidates {
        if let Ok(parsed) = parse_and_validate(&candidate) {
            return Ok(ParseOutcome::Structured(parsed));
        }
    }

    Ok(ParseOutcome::RawSummary(combined.trim().to_string()))
}

fn normalize_output(stdout: &str, stderr: &str) -> String {
    let out = stdout.trim();
    let err = stderr.trim();

    if !out.is_empty() && !err.is_empty() {
        format!("{out}\n\n[stderr]\n{err}")
    } else if !out.is_empty() {
        out.to_string()
    } else {
        err.to_string()
    }
}

fn parse_and_validate(json: &str) -> Result<StructuredReview> {
    let parsed: StructuredReview =
        serde_json::from_str(json).map_err(|e| anyhow!("review JSON parse failed: {e}"))?;

    if parsed.summary.trim().is_empty() {
        return Err(anyhow!("summary cannot be empty"));
    }

    for comment in &parsed.comments {
        if comment.file.trim().is_empty() {
            return Err(anyhow!("comment file cannot be empty"));
        }
        if comment.line == 0 {
            return Err(anyhow!("comment line must be > 0"));
        }
        if comment.body.trim().is_empty() {
            return Err(anyhow!("comment body cannot be empty"));
        }
    }

    Ok(parsed)
}

fn extract_marked_json(text: &str) -> Option<String> {
    let marker = "```pr-review-json";
    let start = text.find(marker)?;
    let after = &text[start + marker.len()..];
    let after = after.strip_prefix('\n').unwrap_or(after);
    let end = after.find("```")?;
    Some(after[..end].trim().to_string())
}

fn extract_json_objects(text: &str) -> Vec<String> {
    let mut results = Vec::new();
    let mut start_idx: Option<usize> = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start_idx = Some(idx);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    continue;
                }
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start_idx.take() {
                        results.push(text[start..=idx].to_string());
                    }
                }
            }
            _ => {}
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_marked_json_block() {
        let input = r#"
analysis
```pr-review-json
{"summary":"ok","verdict":"comment","comments":[]}
```
"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.summary, "ok");
                assert_eq!(review.verdict, ReviewVerdict::Comment);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn picks_last_json_object() {
        let input = r#"{"summary":"old","verdict":"comment","comments":[]} {"summary":"new","verdict":"approve","comments":[]}"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.summary, "new");
                assert_eq!(review.verdict, ReviewVerdict::Approve);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn falls_back_to_raw_summary_on_invalid_json() {
        let input = "I could not produce valid JSON";
        let parsed = parse_review_output(input, "").expect("parse output");
        match parsed {
            ParseOutcome::RawSummary(s) => assert!(s.contains("could not")),
            _ => panic!("expected raw fallback"),
        }
    }

    #[test]
    fn empty_output_returns_empty_outcome() {
        let parsed = parse_review_output("", "").expect("parse output");
        assert!(matches!(parsed, ParseOutcome::Empty));
    }
}

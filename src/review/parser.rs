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

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ConfidenceRatings {
    pub style_maintainability: u8,
    pub repo_convention_adherence: u8,
    pub merge_conflict_detection: u8,
    pub security_vulnerability_detection: u8,
    pub injection_risk_detection: u8,
    pub attack_surface_risk_assessment: u8,
    pub future_hardening_guidance: u8,
    pub scope_alignment: u8,
    pub duplication_awareness: u8,
    pub tooling_pattern_leverage: u8,
    pub functional_completeness: u8,
    pub pattern_correctness: u8,
    pub documentation_coverage: u8,
}

impl ConfidenceRatings {
    pub fn validate(&self) -> Result<()> {
        validate_confidence("style_maintainability", self.style_maintainability)?;
        validate_confidence("repo_convention_adherence", self.repo_convention_adherence)?;
        validate_confidence("merge_conflict_detection", self.merge_conflict_detection)?;
        validate_confidence(
            "security_vulnerability_detection",
            self.security_vulnerability_detection,
        )?;
        validate_confidence("injection_risk_detection", self.injection_risk_detection)?;
        validate_confidence(
            "attack_surface_risk_assessment",
            self.attack_surface_risk_assessment,
        )?;
        validate_confidence("future_hardening_guidance", self.future_hardening_guidance)?;
        validate_confidence("scope_alignment", self.scope_alignment)?;
        validate_confidence("duplication_awareness", self.duplication_awareness)?;
        validate_confidence("tooling_pattern_leverage", self.tooling_pattern_leverage)?;
        validate_confidence("functional_completeness", self.functional_completeness)?;
        validate_confidence("pattern_correctness", self.pattern_correctness)?;
        validate_confidence("documentation_coverage", self.documentation_coverage)?;
        Ok(())
    }

    pub fn average(&self) -> f32 {
        let sum = self.style_maintainability as u32
            + self.repo_convention_adherence as u32
            + self.merge_conflict_detection as u32
            + self.security_vulnerability_detection as u32
            + self.injection_risk_detection as u32
            + self.attack_surface_risk_assessment as u32
            + self.future_hardening_guidance as u32
            + self.scope_alignment as u32
            + self.duplication_awareness as u32
            + self.tooling_pattern_leverage as u32
            + self.functional_completeness as u32
            + self.pattern_correctness as u32
            + self.documentation_coverage as u32;
        sum as f32 / 13.0
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
    pub confidence: ConfidenceRatings,
    #[serde(default)]
    pub comments: Vec<ReviewComment>,
}

#[derive(Debug, Clone)]
pub enum ParseOutcome {
    Structured(StructuredReview),
    RawSummary(String),
    Empty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredReplyUpdate {
    pub reply: String,
    pub confidence: ConfidenceRatings,
}

#[derive(Debug, Clone)]
pub enum ReplyParseOutcome {
    Structured(StructuredReplyUpdate),
    Raw(String),
    Empty,
}

pub fn parse_review_output(stdout: &str, stderr: &str) -> Result<ParseOutcome> {
    let combined = normalize_output(stdout, stderr);
    if combined.trim().is_empty() {
        return Ok(ParseOutcome::Empty);
    }

    if let Some(marked) = extract_marked_json(&combined, "pr-review-json") {
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

pub fn parse_reply_output(stdout: &str, stderr: &str) -> Result<ReplyParseOutcome> {
    let combined = normalize_output(stdout, stderr);
    if combined.trim().is_empty() {
        return Ok(ReplyParseOutcome::Empty);
    }

    if let Some(marked) = extract_marked_json(&combined, "pr-review-reply-json") {
        if let Ok(parsed) = parse_reply_and_validate(&marked) {
            return Ok(ReplyParseOutcome::Structured(parsed));
        }
    }

    let mut candidates = extract_json_objects(&combined);
    candidates.reverse();

    for candidate in candidates {
        if let Ok(parsed) = parse_reply_and_validate(&candidate) {
            return Ok(ReplyParseOutcome::Structured(parsed));
        }
    }

    Ok(ReplyParseOutcome::Raw(combined.trim().to_string()))
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
    parsed.confidence.validate()?;

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

fn parse_reply_and_validate(json: &str) -> Result<StructuredReplyUpdate> {
    let parsed: StructuredReplyUpdate =
        serde_json::from_str(json).map_err(|e| anyhow!("reply JSON parse failed: {e}"))?;

    if parsed.reply.trim().is_empty() {
        return Err(anyhow!("reply cannot be empty"));
    }
    parsed.confidence.validate()?;
    Ok(parsed)
}

fn validate_confidence(name: &str, value: u8) -> Result<()> {
    if (1..=10).contains(&value) {
        Ok(())
    } else {
        Err(anyhow!("{name} confidence must be within 1..=10"))
    }
}

fn extract_marked_json(text: &str, marker_name: &str) -> Option<String> {
    let marker = format!("```{marker_name}");
    let start = text.find(&marker)?;
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

    fn confidence_json() -> &'static str {
        r#""confidence":{"style_maintainability":8,"repo_convention_adherence":8,"merge_conflict_detection":7,"security_vulnerability_detection":8,"injection_risk_detection":8,"attack_surface_risk_assessment":7,"future_hardening_guidance":7,"scope_alignment":9,"duplication_awareness":8,"tooling_pattern_leverage":8,"functional_completeness":7,"pattern_correctness":8,"documentation_coverage":6}"#
    }

    #[test]
    fn parses_marked_json_block() {
        let input = format!(
            "analysis\n```pr-review-json\n{{\"summary\":\"ok\",\"verdict\":\"comment\",{},\"comments\":[]}}\n```\n",
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.summary, "ok");
                assert_eq!(review.verdict, ReviewVerdict::Comment);
                assert_eq!(review.confidence.scope_alignment, 9);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn picks_last_json_object() {
        let input = format!(
            "{{\"summary\":\"old\",\"verdict\":\"comment\",{},\"comments\":[]}} {{\"summary\":\"new\",\"verdict\":\"approve\",{},\"comments\":[]}}",
            confidence_json(),
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.summary, "new");
                assert_eq!(review.verdict, ReviewVerdict::Approve);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn confidence_out_of_range_falls_back_to_raw() {
        let input = r#"```pr-review-json
{"summary":"bad","verdict":"comment","confidence":{"style_maintainability":0,"repo_convention_adherence":8,"merge_conflict_detection":7,"security_vulnerability_detection":8,"injection_risk_detection":8,"attack_surface_risk_assessment":7,"future_hardening_guidance":7,"scope_alignment":9,"duplication_awareness":8,"tooling_pattern_leverage":8,"functional_completeness":7,"pattern_correctness":8,"documentation_coverage":6},"comments":[]}
```"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        assert!(matches!(parsed, ParseOutcome::RawSummary(_)));
    }

    #[test]
    fn parses_reply_json_block() {
        let input = format!(
            "```pr-review-reply-json\n{{\"reply\":\"Thanks, verified fix.\",{}}}\n```",
            confidence_json()
        );
        let parsed = parse_reply_output(&input, "").expect("parse output");
        match parsed {
            ReplyParseOutcome::Structured(reply) => {
                assert!(reply.reply.contains("verified"));
                assert_eq!(reply.confidence.pattern_correctness, 8);
            }
            _ => panic!("expected structured reply"),
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

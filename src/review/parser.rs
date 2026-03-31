use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    NoIssues,
    Comment,
    RequestChanges,
    /// Legacy variant: if the model outputs "approve", normalize to NoIssues.
    #[serde(alias = "approve")]
    #[serde(skip_serializing)]
    Approve,
}

impl ReviewVerdict {
    pub fn as_github_event(self) -> &'static str {
        match self {
            ReviewVerdict::NoIssues | ReviewVerdict::Approve => "COMMENT",
            ReviewVerdict::Comment => "COMMENT",
            ReviewVerdict::RequestChanges => "REQUEST_CHANGES",
        }
    }

    /// Normalize legacy Approve into NoIssues.
    pub fn normalized(self) -> Self {
        match self {
            ReviewVerdict::Approve => ReviewVerdict::NoIssues,
            other => other,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    High,
    Medium,
    Low,
}

impl std::fmt::Display for ConfidenceLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfidenceLevel::High => write!(f, "High"),
            ConfidenceLevel::Medium => write!(f, "Medium"),
            ConfidenceLevel::Low => write!(f, "Low"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confidence {
    pub level: ConfidenceLevel,
    pub reasons: Vec<ConfidenceReason>,
    pub justification: String,
}

impl Confidence {
    pub fn validate(&self) -> Result<()> {
        if self.reasons.is_empty() {
            return Err(anyhow!("confidence reasons cannot be empty"));
        }
        if self.justification.trim().is_empty() {
            return Err(anyhow!("confidence justification cannot be empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceReason {
    SufficientDiffEvidence,
    TargetedContextIncluded,
    MissingRuntimeRepro,
    MissingCrossModuleContext,
    AmbiguousRequirements,
}

impl ConfidenceReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ConfidenceReason::SufficientDiffEvidence => "sufficient_diff_evidence",
            ConfidenceReason::TargetedContextIncluded => "targeted_context_included",
            ConfidenceReason::MissingRuntimeRepro => "missing_runtime_repro",
            ConfidenceReason::MissingCrossModuleContext => "missing_cross_module_context",
            ConfidenceReason::AmbiguousRequirements => "ambiguous_requirements",
        }
    }
}

impl std::fmt::Display for ConfidenceReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CommentSeverity {
    Blocking,
    Warning,
    Nitpick,
}

impl Default for CommentSeverity {
    fn default() -> Self {
        CommentSeverity::Warning
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub file: String,
    pub line: u32,
    pub body: String,
    #[serde(default)]
    pub evidence_note: Option<String>,
    #[serde(default)]
    pub severity: CommentSeverity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructuredReview {
    pub summary: String,
    pub verdict: ReviewVerdict,
    pub confidence: Confidence,
    #[serde(default)]
    pub comments: Vec<ReviewComment>,
    #[serde(default)]
    pub ui_screenshot_needed: bool,
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
    let mut parsed: StructuredReview =
        serde_json::from_str(json).map_err(|e| anyhow!("review JSON parse failed: {e}"))?;

    // Normalize legacy "approve" verdict to NoIssues
    parsed.verdict = parsed.verdict.normalized();

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
    Ok(parsed)
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
        r#""confidence":{"level":"medium","reasons":["sufficient_diff_evidence"],"justification":"Changes are straightforward but touch error paths."}"#
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
                assert_eq!(review.confidence.level, ConfidenceLevel::Medium);
                assert!(review.confidence.justification.contains("error paths"));
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn picks_last_json_object_and_normalizes_approve() {
        let input = format!(
            "{{\"summary\":\"old\",\"verdict\":\"comment\",{},\"comments\":[]}} {{\"summary\":\"new\",\"verdict\":\"approve\",{},\"comments\":[]}}",
            confidence_json(),
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.summary, "new");
                // "approve" should be normalized to NoIssues
                assert_eq!(review.verdict, ReviewVerdict::NoIssues);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn no_issues_verdict_parses() {
        let input = format!(
            "```pr-review-json\n{{\"summary\":\"All good.\",\"verdict\":\"no_issues\",{},\"comments\":[]}}\n```",
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.verdict, ReviewVerdict::NoIssues);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn empty_confidence_justification_falls_back_to_raw() {
        let input = r#"```pr-review-json
{"summary":"bad","verdict":"comment","confidence":{"level":"high","reasons":["sufficient_diff_evidence"],"justification":""},"comments":[]}
```"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        assert!(matches!(parsed, ParseOutcome::RawSummary(_)));
    }

    #[test]
    fn missing_confidence_reasons_falls_back_to_raw() {
        let input = r#"```pr-review-json
{"summary":"bad","verdict":"comment","confidence":{"level":"high","justification":"clear bug in diff"},"comments":[]}
```"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        assert!(matches!(parsed, ParseOutcome::RawSummary(_)));
    }

    #[test]
    fn boilerplate_context_disclaimer_does_not_drop_structured_output() {
        let input = r#"```pr-review-json
{"summary":"bad","verdict":"comment","confidence":{"level":"low","reasons":["missing_runtime_repro"],"justification":"Confidence is low because full repository contents and runtime behavior are not available in this review context."},"comments":[]}
```"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        assert!(matches!(parsed, ParseOutcome::Structured(_)));
    }

    #[test]
    fn boilerplate_context_disclaimer_without_context_reason_is_still_structured() {
        let input = r#"```pr-review-json
{"summary":"ok","verdict":"comment","confidence":{"level":"low","reasons":["sufficient_diff_evidence"],"justification":"Runtime behavior is hard to verify in this review context."},"comments":[]}
```"#;
        let parsed = parse_review_output(input, "").expect("parse output");
        assert!(matches!(parsed, ParseOutcome::Structured(_)));
    }

    #[test]
    fn parses_comment_severity() {
        let input = format!(
            "```pr-review-json\n{{\"summary\":\"found stuff\",\"verdict\":\"request_changes\",{},\"comments\":[{{\"file\":\"src/main.rs\",\"line\":10,\"body\":\"bug\",\"severity\":\"blocking\"}}]}}\n```",
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.comments[0].severity, CommentSeverity::Blocking);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn missing_severity_defaults_to_warning() {
        let input = format!(
            "```pr-review-json\n{{\"summary\":\"found stuff\",\"verdict\":\"comment\",{},\"comments\":[{{\"file\":\"src/main.rs\",\"line\":10,\"body\":\"note\"}}]}}\n```",
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert_eq!(review.comments[0].severity, CommentSeverity::Warning);
            }
            _ => panic!("expected structured review"),
        }
    }

    #[test]
    fn parses_reply_json_block() {
        let input = "```pr-review-reply-json\n{\"reply\":\"Thanks, verified fix.\"}\n```";
        let parsed = parse_reply_output(input, "").expect("parse output");
        match parsed {
            ReplyParseOutcome::Structured(reply) => {
                assert!(reply.reply.contains("verified"));
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

    #[test]
    fn ui_screenshot_needed_defaults_false() {
        let input = format!(
            "```pr-review-json\n{{\"summary\":\"ok\",\"verdict\":\"no_issues\",{},\"comments\":[]}}\n```",
            confidence_json()
        );
        let parsed = parse_review_output(&input, "").expect("parse output");
        match parsed {
            ParseOutcome::Structured(review) => {
                assert!(!review.ui_screenshot_needed);
            }
            _ => panic!("expected structured review"),
        }
    }
}

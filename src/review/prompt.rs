use crate::config::RepoConfig;
use crate::github::types::PullRequest;

pub fn build_review_prompt(
    repo_cfg: &RepoConfig,
    pr: &PullRequest,
    context: &str,
    bot_name: &str,
) -> String {
    let mut prompt = String::new();

    prompt.push_str("You are an expert code reviewer. Focus only on high-signal issues: bugs, security flaws, data corruption, race conditions, logic mistakes, and breaking changes. Ignore style-only suggestions unless they hide a correctness issue.\n\n");
    prompt.push_str(
        "Output MUST be a JSON object in a fenced block tagged exactly as `pr-review-json`.\n",
    );
    prompt.push_str("Schema:\n");
    prompt.push_str("- summary: string\n");
    prompt.push_str("- verdict: one of [\"approve\", \"comment\", \"request_changes\"]\n");
    prompt.push_str("- confidence: object with integer ratings from 1 to 10 for:\n");
    prompt.push_str("  - style_maintainability\n");
    prompt.push_str("  - repo_convention_adherence\n");
    prompt.push_str("  - merge_conflict_detection\n");
    prompt.push_str("  - scope_alignment\n");
    prompt.push_str("  - duplication_awareness\n");
    prompt.push_str("  - tooling_pattern_leverage\n");
    prompt.push_str("  - functional_completeness\n");
    prompt.push_str("  - pattern_correctness\n");
    prompt.push_str("  - documentation_coverage\n");
    prompt.push_str(
        "- comments: array of objects with fields { file: string, line: integer, body: string }\n",
    );
    prompt.push_str(
        "Line numbers must refer to changed lines in the current diff whenever possible.\n\n",
    );
    prompt.push_str(
        "If prior review history is provided, explicitly mention what was fixed and what remains unresolved.\n\n",
    );

    prompt.push_str("Example output:\n");
    prompt.push_str("```pr-review-json\n");
    prompt.push_str("{\n");
    prompt.push_str("  \"summary\": \"Found one security issue and one null handling bug.\",\n");
    prompt.push_str("  \"verdict\": \"request_changes\",\n");
    prompt.push_str("  \"confidence\": {\n");
    prompt.push_str("    \"style_maintainability\": 8,\n");
    prompt.push_str("    \"repo_convention_adherence\": 9,\n");
    prompt.push_str("    \"merge_conflict_detection\": 7,\n");
    prompt.push_str("    \"scope_alignment\": 8,\n");
    prompt.push_str("    \"duplication_awareness\": 8,\n");
    prompt.push_str("    \"tooling_pattern_leverage\": 7,\n");
    prompt.push_str("    \"functional_completeness\": 8,\n");
    prompt.push_str("    \"pattern_correctness\": 7,\n");
    prompt.push_str("    \"documentation_coverage\": 6\n");
    prompt.push_str("  },\n");
    prompt.push_str("  \"comments\": [\n");
    prompt.push_str("    {\"file\": \"src/auth.rs\", \"line\": 41, \"body\": \"Potential SQL injection when interpolating user input into query string.\"}\n");
    prompt.push_str("  ]\n");
    prompt.push_str("}\n");
    prompt.push_str("```\n\n");

    prompt.push_str(&format!(
        "Repository: {}/{}\n",
        repo_cfg.owner, repo_cfg.name
    ));
    prompt.push_str(&format!("Pull Request: #{}\n", pr.number));
    prompt.push_str(&format!("Bot identity: {}\n", bot_name));

    if let Some(custom) = repo_cfg.custom_instructions.as_deref() {
        if !custom.trim().is_empty() {
            prompt.push_str("\nCustom repository instructions:\n");
            prompt.push_str(custom);
            prompt.push('\n');
        }
    }

    prompt.push_str("\nContext:\n");
    prompt.push_str(context);

    prompt
}

use crate::config::RepoConfig;
use crate::github::types::PullRequest;

pub fn build_review_prompt(
    repo_cfg: &RepoConfig,
    pr: &PullRequest,
    context: &str,
    bot_name: &str,
    repo_conventions: Option<&str>,
    has_prior_reviews: bool,
    ui_files_changed: Option<&[String]>,
) -> String {
    let mut prompt = String::new();

    prompt.push_str("Review this pull request. Be discerning. Use good judgment.\n\n");
    prompt.push_str(
        "Focus on: bugs, security flaws, data corruption, race conditions, logic mistakes, breaking changes, and patterns that diverge from codebase conventions. Do not flag style-only issues unless they conceal a correctness problem.\n\n",
    );
    prompt.push_str(&format!(
        "You are an automated review tool (identity: {}). State this in the first line of your summary.\n\n",
        bot_name,
    ));
    prompt.push_str("Instructions:\n");
    prompt.push_str(
        "1. Read the PR description. Validate whether the implementation achieves what the description claims. If the PR says it does X, verify the code does X. If the implementation diverges from stated goals, flag this.\n",
    );
    prompt.push_str(
        "2. Check for introduced security vulnerabilities, injection risks, and attack-surface expansion.\n",
    );

    let mut instruction_num = 3;

    if repo_conventions.is_some() {
        prompt.push_str(&format!(
            "{}. Check adherence to repository conventions provided below. Flag deviations only when they affect correctness, consistency, or maintainability.\n",
            instruction_num,
        ));
        instruction_num += 1;
    }

    if has_prior_reviews {
        prompt.push_str(&format!(
            "{}. Prior review history and inline discussion are in the context. Items marked [dismissed by human], [rejected with rationale], [out of scope for this pr], or [likely addressed] MUST NOT be re-flagged unless the new diff adds materially new evidence. If you re-raise a prior concern, include a short `evidence_note` naming the new lines or changed path that justify reopening it. If a human said a concern is intentional, acceptable, or out of scope for this PR, respect that and do not press the same angle again.\n",
            instruction_num,
        ));
        instruction_num += 1;
    }

    if let Some(ui_files) = ui_files_changed {
        let file_list: String = ui_files
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ");
        prompt.push_str(&format!(
            "{}. This PR modifies UI files: {}. If the PR description does not reference screenshots or visual previews, set ui_screenshot_needed to true. This is a non-blocking metadata note only, never a blocker, and it should only be surfaced once per PR.\n",
            instruction_num, file_list,
        ));
        let _ = instruction_num; // suppress unused warning
    }

    prompt.push_str(
        "Do not turn adjacent architecture preferences into blockers. A blocker must be grounded in concrete evidence from the changed code or targeted context and must directly affect the PR's stated behavior, security, or data integrity.\n\n",
    );

    prompt.push_str(
        "\nYou MUST NOT approve this PR or state it is safe to merge. You are not authorized to make that call. Your role is to flag issues or signal readiness for human review.\n\n",
    );

    prompt.push_str("Output a JSON object in a fenced block tagged exactly `pr-review-json`.\n");
    prompt.push_str("Schema:\n");
    prompt
        .push_str("- summary: string (first line must identify this as an automated bot review)\n");
    prompt.push_str("- verdict: one of [\"no_issues\", \"comment\", \"request_changes\"]\n");
    prompt.push_str("  - \"no_issues\": nothing worth flagging, ready for human review\n");
    prompt.push_str("  - \"comment\": found issues worth discussing but not blocking\n");
    prompt.push_str("  - \"request_changes\": found issues that should be fixed before merge\n");
    prompt.push_str("- confidence: object with:\n");
    prompt.push_str("  - level: one of [\"high\", \"medium\", \"low\"]\n");
    prompt.push_str("  - reasons: array of one or more reason codes from [\"sufficient_diff_evidence\", \"targeted_context_included\", \"missing_runtime_repro\", \"missing_cross_module_context\", \"ambiguous_requirements\"]\n");
    prompt.push_str("  - justification: string explaining your confidence with concrete evidence. Do NOT use boilerplate like \"full repository context is unavailable\" unless you name a specific missing artifact (example: failing test case name, runtime trace, or exact file/module that is missing).\n");
    prompt.push_str("- ui_screenshot_needed: boolean (true if UI files changed and no screenshots referenced in PR description)\n");
    prompt.push_str("- comments: array of objects with:\n");
    prompt.push_str("  - file: string (file path)\n");
    prompt.push_str("  - line: integer (line number in the changed file)\n");
    prompt.push_str("  - body: string (the finding)\n");
    prompt.push_str("  - evidence_note: optional string (required when re-raising a previously rebutted concern, briefly naming the new lines or changed path that justify reopening it)\n");
    prompt.push_str("  - severity: one of [\"blocking\", \"warning\", \"nitpick\"]\n");
    prompt.push_str(
        "\nLine numbers must refer to changed lines in the current diff whenever possible.\n\n",
    );

    prompt.push_str("Example output:\n");
    prompt.push_str("```pr-review-json\n");
    prompt.push_str("{\n");
    prompt.push_str("  \"summary\": \"[Automated Review] Found one security issue and one null handling bug.\",\n");
    prompt.push_str("  \"verdict\": \"request_changes\",\n");
    prompt.push_str("  \"confidence\": {\n");
    prompt.push_str("    \"level\": \"high\",\n");
    prompt.push_str(
        "    \"reasons\": [\"sufficient_diff_evidence\", \"targeted_context_included\"],\n",
    );
    prompt.push_str("    \"justification\": \"The SQL interpolation is explicit in src/auth.rs and is reachable from the login handler. The issue is directly provable from this diff.\"\n");
    prompt.push_str("  },\n");
    prompt.push_str("  \"ui_screenshot_needed\": false,\n");
    prompt.push_str("  \"comments\": [\n");
    prompt.push_str("    {\"file\": \"src/auth.rs\", \"line\": 41, \"body\": \"Potential SQL injection when interpolating user input into query string.\", \"severity\": \"blocking\"}\n");
    prompt.push_str("  ]\n");
    prompt.push_str("}\n");
    prompt.push_str("```\n\n");

    prompt.push_str(&format!(
        "Repository: {}/{}\n",
        repo_cfg.owner, repo_cfg.name,
    ));
    prompt.push_str(&format!("Pull Request: #{}\n", pr.number));

    if let Some(conventions) = repo_conventions {
        if !conventions.trim().is_empty() {
            prompt.push_str("\n## Repository Conventions\n");
            prompt.push_str(conventions);
            prompt.push('\n');
        }
    }

    if let Some(custom) = repo_cfg.custom_instructions.as_deref() {
        if !custom.trim().is_empty() {
            prompt.push_str("\n## Custom Repository Instructions\n");
            prompt.push_str(custom);
            prompt.push('\n');
        }
    }

    prompt.push_str("\n## Context\n");
    prompt.push_str(context);

    prompt
}

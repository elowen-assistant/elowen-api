//! Pure formatting and normalization helpers for conversational and job output.

use serde_json::Value;

use crate::models::{ExecutionIntent, JobRecord, SummaryRecord};

pub(crate) fn execution_report_status<'a>(report: &'a Value, key: &str) -> Option<&'a str> {
    report.get(key)?.get("status")?.as_str()
}

pub(crate) fn execution_report_changed_entries(report: &Value) -> Option<usize> {
    report
        .get("changed_files")?
        .as_array()
        .map(|items| items.len())
}

pub(crate) fn execution_report_last_message(report: &Value) -> Option<&str> {
    report
        .get("last_message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn sanitize_chat_result_text(value: &str) -> String {
    let compact_links = compact_markdown_file_links(value.trim());
    compact_disposable_worktree_paths(&compact_links)
}

pub(crate) fn primary_result_excerpt(value: &str) -> String {
    let first_paragraph = value
        .split("\n\n")
        .map(str::trim)
        .find(|segment| !segment.is_empty())
        .unwrap_or(value.trim());
    truncate_text(&sanitize_chat_result_text(first_paragraph), 600)
}

pub(crate) fn format_success_reply(
    job: &JobRecord,
    build_status: Option<&str>,
    test_status: Option<&str>,
    changed_entries: Option<usize>,
    last_message: Option<&str>,
    approval_summary: &str,
) -> String {
    let mut lines = vec![format!(
        "I finished job `{}` for repo `{}` successfully.",
        job.short_id, job.repo_name
    )];

    if let Some(branch_name) = &job.branch_name {
        lines.push(format!("Branch: `{branch_name}`"));
    }
    if let Some(build_status) = build_status {
        lines.push(format!("Build: {build_status}"));
    }
    if let Some(test_status) = test_status {
        lines.push(format!("Test: {test_status}"));
    }
    if let Some(changed_entries) = changed_entries {
        lines.push(format!("Changed entries: {changed_entries}"));
    }
    if let Some(last_message) = last_message {
        lines.push(String::new());
        lines.push("Final result:".to_string());
        lines.push(truncate_text(last_message, 600));
    }

    lines.push(String::new());
    lines.push(approval_summary.to_string());
    lines.join("\n")
}

pub(crate) fn format_success_without_push_reply(
    job: &JobRecord,
    build_status: Option<&str>,
    test_status: Option<&str>,
    changed_entries: Option<usize>,
    last_message: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "I finished job `{}` for repo `{}` successfully.",
        job.short_id, job.repo_name
    )];

    if let Some(branch_name) = &job.branch_name {
        lines.push(format!("Branch: `{branch_name}`"));
    }
    if let Some(build_status) = build_status {
        lines.push(format!("Build: {build_status}"));
    }
    if let Some(test_status) = test_status {
        lines.push(format!("Test: {test_status}"));
    }
    if let Some(changed_entries) = changed_entries {
        lines.push(format!("Changed entries: {changed_entries}"));
    }
    if let Some(last_message) = last_message {
        lines.push(String::new());
        lines.push("Final result:".to_string());
        lines.push(truncate_text(last_message, 600));
    }

    lines.push(String::new());
    lines.push(
        "No committed repository changes were produced, so no push approval was required."
            .to_string(),
    );
    lines.join("\n")
}

pub(crate) fn format_read_only_success_reply(
    job: &JobRecord,
    build_status: Option<&str>,
    test_status: Option<&str>,
    changed_entries: Option<usize>,
    last_message: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "I finished read-only job `{}` for repo `{}` successfully.",
        job.short_id, job.repo_name
    )];

    if let Some(branch_name) = &job.branch_name {
        lines.push(format!("Branch: `{branch_name}`"));
    }
    if let Some(build_status) = build_status {
        lines.push(format!("Build: {build_status}"));
    }
    if let Some(test_status) = test_status {
        lines.push(format!("Test: {test_status}"));
    }
    if let Some(changed_entries) = changed_entries {
        lines.push(format!("Changed entries: {changed_entries}"));
    }
    if let Some(last_message) = last_message {
        lines.push(String::new());
        lines.push("Final result:".to_string());
        lines.push(truncate_text(last_message, 600));
    }

    lines.push(String::new());
    match changed_entries.unwrap_or(0) {
        0 => lines.push(
            "This ran in read-only mode, so no repository changes were produced and no push approval was required."
                .to_string(),
        ),
        _ => lines.push(
            "This ran in read-only mode. Any tracked changes were left uncommitted in the disposable worktree, and no push approval was opened."
                .to_string(),
        ),
    }

    lines.join("\n")
}

pub(crate) fn execution_intent_note(intent: &ExecutionIntent) -> &'static str {
    match intent {
        ExecutionIntent::WorkspaceChange => "workspace-change",
        ExecutionIntent::ReadOnly => "read-only",
    }
}

pub(crate) fn format_failure_reply(
    job: &JobRecord,
    build_status: Option<&str>,
    test_status: Option<&str>,
    changed_entries: Option<usize>,
    detail: Option<&str>,
    summary: Option<&SummaryRecord>,
) -> String {
    let mut lines = vec![format!(
        "I couldn't complete job `{}` for repo `{}`.",
        job.short_id, job.repo_name
    )];

    if let Some(branch_name) = &job.branch_name {
        lines.push(format!("Branch: `{branch_name}`"));
    }
    if let Some(build_status) = build_status {
        lines.push(format!("Build: {build_status}"));
    }
    if let Some(test_status) = test_status {
        lines.push(format!("Test: {test_status}"));
    }
    if let Some(changed_entries) = changed_entries {
        lines.push(format!("Changed entries: {changed_entries}"));
    }
    if let Some(detail) = detail {
        lines.push(format!("Detail: {}", truncate_text(detail, 240)));
    } else if let Some(summary) = summary.filter(|summary| !summary.content.trim().is_empty()) {
        lines.push(format!("Summary: {}", truncate_text(&summary.content, 240)));
    }

    lines.join("\n")
}

pub(crate) fn sanitize_string_list(values: Vec<String>) -> Vec<String> {
    let mut sanitized = Vec::new();

    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() || sanitized.iter().any(|item| item == trimmed) {
            continue;
        }

        sanitized.push(trimmed.to_string());
    }

    sanitized
}

pub(crate) fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|candidate| {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

pub(crate) fn derive_job_title_from_message(content: &str) -> String {
    let trimmed = content.trim();
    let first_line = trimmed
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("New coding task")
        .trim();
    let normalized = normalize_title_input(first_line);
    let lower = normalized.to_ascii_lowercase();
    let action = extract_title_action(&lower);
    let target = extract_title_target(&normalized, &lower);

    let mut title = match (action, target.as_deref()) {
        (Some(action), Some(target)) => format!("{action} {target}"),
        (Some(action), None) => action.to_string(),
        _ => normalized,
    };

    if title.chars().count() > 72 {
        title = title.chars().take(72).collect::<String>();
    }

    let title = title.trim().trim_end_matches(['.', '!', '?']).to_string();
    if title.is_empty() {
        "New coding task".to_string()
    } else {
        title
    }
}

fn normalize_title_input(value: &str) -> String {
    let mut normalized = value.trim().trim_end_matches(['.', '!', '?']).to_string();
    for prefix in [
        "For repo ",
        "For repository ",
        "In repo ",
        "In repository ",
        "Please ",
        "Can you ",
    ] {
        if normalized.starts_with(prefix) {
            normalized = normalized[prefix.len()..].trim_start().to_string();
        }
    }

    if normalized.starts_with('`')
        && let Some(end) = normalized[1..].find('`')
    {
        let remainder = normalized[(end + 2)..]
            .trim_start_matches([',', ':', ' '])
            .trim_start()
            .to_string();
        if !remainder.is_empty() {
            normalized = remainder;
        }
    }

    normalized
}

fn extract_title_action(lower: &str) -> Option<&'static str> {
    [
        ("append", "Update"),
        ("update", "Update"),
        ("edit", "Edit"),
        ("fix", "Fix"),
        ("implement", "Implement"),
        ("add", "Add"),
        ("remove", "Remove"),
        ("create", "Create"),
        ("refactor", "Refactor"),
        ("review", "Review"),
        ("document", "Document"),
        ("investigate", "Investigate"),
    ]
    .iter()
    .find_map(|(needle, label)| lower.contains(needle).then_some(*label))
}

fn extract_title_target(normalized: &str, lower: &str) -> Option<String> {
    if let Some(target) = extract_backticked_filename(normalized) {
        return Some(simplify_title_target(&target));
    }

    for needle in ["readme.md", "readme", "main.rs", "app.rs", "roadmap.md"] {
        if lower.contains(needle) {
            return Some(simplify_title_target(needle));
        }
    }

    let action_index = [
        "append",
        "update",
        "edit",
        "fix",
        "implement",
        "add",
        "remove",
        "create",
        "refactor",
        "review",
        "document",
        "investigate",
    ]
    .iter()
    .filter_map(|needle| lower.find(needle).map(|index| (index, *needle)))
    .min_by_key(|(index, _)| *index)?;
    let remainder = normalized[(action_index.0 + action_index.1.len())..].trim_start();
    let target = split_title_target(remainder);
    (!target.is_empty()).then(|| simplify_title_target(target))
}

fn extract_backticked_filename(value: &str) -> Option<String> {
    value
        .split('`')
        .skip(1)
        .step_by(2)
        .find(|candidate| candidate.contains('.') || candidate.contains('/'))
        .map(ToOwned::to_owned)
}

fn compact_markdown_file_links(value: &str) -> String {
    let mut output = String::new();
    let mut remainder = value;

    while let Some(start) = remainder.find('[') {
        output.push_str(&remainder[..start]);
        let Some(label_end) = remainder[start + 1..].find("](") else {
            output.push_str(&remainder[start..]);
            return output;
        };
        let label_end = start + 1 + label_end;
        let label = &remainder[start + 1..label_end];
        let after_label = &remainder[label_end + 2..];
        let Some(path_end) = after_label.find(')') else {
            output.push_str(&remainder[start..]);
            return output;
        };
        let path = &after_label[..path_end];
        output.push_str(&format!("`{}`", compact_file_reference(label, path)));
        remainder = &after_label[path_end + 1..];
    }

    output.push_str(remainder);
    output
}

fn compact_disposable_worktree_paths(value: &str) -> String {
    let mut sanitized = value.to_string();
    for marker in [
        "/.elowen/worktrees/",
        "\\.elowen\\worktrees\\",
        "/.elowen-sandbox/",
        "\\.elowen-sandbox\\",
    ] {
        if !sanitized.contains(marker) {
            continue;
        }
        sanitized = sanitized.replace('\\', "/");
    }
    sanitized
}

fn compact_file_reference(label: &str, path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(file_name) = normalized.rsplit('/').next() {
        if let Some((_, anchor)) = file_name.split_once('#') {
            return format!("{label}#{anchor}");
        }
    }
    label.to_string()
}

fn split_title_target(value: &str) -> &str {
    let lower = value.to_ascii_lowercase();
    let mut cutoff = value.len();
    for marker in [
        " by ",
        " and ",
        " with ",
        " using ",
        " so ",
        ". ",
        ", ",
        "; ",
        " make no other changes",
    ] {
        if let Some(index) = lower.find(marker) {
            cutoff = cutoff.min(index);
        }
    }
    value[..cutoff].trim()
}

fn simplify_title_target(value: &str) -> String {
    let trimmed = value.trim().trim_matches('`');
    let lower = trimmed.to_ascii_lowercase();
    if lower == "readme.md" || lower == "readme" {
        "README".to_string()
    } else if lower.ends_with(".md") || lower.ends_with(".rs") || lower.ends_with(".toml") {
        trimmed.to_string()
    } else {
        trimmed
            .split_whitespace()
            .take(6)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(crate) fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut truncated = value.trim().chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
}

pub(crate) fn slugify(value: &str) -> String {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() {
        "job".to_string()
    } else {
        trimmed.chars().take(32).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{derive_job_title_from_message, primary_result_excerpt, sanitize_chat_result_text};

    #[test]
    fn title_synthesis_uses_concise_labels() {
        assert_eq!(
            derive_job_title_from_message(
                "For repo `elowen-api`, update README.md by appending one sentence."
            ),
            "Update README"
        );
    }

    #[test]
    fn primary_result_excerpt_uses_first_paragraph_only() {
        assert_eq!(
            primary_result_excerpt("The answer is 42.\n\nExtra operational context."),
            "The answer is 42."
        );
    }

    #[test]
    fn sanitize_chat_result_text_compacts_markdown_file_links() {
        assert_eq!(
            sanitize_chat_result_text(
                "The Cargo package name is `elowen-api`, from [Cargo.toml](D:/Projects/elowen/.elowen/worktrees/elowen-api/01knactp/Cargo.toml#L2)."
            ),
            "The Cargo package name is `elowen-api`, from `Cargo.toml#L2`."
        );
    }
}

//! Conversational reply and execution-draft helpers.

use serde_json::{Value, json};
use tracing::warn;

use crate::{
    db::{approvals::load_job_approvals, jobs::load_current_job_summary},
    error::AppError,
    formatting::{derive_job_title_from_message, sanitize_optional_string},
    models::{
        ExecutionDraft, ExecutionIntent, JobRecord, MessageRecord, RelatedNoteContext,
        SummaryRecord, ThreadRecord,
    },
    state::AppState,
};

pub(crate) async fn generate_conversational_reply(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[RelatedNoteContext],
    execution_draft: Option<&ExecutionDraft>,
) -> Result<String, AppError> {
    if state.assistant.api_key.is_some() {
        match request_assistant_reply(
            state,
            thread,
            messages,
            jobs,
            related_notes,
            execution_draft,
        )
        .await
        {
            Ok(reply) => return Ok(reply),
            Err(error) => {
                warn!(error = ?error, thread_id = %thread.id, "conversational assistant request failed; using fallback reply");
            }
        }
    }

    Ok(build_fallback_conversational_reply(
        thread,
        messages,
        jobs,
        related_notes,
        execution_draft,
    ))
}

async fn request_assistant_reply(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[RelatedNoteContext],
    execution_draft: Option<&ExecutionDraft>,
) -> Result<String, AppError> {
    let api_key = state
        .assistant
        .api_key
        .as_deref()
        .ok_or_else(|| AppError::bad_request(anyhow::anyhow!("missing OPENAI_API_KEY")))?;
    let url = format!("{}/responses", state.assistant.base_url);
    let response = state
        .http
        .post(url)
        .bearer_auth(api_key)
        .json(&json!({
            "model": state.assistant.model,
            "instructions": build_conversation_instructions(),
            "input": build_conversation_input(
                &state.pool,
                thread,
                messages,
                jobs,
                related_notes,
                execution_draft,
            )
            .await?,
            "max_output_tokens": 600,
        }))
        .send()
        .await
        .map_err(AppError::from)?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow::anyhow!(
            "assistant response request failed with status {status}: {body}"
        )));
    }

    let body = response.json::<Value>().await.map_err(AppError::from)?;
    extract_response_text(&body).ok_or_else(|| {
        AppError::from(anyhow::anyhow!(
            "assistant response did not include any text output"
        ))
    })
}

fn build_conversation_instructions() -> &'static str {
    "You are Elowen, the orchestrator-side assistant in conversational mode. \
Reply conversationally and concisely. Use only the provided thread, job, and notes context. \
Do not claim to have dispatched a laptop job, modified code, run tests, or inspected a worktree unless the context explicitly says that already happened. \
When the user appears to want real code execution, planning, or repo changes, explain that execution happens through an explicit handoff and suggest using the dispatch controls when they are ready. \
If a current execution draft is present, treat it as editable planning context and help refine it without pretending a job has already been dispatched. Preserve a read-only draft as read-only unless the user clearly asks to make repository changes. \
If the context is incomplete, say so plainly rather than inventing details. \
When prior jobs, approvals, summaries, or notes materially support the answer, mention them briefly using labels like [Job 01abcd12] or [Note Title]."
}

async fn build_conversation_input(
    pool: &sqlx::PgPool,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[RelatedNoteContext],
    execution_draft: Option<&ExecutionDraft>,
) -> Result<String, AppError> {
    let latest_user = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| summarize_text(&message.content, 600))
        .unwrap_or_default();
    let job_context = load_conversation_job_context(pool, jobs, 4).await?;

    Ok(format!(
        "Thread title: {title}\n\
Thread status: {status}\n\
\n\
Latest user message:\n{latest_user}\n\
\n\
Recent thread messages:\n{message_context}\n\
\n\
Current execution draft:\n{draft_context}\n\
\n\
Recent related jobs:\n{job_context}\n\
\n\
Related notes:\n{note_context}\n",
        title = thread.title,
        status = thread.status,
        latest_user = latest_user,
        message_context = format_message_context(messages, 10),
        draft_context = format_execution_draft_context(execution_draft),
        job_context = format_job_context(&job_context),
        note_context = format_note_context(related_notes, 4),
    ))
}

fn format_message_context(messages: &[MessageRecord], limit: usize) -> String {
    if messages.is_empty() {
        return "- none".to_string();
    }

    messages
        .iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|message| {
            let mode = format_message_mode_label(message);
            format!(
                "- [{} | {}] {}",
                message.role,
                mode,
                summarize_text(&message.content, 280)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_execution_draft_context(execution_draft: Option<&ExecutionDraft>) -> String {
    let Some(draft) = execution_draft else {
        return "- none".to_string();
    };

    format!(
        "- title `{}`; repo `{}`; branch `{}`; intent `{}`; request {}; rationale {}",
        draft.title,
        draft.repo_name.as_deref().unwrap_or("unspecified"),
        draft.base_branch,
        draft.execution_intent.as_str(),
        summarize_text(&draft.request_text, 220),
        summarize_text(&draft.rationale, 160),
    )
}

struct ConversationJobContext {
    job: JobRecord,
    summary: Option<SummaryRecord>,
    pending_approval: Option<crate::models::ApprovalRecord>,
}

async fn load_conversation_job_context(
    pool: &sqlx::PgPool,
    jobs: &[JobRecord],
    limit: usize,
) -> Result<Vec<ConversationJobContext>, AppError> {
    let mut context = Vec::new();
    for job in jobs.iter().take(limit) {
        let summary = load_current_job_summary(pool, &job.id).await?;
        let pending_approval = load_job_approvals(pool, &job.id)
            .await?
            .into_iter()
            .find(|approval| approval.status == "pending");
        context.push(ConversationJobContext {
            job: job.clone(),
            summary,
            pending_approval,
        });
    }
    Ok(context)
}

fn format_job_context(job_context: &[ConversationJobContext]) -> String {
    if job_context.is_empty() {
        return "- none".to_string();
    }

    job_context
        .iter()
        .map(|entry| {
            let job = &entry.job;
            let mut parts = vec![format!(
                "- [Job {}] repo `{}` is `{}`",
                job.short_id, job.repo_name, job.status
            )];
            if let Some(result) = job.result.as_deref() {
                parts.push(format!("result `{result}`"));
            }
            if let Some(branch) = job.branch_name.as_deref() {
                parts.push(format!("branch `{branch}`"));
            }
            if let Some(summary) = entry.summary.as_ref() {
                parts.push(format!("summary {}", summarize_text(&summary.content, 160)));
            }
            if let Some(approval) = entry.pending_approval.as_ref() {
                parts.push(format!(
                    "pending approval {}",
                    summarize_text(&approval.summary, 120)
                ));
            }
            parts.join(", ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_note_context(notes: &[RelatedNoteContext], limit: usize) -> String {
    if notes.is_empty() {
        return "- none".to_string();
    }

    notes
        .iter()
        .take(limit)
        .map(|note| {
            let excerpt = note
                .detail_excerpt
                .as_deref()
                .map(|excerpt| format!(" | excerpt {}", summarize_text(excerpt, 180)))
                .unwrap_or_default();
            let reasons = if note.note.match_reasons.is_empty() {
                "none".to_string()
            } else {
                note.note.match_reasons.join(", ")
            };
            format!(
                "- [Note {}] role `{}` source `{}` kind `{}` score `{:.0}` reasons [{}] updated `{}`: {}{}",
                note.note.title,
                note.memory_role,
                note.source_label,
                note.note.source_kind.as_deref().unwrap_or("general"),
                note.note.relevance_score,
                reasons,
                note.note.updated_at.to_rfc3339(),
                summarize_text(&note.note.summary, 200),
                excerpt,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_message_mode_label(message: &MessageRecord) -> &'static str {
    if message.status == "conversation.reply" && message_execution_draft(message).is_some() {
        "conversation-draft"
    } else if message.status == "conversation.reply" {
        "conversation"
    } else if message.status == "workflow.handoff.created" {
        "handoff"
    } else if message.status == "workflow.dispatch.created" {
        "dispatch"
    } else if message.status.starts_with("job_event:") {
        "job-update"
    } else {
        "message"
    }
}

pub(crate) fn message_execution_draft(message: &MessageRecord) -> Option<ExecutionDraft> {
    message
        .payload_json
        .get("execution_draft")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

pub(crate) fn build_message_payload(execution_draft: Option<&ExecutionDraft>) -> Value {
    let Some(execution_draft) = execution_draft else {
        return json!({});
    };

    json!({
        "execution_draft": execution_draft,
    })
}

fn extract_response_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("output_text").and_then(Value::as_str) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    let text = value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|content| {
            content
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn build_fallback_conversational_reply(
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[RelatedNoteContext],
    execution_draft: Option<&ExecutionDraft>,
) -> String {
    let latest_user = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message.content.trim())
        .unwrap_or_default();
    let latest_job = jobs.first();

    if looks_like_execution_request(latest_user) {
        let repo_hint = latest_job
            .map(|job| {
                format!(
                    " The most recent job in this thread targeted repo `{}`.",
                    job.repo_name
                )
            })
            .unwrap_or_default();
        if let Some(draft) = execution_draft {
            return format!(
                "I'm still in conversational mode for thread `{}`. I prepared a draft for repo `{}` on base branch `{}` below so you can refine it before you dispatch anything.{}",
                thread.title,
                draft.repo_name.as_deref().unwrap_or("unspecified"),
                draft.base_branch,
                repo_hint
            );
        }
        return format!(
            "I'm in conversational mode for thread `{}` right now, so I have not created a laptop job yet. I can help refine the request or you can use the explicit dispatch controls when you're ready.{}",
            thread.title, repo_hint
        );
    }

    if let Some(draft) = execution_draft {
        return format!(
            "I'm still in conversational mode, and I prepared an execution draft for repo `{}` on base branch `{}` from the latest request. You can keep refining it here before explicitly dispatching it.",
            draft.repo_name.as_deref().unwrap_or("unspecified"),
            draft.base_branch
        );
    }

    if let Some(job) = latest_job {
        return format!(
            "I'm here in conversational mode. The latest job in this thread is `{}` for repo `{}`, currently `{}`{}.\n\nI can help you reason about the next step, summarize what happened, or prepare an explicit handoff into laptop execution when you want it.",
            job.short_id,
            job.repo_name,
            job.status,
            job.result
                .as_deref()
                .map(|result| format!(", with result `{result}`"))
                .unwrap_or_default()
        );
    }

    if let Some(note) = related_notes.first() {
        return format!(
            "I'm here in conversational mode. I can answer questions about this thread, help plan work, or prepare an explicit laptop dispatch when needed.\n\nThe closest related note I found is `{}`: {}",
            note.note.title,
            summarize_text(&note.note.summary, 160)
        );
    }

    "I'm here in conversational mode. Ask me questions, work through a plan with me, or use the explicit dispatch controls when you want me to create a real laptop job.".to_string()
}

pub(crate) fn looks_like_execution_request(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "implement ",
        "fix ",
        "change ",
        "edit ",
        "update ",
        "run ",
        "dispatch ",
        "create a job",
        "send to laptop",
        "review the code",
        "write code",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_like_read_only_request(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let explicit_read_only = [
        "read-only",
        "read only",
        "do not modify",
        "don't modify",
        "do not change",
        "don't change",
        "no changes",
        "without changing",
        "without modifying",
        "do not create commits",
        "do not request push approval",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    let informational_request = [
        "what ",
        "which ",
        "explain ",
        "summarize ",
        "summarise ",
        "review ",
        "inspect ",
        "tell me ",
        "report ",
        "find ",
        "identify ",
        "show ",
        "read ",
        "where ",
        "why ",
        "how does ",
    ]
    .iter()
    .any(|needle| lower.contains(needle));

    explicit_read_only || informational_request
}

pub(crate) fn infer_execution_intent(value: &str) -> ExecutionIntent {
    if looks_like_read_only_request(value) {
        ExecutionIntent::ReadOnly
    } else {
        ExecutionIntent::WorkspaceChange
    }
}

fn looks_like_draft_refinement(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "repo ",
        "repository ",
        "branch ",
        "base branch",
        "title ",
        "call it ",
        "rename ",
        "instead",
        "use ",
        "send it",
        "dispatch it",
        "run it",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DraftRefinement {
    repo_name: Option<String>,
    base_branch: Option<String>,
    title: Option<String>,
    request_update: Option<String>,
}

pub(crate) fn maybe_build_execution_draft(
    messages: &[MessageRecord],
    jobs: &[JobRecord],
) -> Option<ExecutionDraft> {
    let latest_user = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")?;
    let latest_text = latest_user.content.trim();
    let previous_draft = messages.iter().rev().find_map(message_execution_draft);
    let request_is_execution = looks_like_execution_request(latest_text);
    let should_draft = request_is_execution
        || (previous_draft.is_some() && looks_like_draft_refinement(latest_text));
    if !should_draft {
        return None;
    }

    let refinement = if previous_draft.is_some() && !request_is_execution {
        parse_draft_refinement(latest_text)
    } else {
        DraftRefinement::default()
    };
    let request_text = if request_is_execution {
        latest_text.to_string()
    } else if let Some(previous_draft) = previous_draft.as_ref() {
        merge_request_text(
            previous_draft.request_text.trim(),
            refinement.request_update.as_deref(),
        )
    } else {
        latest_text.to_string()
    };
    let repo_name = refinement
        .repo_name
        .or_else(|| extract_repo_hint(latest_text))
        .or_else(|| {
            previous_draft
                .as_ref()
                .and_then(|draft| draft.repo_name.clone())
        })
        .or_else(|| jobs.first().map(|job| job.repo_name.clone()));
    let base_branch = refinement
        .base_branch
        .or_else(|| extract_base_branch_hint(latest_text))
        .or_else(|| {
            previous_draft
                .as_ref()
                .map(|draft| draft.base_branch.clone())
        })
        .or_else(|| jobs.first().and_then(|job| job.base_branch.clone()))
        .unwrap_or_else(|| "main".to_string());
    let execution_intent = if request_is_execution {
        infer_execution_intent(latest_text)
    } else if let Some(intent_override) = extract_execution_intent_override(latest_text) {
        intent_override
    } else {
        previous_draft
            .as_ref()
            .map(|draft| draft.execution_intent.clone())
            .unwrap_or_else(|| infer_execution_intent(latest_text))
    };
    let title = if let Some(title) = refinement.title {
        title
    } else if request_is_execution {
        derive_job_title_from_message(&request_text)
    } else if let Some(request_update) = refinement.request_update.as_deref() {
        derive_job_title_from_message(request_update)
    } else {
        let request_changed = previous_draft
            .as_ref()
            .map(|draft| draft.request_text.trim() != request_text.trim())
            .unwrap_or(true);
        previous_draft
            .as_ref()
            .filter(|_| !request_changed)
            .map(|draft| draft.title.clone())
            .unwrap_or_else(|| derive_job_title_from_message(&request_text))
    };
    let rationale = if previous_draft.is_some() && looks_like_draft_refinement(latest_text) {
        "Updated the draft using the latest conversational refinement without dispatching a job."
            .to_string()
    } else if matches!(execution_intent, ExecutionIntent::ReadOnly) {
        "Prepared as a read-only repository investigation so the laptop can inspect and report without creating durable repo changes.".to_string()
    } else {
        "Prepared from the latest user request so it can be reviewed before dispatch.".to_string()
    };

    Some(ExecutionDraft {
        title,
        repo_name,
        base_branch,
        request_text,
        execution_intent,
        source_message_id: latest_user.id.clone(),
        source_role: latest_user.role.clone(),
        rationale,
    })
}

fn extract_repo_hint(value: &str) -> Option<String> {
    extract_backticked_hint(value, "repo")
        .or_else(|| extract_backticked_hint(value, "repository"))
        .or_else(|| extract_keyword_value(value, "repo"))
        .or_else(|| extract_keyword_value(value, "repository"))
}

fn extract_base_branch_hint(value: &str) -> Option<String> {
    extract_backticked_hint(value, "base branch")
        .or_else(|| extract_backticked_hint(value, "branch"))
        .or_else(|| extract_keyword_value(value, "base branch"))
        .or_else(|| extract_keyword_value(value, "branch"))
}

fn extract_title_hint(value: &str) -> Option<String> {
    extract_backticked_hint(value, "title")
        .or_else(|| extract_phrase_value(value, "title"))
        .or_else(|| extract_phrase_value(value, "call it"))
        .or_else(|| extract_phrase_value(value, "rename it to"))
        .or_else(|| extract_phrase_value(value, "rename this to"))
        .or_else(|| extract_phrase_value(value, "rename to"))
}

fn extract_backticked_hint(value: &str, prefix: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let pattern = format!("{prefix} `");
    let start = lower.find(&pattern)?;
    let original = &value[start + pattern.len()..];
    let end = original.find('`')?;
    sanitize_optional_string(Some(original[..end].to_string()))
}

fn extract_keyword_value(value: &str, prefix: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let start = lower.find(prefix)?;
    let original = value[start + prefix.len()..].trim_start_matches([' ', ':']);
    let token = original
        .split_whitespace()
        .next()
        .map(|part| {
            part.trim_matches(|ch: char| {
                ch == ',' || ch == '.' || ch == ';' || ch == ':' || ch == ')' || ch == '('
            })
        })
        .unwrap_or_default();
    sanitize_optional_string(Some(token.to_string()))
}

fn extract_phrase_value(value: &str, prefix: &str) -> Option<String> {
    let lower = value.to_ascii_lowercase();
    let start = lower.find(prefix)?;
    let original = value[start + prefix.len()..].trim_start_matches([' ', ':', '-', '=']);
    if let Some(stripped) = original.strip_prefix('`') {
        let end = stripped.find('`')?;
        return sanitize_optional_string(Some(stripped[..end].trim().to_string()));
    }

    let cutoff = [
        "\n",
        ".",
        ";",
        ", and ",
        " and use ",
        " and switch ",
        " and set ",
        " and keep ",
    ]
    .iter()
    .filter_map(|marker| original.find(marker))
    .min()
    .unwrap_or(original.len());
    sanitize_optional_string(Some(
        original[..cutoff].trim_matches('`').trim().to_string(),
    ))
}

fn extract_execution_intent_override(value: &str) -> Option<ExecutionIntent> {
    let lower = value.to_ascii_lowercase();
    if [
        "read-only",
        "read only",
        "do not modify",
        "don't modify",
        "do not change",
        "don't change",
        "no changes",
        "without changing",
        "without modifying",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some(ExecutionIntent::ReadOnly)
    } else if [
        "allow changes",
        "make changes",
        "modify the repo",
        "workspace change",
        "go ahead and change",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some(ExecutionIntent::WorkspaceChange)
    } else {
        None
    }
}

fn parse_draft_refinement(value: &str) -> DraftRefinement {
    DraftRefinement {
        repo_name: extract_repo_hint(value),
        base_branch: extract_base_branch_hint(value),
        title: extract_title_hint(value),
        request_update: extract_refinement_request_update(value),
    }
}

fn merge_request_text(previous_request: &str, request_update: Option<&str>) -> String {
    let Some(update) = request_update
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return previous_request.to_string();
    };

    if previous_request.trim() == update {
        previous_request.to_string()
    } else {
        format!("{previous_request}\n\nAdditional instruction: {update}")
    }
}

fn extract_refinement_request_update(value: &str) -> Option<String> {
    let mut request_fragments = Vec::new();

    for raw_clause in value.split(['\n', ';', '.']) {
        let clause = raw_clause.trim();
        if clause.is_empty() {
            continue;
        }

        if let Some(fragment) = strip_metadata_prefix(clause) {
            if !fragment.is_empty() && !is_metadata_only_clause(&fragment) {
                request_fragments.push(fragment);
            }
            continue;
        }

        if is_metadata_only_clause(clause) {
            continue;
        }

        request_fragments.push(clause.to_string());
    }

    sanitize_optional_string(Some(request_fragments.join(" ")))
}

fn strip_metadata_prefix(clause: &str) -> Option<String> {
    let lower = clause.to_ascii_lowercase();
    let prefixes = [
        "repo ",
        "repository ",
        "use repo ",
        "use repository ",
        "base branch ",
        "branch ",
        "use branch ",
        "use base branch ",
        "title ",
        "call it ",
        "rename it to ",
        "rename this to ",
        "rename to ",
    ];

    if prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        for marker in [" and ", ", then ", ", "] {
            if let Some(index) = lower.find(marker) {
                let remainder = clause[index + marker.len()..].trim();
                return Some(remainder.to_string());
            }
        }
        return Some(String::new());
    }

    None
}

fn is_metadata_only_clause(clause: &str) -> bool {
    let lower = clause.to_ascii_lowercase();
    if [
        "send it",
        "dispatch it",
        "run it",
        "keep it read-only",
        "make it read-only",
        "leave it read-only",
    ]
    .iter()
    .any(|needle| lower == *needle)
    {
        return true;
    }

    extract_title_hint(clause).is_some()
        || extract_repo_hint(clause).is_some()
        || extract_base_branch_hint(clause).is_some()
        || extract_execution_intent_override(clause).is_some()
}

pub(crate) fn maybe_annotate_draft_reply(
    reply: String,
    execution_draft: Option<&ExecutionDraft>,
) -> String {
    let Some(_) = execution_draft else {
        return reply;
    };

    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return "I prepared an execution draft below so you can keep refining it before dispatch."
            .to_string();
    }

    trimmed.to_string()
}

pub(crate) fn summarize_text(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        trimmed.chars().take(limit).collect::<String>()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DraftRefinement, extract_refinement_request_update, extract_title_hint,
        maybe_annotate_draft_reply, maybe_build_execution_draft, parse_draft_refinement,
    };
    use crate::models::{ExecutionDraft, ExecutionIntent, JobRecord, MessageRecord};
    use chrono::Utc;
    use serde_json::json;

    fn user_message(id: &str, content: &str) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            thread_id: "thread-1".to_string(),
            role: "user".to_string(),
            content: content.to_string(),
            status: "conversation.user".to_string(),
            payload_json: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn assistant_draft_message(id: &str, draft: &ExecutionDraft) -> MessageRecord {
        MessageRecord {
            id: id.to_string(),
            thread_id: "thread-1".to_string(),
            role: "assistant".to_string(),
            content: "draft".to_string(),
            status: "conversation.reply".to_string(),
            payload_json: json!({
                "execution_draft": draft
            }),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn previous_draft() -> ExecutionDraft {
        ExecutionDraft {
            title: "Polish chat shell".to_string(),
            repo_name: Some("elowen-ui".to_string()),
            base_branch: "main".to_string(),
            request_text: "Tighten the chat shell spacing and improve result presentation."
                .to_string(),
            execution_intent: ExecutionIntent::WorkspaceChange,
            source_message_id: "u1".to_string(),
            source_role: "user".to_string(),
            rationale:
                "Prepared from the latest user request so it can be reviewed before dispatch."
                    .to_string(),
        }
    }

    fn sample_job() -> JobRecord {
        JobRecord {
            id: "job-1".to_string(),
            short_id: "job-1".to_string(),
            correlation_id: "corr-1".to_string(),
            thread_id: "thread-1".to_string(),
            title: "Existing".to_string(),
            status: "completed".to_string(),
            result: Some("success".to_string()),
            failure_class: None,
            repo_name: "elowen-api".to_string(),
            device_id: Some("edge-1".to_string()),
            branch_name: Some("codex/job-1-existing".to_string()),
            base_branch: Some("main".to_string()),
            parent_job_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
        }
    }

    #[test]
    fn parses_explicit_refinement_overrides_and_request_update() {
        assert_eq!(
            parse_draft_refinement(
                "Use repo `elowen-api`, branch `release/2026`, call it `API transcript polish`, and add failure detail disclosure."
            ),
            DraftRefinement {
                repo_name: Some("elowen-api".to_string()),
                base_branch: Some("release/2026".to_string()),
                title: Some("API transcript polish".to_string()),
                request_update: Some("add failure detail disclosure".to_string()),
            }
        );
    }

    #[test]
    fn metadata_only_refinement_does_not_pollute_request_text() {
        let draft = previous_draft();
        let messages = vec![
            user_message(
                "u1",
                "Tighten the chat shell spacing and improve result presentation.",
            ),
            assistant_draft_message("a1", &draft),
            user_message(
                "u2",
                "Use repo `elowen-api`, base branch `release/2026`, and call it `API transcript polish`.",
            ),
        ];

        let next_draft = maybe_build_execution_draft(&messages, &[sample_job()]).unwrap();

        assert_eq!(next_draft.repo_name.as_deref(), Some("elowen-api"));
        assert_eq!(next_draft.base_branch, "release/2026");
        assert_eq!(next_draft.title, "API transcript polish");
        assert_eq!(
            next_draft.request_text,
            "Tighten the chat shell spacing and improve result presentation."
        );
    }

    #[test]
    fn substantive_refinement_appends_only_residual_instruction() {
        let draft = previous_draft();
        let messages = vec![
            user_message(
                "u1",
                "Tighten the chat shell spacing and improve result presentation.",
            ),
            assistant_draft_message("a1", &draft),
            user_message(
                "u2",
                "Use repo `elowen-ui` and add local timestamp formatting in the thread transcript.",
            ),
        ];

        let next_draft = maybe_build_execution_draft(&messages, &[sample_job()]).unwrap();

        assert_eq!(
            next_draft.request_text,
            "Tighten the chat shell spacing and improve result presentation.\n\nAdditional instruction: add local timestamp formatting in the thread transcript"
        );
        assert_eq!(
            next_draft.title,
            "Add local timestamp formatting in the thread"
        );
    }

    #[test]
    fn empty_draft_reply_uses_single_line_prompt() {
        assert_eq!(
            maybe_annotate_draft_reply("".to_string(), Some(&previous_draft())),
            "I prepared an execution draft below so you can keep refining it before dispatch."
        );
    }

    #[test]
    fn non_empty_draft_reply_is_not_padded_with_duplicate_handoff_copy() {
        assert_eq!(
            maybe_annotate_draft_reply(
                "Let's tighten the transcript surfaces first.".to_string(),
                Some(&previous_draft()),
            ),
            "Let's tighten the transcript surfaces first."
        );
    }

    #[test]
    fn extracts_title_hint_from_rename_phrase() {
        assert_eq!(
            extract_title_hint("Rename it to `Transcript polish` and keep it read-only."),
            Some("Transcript polish".to_string())
        );
    }

    #[test]
    fn refinement_request_update_ignores_metadata_only_clauses() {
        assert_eq!(
            extract_refinement_request_update(
                "Use repo `elowen-ui`. Base branch `release/2026`. Keep it read-only."
            ),
            None
        );
    }
}

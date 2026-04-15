//! Conversational reply and execution-draft helpers.

use serde_json::{Value, json};
use tracing::warn;

use crate::{
    db::{approvals::load_job_approvals, jobs::load_current_job_summary},
    error::AppError,
    formatting::{derive_job_title_from_message, sanitize_optional_string},
    models::{
        ExecutionDraft, ExecutionIntent, JobRecord, MessageRecord, NoteRecord, SummaryRecord,
        ThreadRecord,
    },
    state::AppState,
};

pub(crate) async fn generate_conversational_reply(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[NoteRecord],
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
    related_notes: &[NoteRecord],
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
    related_notes: &[NoteRecord],
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

fn format_note_context(notes: &[NoteRecord], limit: usize) -> String {
    if notes.is_empty() {
        return "- none".to_string();
    }

    notes
        .iter()
        .take(limit)
        .map(|note| {
            format!(
                "- [Note {}] kind `{}` updated `{}`: {}",
                note.title,
                note.source_kind.as_deref().unwrap_or("general"),
                note.updated_at.to_rfc3339(),
                summarize_text(&note.summary, 200)
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
    related_notes: &[NoteRecord],
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
        return format!(
            "I’m in conversational mode for thread `{}` right now, so I have not created a laptop job yet. I can help refine the request or you can use the explicit dispatch controls to hand this off when you’re ready.{}",
            thread.title, repo_hint
        );
    }

    if let Some(draft) = execution_draft {
        return format!(
            "I’m still in conversational mode, and I prepared an execution draft for repo `{}` on branch `{}` from the latest request. You can keep refining it here before explicitly dispatching it.",
            draft.repo_name.as_deref().unwrap_or("unspecified"),
            draft.base_branch
        );
    }

    if let Some(job) = latest_job {
        return format!(
            "I’m here in conversational mode. The latest job in this thread is `{}` for repo `{}`, currently `{}`{}.\n\nI can help you reason about the next step, summarize what happened, or prepare an explicit handoff into laptop execution when you want it.",
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
            "I’m here in conversational mode. I can answer questions about this thread, help plan work, or prepare an explicit laptop dispatch when needed.\n\nThe closest related note I found is `{}`: {}",
            note.title,
            summarize_text(&note.summary, 160)
        );
    }

    "I’m here in conversational mode. Ask me questions, work through a plan with me, or use the explicit dispatch controls when you want me to create a real laptop job.".to_string()
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
    let should_draft = looks_like_execution_request(latest_text)
        || (previous_draft.is_some() && looks_like_draft_refinement(latest_text));
    if !should_draft {
        return None;
    }

    let repo_name = extract_repo_hint(latest_text)
        .or_else(|| {
            previous_draft
                .as_ref()
                .and_then(|draft| draft.repo_name.clone())
        })
        .or_else(|| jobs.first().map(|job| job.repo_name.clone()));
    let base_branch = extract_base_branch_hint(latest_text)
        .or_else(|| {
            previous_draft
                .as_ref()
                .map(|draft| draft.base_branch.clone())
        })
        .or_else(|| jobs.first().and_then(|job| job.base_branch.clone()))
        .unwrap_or_else(|| "main".to_string());
    let execution_intent = if !looks_like_execution_request(latest_text) {
        previous_draft
            .as_ref()
            .map(|draft| draft.execution_intent.clone())
            .unwrap_or_else(|| infer_execution_intent(latest_text))
    } else {
        infer_execution_intent(latest_text)
    };
    let request_text = if let Some(previous_draft) = previous_draft.as_ref() {
        if !looks_like_execution_request(latest_text) && looks_like_draft_refinement(latest_text) {
            format!(
                "{}\n\nRefinement: {}",
                previous_draft.request_text.trim(),
                latest_text
            )
        } else {
            latest_text.to_string()
        }
    } else {
        latest_text.to_string()
    };
    let title = if !looks_like_execution_request(latest_text) {
        previous_draft
            .as_ref()
            .map(|draft| draft.title.clone())
            .unwrap_or_else(|| derive_job_title_from_message(&request_text))
    } else {
        derive_job_title_from_message(&request_text)
    };
    let rationale = if previous_draft.is_some() && looks_like_draft_refinement(latest_text) {
        "Updated the draft using the latest conversational refinement.".to_string()
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

pub(crate) fn maybe_annotate_draft_reply(
    reply: String,
    execution_draft: Option<&ExecutionDraft>,
) -> String {
    let Some(execution_draft) = execution_draft else {
        return reply;
    };

    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return format!(
            "I prepared an execution draft for repo `{}` on branch `{}`. Review it below before dispatching it.",
            execution_draft
                .repo_name
                .as_deref()
                .unwrap_or("unspecified"),
            execution_draft.base_branch
        );
    }

    format!(
        "{trimmed}\n\nI also drafted an execution handoff below so you can review and refine it before dispatching it."
    )
}

pub(crate) fn summarize_text(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        trimmed.chars().take(limit).collect::<String>()
    }
}

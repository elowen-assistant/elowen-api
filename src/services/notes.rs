//! Notes-service integration helpers.

use std::collections::HashMap;

use anyhow::anyhow;

use crate::{
    error::AppError,
    models::{
        JobRecord, MessageRecord, NoteDetailRecord, NoteRecord, PromoteNoteRequest,
        RelatedNoteContext, SummaryRecord, ThreadRecord,
    },
    state::AppState,
};

const THREAD_MEMORY_BOOST: f64 = 1_000.0;
const JOB_MEMORY_BOOST: f64 = 900.0;
const DETAIL_EXPANSION_LIMIT: usize = 3;

#[derive(Debug, Default)]
pub(crate) struct RelatedNotesBundle {
    pub(crate) notes: Vec<NoteRecord>,
    pub(crate) conversation_context: Vec<RelatedNoteContext>,
}

#[derive(Debug, Default)]
pub(crate) struct SearchNotesRequest<'a> {
    pub(crate) q: Option<&'a str>,
    pub(crate) context: Option<&'a str>,
    pub(crate) source_kind: Option<&'a str>,
    pub(crate) source_id: Option<&'a str>,
    pub(crate) prefer_note_ids: Vec<String>,
    pub(crate) prefer_source_kind: Option<&'a str>,
    pub(crate) prefer_source_id: Option<&'a str>,
    pub(crate) limit: usize,
}

#[derive(Debug)]
struct NoteContextCandidate {
    note: NoteRecord,
    memory_role: String,
    source_label: String,
}

#[derive(Debug)]
struct MergedNoteContext {
    note: NoteRecord,
    memory_role: String,
    source_label: String,
    effective_score: f64,
}

pub(crate) async fn load_related_thread_notes(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
) -> Result<Vec<NoteRecord>, AppError> {
    Ok(
        load_related_thread_notes_bundle(state, thread, messages, jobs)
            .await?
            .notes,
    )
}

pub(crate) async fn load_related_thread_note_context(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
) -> Result<Vec<RelatedNoteContext>, AppError> {
    Ok(
        load_related_thread_notes_bundle(state, thread, messages, jobs)
            .await?
            .conversation_context,
    )
}

pub(crate) async fn load_related_job_notes(
    state: &AppState,
    job: &JobRecord,
    summary: Option<&SummaryRecord>,
) -> Result<Vec<NoteRecord>, AppError> {
    Ok(load_related_job_notes_bundle(state, job, summary)
        .await?
        .notes)
}

async fn load_related_thread_notes_bundle(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
) -> Result<RelatedNotesBundle, AppError> {
    let mut candidates = Vec::new();
    let mut preferred_note_ids = Vec::new();

    let thread_notes = search_notes(
        state,
        SearchNotesRequest {
            source_kind: Some("thread"),
            source_id: Some(thread.id.as_str()),
            limit: 8,
            ..SearchNotesRequest::default()
        },
    )
    .await?;
    preferred_note_ids.extend(thread_notes.iter().map(|note| note.note_id.clone()));
    candidates.extend(thread_notes.into_iter().map(|note| NoteContextCandidate {
        note,
        memory_role: "direct_thread_memory".to_string(),
        source_label: format!("Promoted from thread `{}`", thread.title),
    }));

    for job in jobs {
        let job_notes = search_notes(
            state,
            SearchNotesRequest {
                source_kind: Some("job"),
                source_id: Some(job.id.as_str()),
                limit: 4,
                ..SearchNotesRequest::default()
            },
        )
        .await?;
        preferred_note_ids.extend(job_notes.iter().map(|note| note.note_id.clone()));
        candidates.extend(job_notes.into_iter().map(|note| NoteContextCandidate {
            note,
            memory_role: "direct_job_memory".to_string(),
            source_label: format!("Promoted from job `{}`", job.short_id),
        }));
    }

    let query = build_thread_notes_query(thread, messages);
    let context = build_thread_notes_context(thread, messages, jobs);
    let discovered = search_notes(
        state,
        SearchNotesRequest {
            q: query.as_deref(),
            context: context.as_deref(),
            prefer_note_ids: preferred_note_ids.clone(),
            prefer_source_kind: Some("thread"),
            prefer_source_id: Some(thread.id.as_str()),
            limit: 8,
            ..SearchNotesRequest::default()
        },
    )
    .await?;
    candidates.extend(discovered.into_iter().map(|note| NoteContextCandidate {
        note,
        memory_role: "retrieved_memory".to_string(),
        source_label: "Retrieved by note search".to_string(),
    }));

    finalize_related_notes_bundle(state, candidates).await
}

async fn load_related_job_notes_bundle(
    state: &AppState,
    job: &JobRecord,
    summary: Option<&SummaryRecord>,
) -> Result<RelatedNotesBundle, AppError> {
    let mut candidates = Vec::new();
    let direct_notes = search_notes(
        state,
        SearchNotesRequest {
            source_kind: Some("job"),
            source_id: Some(job.id.as_str()),
            limit: 8,
            ..SearchNotesRequest::default()
        },
    )
    .await?;
    let preferred_note_ids = direct_notes
        .iter()
        .map(|note| note.note_id.clone())
        .collect::<Vec<_>>();
    candidates.extend(direct_notes.into_iter().map(|note| NoteContextCandidate {
        note,
        memory_role: "direct_job_memory".to_string(),
        source_label: format!("Promoted from job `{}`", job.short_id),
    }));

    let query = build_job_notes_query(job, summary);
    let context = build_job_notes_context(job, summary);
    let discovered = search_notes(
        state,
        SearchNotesRequest {
            q: query.as_deref(),
            context: context.as_deref(),
            prefer_note_ids: preferred_note_ids,
            prefer_source_kind: Some("job"),
            prefer_source_id: Some(job.id.as_str()),
            limit: 8,
            ..SearchNotesRequest::default()
        },
    )
    .await?;
    candidates.extend(discovered.into_iter().map(|note| NoteContextCandidate {
        note,
        memory_role: "retrieved_memory".to_string(),
        source_label: "Retrieved by note search".to_string(),
    }));

    finalize_related_notes_bundle(state, candidates).await
}

async fn finalize_related_notes_bundle(
    state: &AppState,
    candidates: Vec<NoteContextCandidate>,
) -> Result<RelatedNotesBundle, AppError> {
    let merged = merge_related_note_contexts(candidates);
    let details = load_note_details(
        state,
        merged
            .iter()
            .take(DETAIL_EXPANSION_LIMIT)
            .map(|entry| entry.note.note_id.as_str())
            .collect(),
    )
    .await?;

    let notes = merged
        .iter()
        .map(|entry| entry.note.clone())
        .collect::<Vec<_>>();
    let conversation_context = merged
        .into_iter()
        .map(|entry| RelatedNoteContext {
            detail_excerpt: details
                .get(&entry.note.note_id)
                .map(|detail| summarize_text(&detail.revision.body_markdown, 280)),
            note: entry.note,
            memory_role: entry.memory_role,
            source_label: entry.source_label,
        })
        .collect();

    Ok(RelatedNotesBundle {
        notes,
        conversation_context,
    })
}

pub(crate) async fn search_notes(
    state: &AppState,
    request: SearchNotesRequest<'_>,
) -> Result<Vec<NoteRecord>, AppError> {
    let url = format!("{}/api/v1/notes/search", state.notes_url);
    let mut query = vec![("limit".to_string(), request.limit.to_string())];

    push_query_value(&mut query, "q", request.q);
    push_query_value(&mut query, "context", request.context);
    push_query_value(&mut query, "source_kind", request.source_kind);
    push_query_value(&mut query, "source_id", request.source_id);
    push_query_value(&mut query, "prefer_source_kind", request.prefer_source_kind);
    push_query_value(&mut query, "prefer_source_id", request.prefer_source_id);
    if !request.prefer_note_ids.is_empty() {
        query.push((
            "prefer_note_ids".to_string(),
            request.prefer_note_ids.join(","),
        ));
    }

    let response = state
        .http
        .get(url)
        .query(&query)
        .send()
        .await
        .map_err(|error| AppError::from(anyhow!(error)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow!(
            "notes search failed with status {status}: {body}"
        )));
    }

    response
        .json::<Vec<NoteRecord>>()
        .await
        .map_err(|error| AppError::from(anyhow!(error)))
}

async fn load_note_details(
    state: &AppState,
    note_ids: Vec<&str>,
) -> Result<HashMap<String, NoteDetailRecord>, AppError> {
    let mut details = HashMap::new();
    for note_id in note_ids {
        if let Some(detail) = load_note_detail(state, note_id).await? {
            details.insert(note_id.to_string(), detail);
        }
    }
    Ok(details)
}

async fn load_note_detail(
    state: &AppState,
    note_id: &str,
) -> Result<Option<NoteDetailRecord>, AppError> {
    let url = format!("{}/api/v1/notes/{}", state.notes_url, note_id);
    let response = state
        .http
        .get(url)
        .send()
        .await
        .map_err(|error| AppError::from(anyhow!(error)))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow!(
            "note detail lookup failed with status {status}: {body}"
        )));
    }

    response
        .json::<NoteDetailRecord>()
        .await
        .map(Some)
        .map_err(|error| AppError::from(anyhow!(error)))
}

pub(crate) async fn promote_note_to_service(
    state: &AppState,
    request: PromoteNoteRequest,
) -> Result<NoteRecord, AppError> {
    let url = format!("{}/api/v1/notes/promotions", state.notes_url);
    let response = state
        .http
        .post(url)
        .json(&request)
        .send()
        .await
        .map_err(|error| AppError::from(anyhow!(error)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow!(
            "note promotion failed with status {status}: {body}"
        )));
    }

    let detail = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| AppError::from(anyhow!(error)))?;
    let note = detail.get("note").cloned().unwrap_or(detail);

    serde_json::from_value::<NoteRecord>(note).map_err(|error| AppError::from(anyhow!(error)))
}

fn push_query_value(query: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        query.push((key.to_string(), value.to_string()));
    }
}

fn merge_related_note_contexts(candidates: Vec<NoteContextCandidate>) -> Vec<MergedNoteContext> {
    let mut merged: HashMap<String, MergedNoteContext> = HashMap::new();

    for candidate in candidates {
        let boost = memory_role_boost(&candidate.memory_role);
        let effective_score = candidate.note.relevance_score + boost;
        let note_id = candidate.note.note_id.clone();
        merged
            .entry(note_id)
            .and_modify(|existing| {
                if effective_score > existing.effective_score {
                    existing.note.summary = candidate.note.summary.clone();
                    existing.note.relevance_score = effective_score;
                    existing.note.updated_at = candidate.note.updated_at;
                    existing.memory_role = candidate.memory_role.clone();
                    existing.source_label = candidate.source_label.clone();
                }

                for reason in &candidate.note.match_reasons {
                    if !existing.note.match_reasons.contains(reason) {
                        existing.note.match_reasons.push(reason.clone());
                    }
                }

                let boosted_reason = memory_role_reason(&candidate.memory_role);
                if !existing
                    .note
                    .match_reasons
                    .iter()
                    .any(|reason| reason == boosted_reason)
                {
                    existing.note.match_reasons.push(boosted_reason.to_string());
                }
                existing.note.relevance_score = existing.note.relevance_score.max(effective_score);
                existing.effective_score = existing.effective_score.max(effective_score);
            })
            .or_insert_with(|| {
                let mut note = candidate.note;
                note.relevance_score = effective_score;
                let boosted_reason = memory_role_reason(&candidate.memory_role).to_string();
                if !note
                    .match_reasons
                    .iter()
                    .any(|reason| reason == &boosted_reason)
                {
                    note.match_reasons.push(boosted_reason);
                }

                MergedNoteContext {
                    note,
                    memory_role: candidate.memory_role,
                    source_label: candidate.source_label,
                    effective_score,
                }
            });
    }

    let mut merged = merged.into_values().collect::<Vec<_>>();
    merged.sort_by(|left, right| {
        right
            .effective_score
            .partial_cmp(&left.effective_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.note.updated_at.cmp(&left.note.updated_at))
    });
    merged
}

fn memory_role_boost(value: &str) -> f64 {
    match value {
        "direct_thread_memory" => THREAD_MEMORY_BOOST,
        "direct_job_memory" => JOB_MEMORY_BOOST,
        _ => 0.0,
    }
}

fn memory_role_reason(value: &str) -> &'static str {
    match value {
        "direct_thread_memory" => "direct_thread_memory",
        "direct_job_memory" => "direct_job_memory",
        _ => "retrieved_memory",
    }
}

fn build_thread_notes_query(thread: &ThreadRecord, messages: &[MessageRecord]) -> Option<String> {
    let mut terms = Vec::new();
    if let Some(message) = messages.iter().rev().find(|message| message.role == "user") {
        terms.push(super::conversation::summarize_text(&message.content, 180));
    } else {
        terms.push(thread.title.clone());
    }

    let query = terms
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!query.is_empty()).then_some(query)
}

fn build_thread_notes_context(
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
) -> Option<String> {
    let mut terms = vec![thread.title.clone()];
    terms.extend(
        messages
            .iter()
            .rev()
            .take(3)
            .map(|message| super::conversation::summarize_text(&message.content, 120)),
    );
    terms.extend(
        jobs.iter()
            .take(2)
            .map(|job| format!("{} {}", job.title, job.repo_name)),
    );
    join_terms(terms)
}

fn build_job_notes_query(job: &JobRecord, summary: Option<&SummaryRecord>) -> Option<String> {
    let mut terms = vec![job.title.clone(), job.repo_name.clone()];
    if let Some(summary) = summary {
        terms.push(super::conversation::summarize_text(&summary.content, 180));
    }
    join_terms(terms)
}

fn build_job_notes_context(job: &JobRecord, summary: Option<&SummaryRecord>) -> Option<String> {
    let mut terms = vec![job.title.clone(), job.repo_name.clone()];
    if let Some(branch_name) = job.branch_name.as_deref() {
        terms.push(branch_name.to_string());
    }
    if let Some(summary) = summary {
        terms.push(super::conversation::summarize_text(&summary.content, 240));
    }
    join_terms(terms)
}

fn join_terms(terms: Vec<String>) -> Option<String> {
    let query = terms
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!query.is_empty()).then_some(query)
}

fn summarize_text(value: &str, limit: usize) -> String {
    let cleaned = value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    super::conversation::summarize_text(&cleaned, limit)
}

#[cfg(test)]
mod tests {
    use super::{build_thread_notes_context, merge_related_note_contexts};
    use crate::models::{JobRecord, MessageRecord, NoteRecord};
    use chrono::Utc;
    use serde_json::json;

    fn note(note_id: &str, relevance_score: f64, reasons: &[&str]) -> NoteRecord {
        NoteRecord {
            note_id: note_id.to_string(),
            title: format!("Note {note_id}"),
            slug: format!("note-{note_id}"),
            summary: "summary".to_string(),
            tags: Vec::new(),
            aliases: Vec::new(),
            note_type: "general".to_string(),
            source_kind: None,
            source_id: None,
            current_revision_id: format!("rev-{note_id}"),
            updated_at: Utc::now(),
            relevance_score,
            match_reasons: reasons.iter().map(|value| value.to_string()).collect(),
        }
    }

    fn message(role: &str, content: &str) -> MessageRecord {
        MessageRecord {
            id: "message-1".to_string(),
            thread_id: "thread-1".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            status: "conversation.user".to_string(),
            payload_json: json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn job(short_id: &str, title: &str, repo_name: &str) -> JobRecord {
        JobRecord {
            id: format!("job-{short_id}"),
            short_id: short_id.to_string(),
            correlation_id: "corr-1".to_string(),
            thread_id: "thread-1".to_string(),
            title: title.to_string(),
            status: "completed".to_string(),
            result: Some("success".to_string()),
            failure_class: None,
            repo_name: repo_name.to_string(),
            device_id: None,
            branch_name: Some("main".to_string()),
            base_branch: Some("main".to_string()),
            parent_job_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
        }
    }

    #[test]
    fn merging_prefers_direct_memory_and_accumulates_reasons() {
        let merged = merge_related_note_contexts(vec![
            super::NoteContextCandidate {
                note: note("note-1", 0.0, &[]),
                memory_role: "direct_thread_memory".to_string(),
                source_label: "Promoted from thread".to_string(),
            },
            super::NoteContextCandidate {
                note: note("note-1", 45.0, &["query_summary"]),
                memory_role: "retrieved_memory".to_string(),
                source_label: "Retrieved by note search".to_string(),
            },
        ]);

        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].memory_role, "direct_thread_memory");
        assert!(merged[0].note.relevance_score >= 1_000.0);
        assert!(
            merged[0]
                .note
                .match_reasons
                .contains(&"direct_thread_memory".to_string())
        );
        assert!(
            merged[0]
                .note
                .match_reasons
                .contains(&"query_summary".to_string())
        );
    }

    #[test]
    fn thread_context_includes_recent_messages_and_jobs() {
        let thread = crate::models::ThreadRecord {
            id: "thread-1".to_string(),
            title: "Notes retrieval polish".to_string(),
            status: "open".to_string(),
            current_summary_id: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        let context = build_thread_notes_context(
            &thread,
            &[
                message(
                    "user",
                    "Improve note ranking and better use promoted memory.",
                ),
                message(
                    "assistant",
                    "I can expand the assistant memory bundle next.",
                ),
            ],
            &[job("job-1", "Previous API work", "elowen-api")],
        )
        .unwrap();

        assert!(context.contains("Notes retrieval polish"));
        assert!(context.contains("Improve note ranking"));
        assert!(context.contains("Previous API work elowen-api"));
    }
}

//! Notes-service integration helpers.

use anyhow::anyhow;

use crate::{
    models::{JobRecord, NoteRecord, PromoteNoteRequest, SummaryRecord, ThreadRecord},
    state::AppState,
};

pub(crate) async fn load_related_thread_notes(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[crate::models::MessageRecord],
    jobs: &[JobRecord],
) -> Result<Vec<NoteRecord>, crate::error::AppError> {
    let mut promoted =
        search_notes(state, None, Some("thread"), Some(thread.id.as_str()), 8).await?;
    for job in jobs {
        let job_notes = search_notes(state, None, Some("job"), Some(job.id.as_str()), 4).await?;
        promoted = merge_note_sets(promoted, job_notes);
    }
    let query = build_thread_notes_query(thread, messages);
    let related = search_notes(state, query.as_deref(), None, None, 6).await?;
    Ok(merge_note_sets(promoted, related))
}

pub(crate) async fn load_related_job_notes(
    state: &AppState,
    job: &JobRecord,
    summary: Option<&SummaryRecord>,
) -> Result<Vec<NoteRecord>, crate::error::AppError> {
    let promoted = search_notes(state, None, Some("job"), Some(job.id.as_str()), 8).await?;
    let query = build_job_notes_query(job, summary);
    let related = search_notes(state, query.as_deref(), None, None, 6).await?;
    Ok(merge_note_sets(promoted, related))
}

pub(crate) async fn search_notes(
    state: &AppState,
    query: Option<&str>,
    source_kind: Option<&str>,
    source_id: Option<&str>,
    limit: usize,
) -> Result<Vec<NoteRecord>, crate::error::AppError> {
    let url = format!("{}/api/v1/notes/search", state.notes_url);
    let response = state
        .http
        .get(url)
        .query(&[
            ("q", query.unwrap_or("")),
            ("source_kind", source_kind.unwrap_or("")),
            ("source_id", source_id.unwrap_or("")),
            ("limit", &limit.to_string()),
        ])
        .send()
        .await
        .map_err(|error| crate::error::AppError::from(anyhow!(error)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(crate::error::AppError::from(anyhow!(
            "notes search failed with status {status}: {body}"
        )));
    }

    response
        .json::<Vec<NoteRecord>>()
        .await
        .map_err(|error| crate::error::AppError::from(anyhow!(error)))
}

pub(crate) async fn promote_note_to_service(
    state: &AppState,
    request: PromoteNoteRequest,
) -> Result<NoteRecord, crate::error::AppError> {
    let url = format!("{}/api/v1/notes/promotions", state.notes_url);
    let response = state
        .http
        .post(url)
        .json(&request)
        .send()
        .await
        .map_err(|error| crate::error::AppError::from(anyhow!(error)))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(crate::error::AppError::from(anyhow!(
            "note promotion failed with status {status}: {body}"
        )));
    }

    let detail = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| crate::error::AppError::from(anyhow!(error)))?;
    let note = detail.get("note").cloned().unwrap_or(detail);

    serde_json::from_value::<NoteRecord>(note)
        .map_err(|error| crate::error::AppError::from(anyhow!(error)))
}

fn merge_note_sets(primary: Vec<NoteRecord>, secondary: Vec<NoteRecord>) -> Vec<NoteRecord> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();

    for note in primary.into_iter().chain(secondary) {
        if seen.insert(note.note_id.clone()) {
            merged.push(note);
        }
    }

    merged
}

fn build_thread_notes_query(
    thread: &ThreadRecord,
    messages: &[crate::models::MessageRecord],
) -> Option<String> {
    let mut terms = vec![thread.title.clone()];
    if let Some(message) = messages.iter().rev().find(|message| message.role == "user") {
        terms.push(super::conversation::summarize_text(&message.content, 120));
    }

    let query = terms
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!query.is_empty()).then_some(query)
}

fn build_job_notes_query(job: &JobRecord, summary: Option<&SummaryRecord>) -> Option<String> {
    let mut terms = vec![job.title.clone(), job.repo_name.clone()];
    if let Some(summary) = summary {
        terms.push(super::conversation::summarize_text(&summary.content, 120));
    }

    let query = terms
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!query.is_empty()).then_some(query)
}

//! Thread/job note routes.

use axum::{
    Extension, Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde_json::json;

use crate::{
    db::jobs::{load_current_job_summary, load_job_record, load_thread_jobs},
    db::threads::{load_thread_messages, load_thread_record},
    error::AppError,
    formatting::sanitize_optional_string,
    models::{
        NoteAuthor, NoteRecord, NoteSourceReference, PromoteJobNoteRequest, PromoteNoteRequest,
    },
    routes::require_session_actor,
    services::{
        conversation::summarize_text,
        notes::{
            SearchNotesRequest, load_related_job_notes, load_related_thread_notes,
            promote_note_to_service, search_notes,
        },
        ui_events::{job_ui_event, publish_ui_event},
    },
    state::AppState,
};

pub(crate) async fn get_thread_notes(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<NoteRecord>>, AppError> {
    let thread = load_thread_record(&state.pool, &thread_id).await?;
    let messages = load_thread_messages(&state.pool, &thread_id).await?;
    let jobs = load_thread_jobs(&state.pool, &thread_id).await?;
    Ok(Json(
        load_related_thread_notes(&state, &thread, &messages, &jobs).await?,
    ))
}

pub(crate) async fn get_job_notes(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Vec<NoteRecord>>, AppError> {
    let job = load_job_record(&state.pool, &job_id).await?;
    let summary = load_current_job_summary(&state.pool, &job_id).await?;
    Ok(Json(
        load_related_job_notes(&state, &job, summary.as_ref()).await?,
    ))
}

pub(crate) async fn promote_job_note(
    State(state): State<AppState>,
    actor: Option<Extension<crate::auth::SessionActor>>,
    Path(job_id): Path<String>,
    Json(request): Json<PromoteJobNoteRequest>,
) -> Result<(StatusCode, Json<NoteRecord>), AppError> {
    let actor = require_session_actor(actor);
    let job = load_job_record(&state.pool, &job_id).await?;
    let summary = load_current_job_summary(&state.pool, &job_id).await?;
    let existing_note_id = search_notes(
        &state,
        SearchNotesRequest {
            source_kind: Some("job"),
            source_id: Some(job.id.as_str()),
            limit: 1,
            ..SearchNotesRequest::default()
        },
    )
    .await?
    .into_iter()
    .next()
    .map(|note| note.note_id);
    let default_body = summary
        .as_ref()
        .map(|record| record.content.clone())
        .unwrap_or_else(|| format!("# {}\n\nResult: {:?}\n", job.title, job.result));
    let body_markdown = sanitize_optional_string(request.body_markdown).unwrap_or(default_body);
    let mut source_references = vec![
        NoteSourceReference {
            source_kind: "job".to_string(),
            source_id: job.id.clone(),
            label: Some(format!("Job {}", job.short_id)),
        },
        NoteSourceReference {
            source_kind: "thread".to_string(),
            source_id: job.thread_id.clone(),
            label: Some("Owning thread".to_string()),
        },
    ];
    if let Some(summary) = summary.as_ref() {
        source_references.push(NoteSourceReference {
            source_kind: "summary".to_string(),
            source_id: summary.id.clone(),
            label: Some(format!("Job summary v{}", summary.version)),
        });
    }

    let promoted = promote_note_to_service(
        &state,
        PromoteNoteRequest {
            note_id: existing_note_id,
            source_kind: Some("job".to_string()),
            source_id: Some(job.id.clone()),
            title: request.title.or_else(|| Some(job.title.clone())),
            slug: None,
            summary: request.summary.or_else(|| {
                summary
                    .as_ref()
                    .map(|record| summarize_text(&record.content, 240))
            }),
            body_markdown,
            tags: request.tags,
            aliases: request.aliases,
            note_type: request.note_type,
            frontmatter: Some(json!({
                "job_id": job.id,
                "repo_name": job.repo_name,
                "branch_name": job.branch_name,
            })),
            authored_by: Some(NoteAuthor {
                actor_type: "operator".to_string(),
                actor_id: actor.username,
                display_name: Some(actor.display_name),
            }),
            source_references,
        },
    )
    .await?;

    publish_ui_event(
        &state,
        job_ui_event(&job.thread_id, &job.id, job.device_id.as_deref()),
    );

    Ok((StatusCode::CREATED, Json(promoted)))
}

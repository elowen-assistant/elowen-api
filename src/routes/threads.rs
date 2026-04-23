//! Thread, message, chat, and dispatch routes.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

use crate::{
    db::{
        jobs::load_thread_jobs,
        threads::{
            create_thread as create_thread_record, ensure_thread_exists, insert_thread_message,
            insert_thread_message_with_status, insert_thread_message_with_status_and_payload,
            list_threads as load_threads, load_thread_message, load_thread_messages,
            load_thread_record, touch_thread,
        },
    },
    error::AppError,
    formatting::{derive_job_title_from_message, execution_intent_note, sanitize_optional_string},
    models::{
        ChatDispatchResponse, ChatReplyResponse, CreateChatDispatchRequest, CreateJobRequest,
        CreateMessageRequest, CreateThreadChatRequest, CreateThreadRequest,
        DispatchThreadMessageRequest, JobTargetKind, MessageDispatchResponse, ThreadDetail,
        ThreadSummary,
    },
    services::{
        conversation::{
            build_message_payload, generate_conversational_reply, infer_execution_intent,
            maybe_annotate_draft_reply, maybe_build_execution_draft, message_execution_draft,
            summarize_text,
        },
        jobs::{create_job_record, load_job_detail_from_record},
        notes::{load_related_thread_note_context, load_related_thread_notes},
        ui_events::{job_ui_event, publish_ui_event, thread_ui_event},
    },
    state::AppState,
};

pub(crate) async fn list_threads(
    State(state): State<AppState>,
) -> Result<Json<Vec<ThreadSummary>>, AppError> {
    Ok(Json(load_threads(&state.pool).await?))
}

pub(crate) async fn get_thread(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<ThreadDetail>, AppError> {
    let thread = load_thread_record(&state.pool, &thread_id).await?;
    let messages = load_thread_messages(&state.pool, &thread_id).await?;
    let jobs = load_thread_jobs(&state.pool, &thread_id).await?;
    let related_notes = load_related_thread_notes(&state, &thread, &messages, &jobs).await?;

    Ok(Json(ThreadDetail {
        thread,
        messages,
        jobs,
        related_notes,
    }))
}

pub(crate) async fn create_thread(
    State(state): State<AppState>,
    Json(request): Json<CreateThreadRequest>,
) -> Result<(StatusCode, Json<ThreadDetail>), AppError> {
    let title = request.title.trim();
    if title.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "thread title is required"
        )));
    }

    let thread = create_thread_record(&state.pool, title).await?;
    publish_ui_event(&state, thread_ui_event(&thread.id));

    Ok((
        StatusCode::CREATED,
        Json(ThreadDetail {
            thread,
            messages: Vec::new(),
            jobs: Vec::new(),
            related_notes: Vec::new(),
        }),
    ))
}

pub(crate) async fn create_message(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CreateMessageRequest>,
) -> Result<(StatusCode, Json<crate::models::MessageRecord>), AppError> {
    let content = request.content.trim();
    if content.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "message content is required"
        )));
    }

    if !matches!(request.role.as_str(), "user" | "assistant" | "system") {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "message role must be one of: user, assistant, system"
        )));
    }

    ensure_thread_exists(&state.pool, &thread_id).await?;
    let message = insert_thread_message(&state.pool, &thread_id, &request.role, content).await?;

    touch_thread(&state.pool, &thread_id).await?;
    publish_ui_event(&state, thread_ui_event(&thread_id));

    Ok((StatusCode::CREATED, Json(message)))
}

pub(crate) async fn create_thread_chat(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CreateThreadChatRequest>,
) -> Result<(StatusCode, Json<ChatReplyResponse>), AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;

    let content = request.content.trim();
    if content.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "message content is required"
        )));
    }

    let user_message = insert_thread_message(&state.pool, &thread_id, "user", content).await?;
    let thread = load_thread_record(&state.pool, &thread_id).await?;
    let messages = load_thread_messages(&state.pool, &thread_id).await?;
    let jobs = load_thread_jobs(&state.pool, &thread_id).await?;
    let related_note_context =
        load_related_thread_note_context(&state, &thread, &messages, &jobs).await?;
    let execution_draft = maybe_build_execution_draft(&messages, &jobs);
    let assistant_reply = generate_conversational_reply(
        &state,
        &thread,
        &messages,
        &jobs,
        &related_note_context,
        execution_draft.as_ref(),
    )
    .await?;
    let assistant_reply = maybe_annotate_draft_reply(assistant_reply, execution_draft.as_ref());
    let assistant_message = insert_thread_message_with_status_and_payload(
        &state.pool,
        &thread_id,
        "assistant",
        &assistant_reply,
        "conversation.reply",
        build_message_payload(execution_draft.as_ref()),
    )
    .await?;

    touch_thread(&state.pool, &thread_id).await?;
    publish_ui_event(&state, thread_ui_event(&thread_id));

    Ok((
        StatusCode::CREATED,
        Json(ChatReplyResponse {
            user_message,
            assistant_message,
        }),
    ))
}

pub(crate) async fn dispatch_thread_message(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<DispatchThreadMessageRequest>,
) -> Result<(StatusCode, Json<MessageDispatchResponse>), AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;

    let source_message =
        load_thread_message(&state.pool, &thread_id, request.source_message_id.trim()).await?;
    if !matches!(source_message.role.as_str(), "user" | "assistant") {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "only user or assistant messages can be dispatched"
        )));
    }

    let embedded_draft = message_execution_draft(&source_message);
    let prompt = sanitize_optional_string(request.prompt.clone())
        .or_else(|| embedded_draft.as_ref().map(|draft| draft.prompt.clone()))
        .unwrap_or_else(|| source_message.content.trim().to_string());
    if prompt.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "source message content is required"
        )));
    }

    let title = sanitize_optional_string(request.title)
        .or_else(|| embedded_draft.as_ref().map(|draft| draft.title.clone()))
        .unwrap_or_else(|| derive_job_title_from_message(&prompt));
    let target_kind = if matches!(request.target_kind, JobTargetKind::Repository)
        && request.target_name.is_none()
    {
        embedded_draft
            .as_ref()
            .map(|draft| draft.target_kind.clone())
            .unwrap_or(JobTargetKind::Repository)
    } else {
        request.target_kind.clone()
    };
    let target_name = sanitize_optional_string(request.target_name.clone()).or_else(|| {
        embedded_draft
            .as_ref()
            .map(|draft| draft.target_name.clone())
    });
    let base_branch = sanitize_optional_string(request.base_branch).or_else(|| {
        embedded_draft
            .as_ref()
            .and_then(|draft| draft.base_branch.clone())
    });
    let execution_intent = request
        .execution_intent
        .or_else(|| {
            embedded_draft
                .as_ref()
                .map(|draft| draft.execution_intent.clone())
        })
        .unwrap_or_else(|| infer_execution_intent(&prompt));

    let job = create_job_record(
        &state,
        &thread_id,
        &CreateJobRequest {
            title,
            target_kind: target_kind.clone(),
            target_name: target_name.clone(),
            base_branch,
            prompt: prompt.clone(),
            device_id: request.device_id,
            execution_intent: Some(execution_intent.clone()),
        },
    )
    .await?;
    let acknowledgement = insert_thread_message_with_status(
        &state.pool,
        &thread_id,
        "system",
        &format!(
            "Escalated {} message `{}` into job `{}` for {} `{}` in {} mode.\nRequest summary: {}",
            source_message.role,
            source_message.id,
            job.short_id,
            match job.target_kind_enum() {
                JobTargetKind::Repository => "repository",
                JobTargetKind::Capability => "capability",
            },
            job.target_name(),
            execution_intent_note(&execution_intent),
            summarize_text(&prompt, 160)
        ),
        "workflow.handoff.created",
    )
    .await?;

    touch_thread(&state.pool, &thread_id).await?;
    publish_ui_event(
        &state,
        job_ui_event(&thread_id, &job.id, job.device_id.as_deref()),
    );

    Ok((
        StatusCode::CREATED,
        Json(MessageDispatchResponse {
            source_message,
            acknowledgement,
            job,
        }),
    ))
}

pub(crate) async fn create_chat_dispatch(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CreateChatDispatchRequest>,
) -> Result<(StatusCode, Json<ChatDispatchResponse>), AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;

    let content = request.content.trim();
    if content.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "message content is required"
        )));
    }

    let title = sanitize_optional_string(request.title)
        .unwrap_or_else(|| derive_job_title_from_message(content));
    let execution_intent = request
        .execution_intent
        .unwrap_or_else(|| infer_execution_intent(content));

    let message = insert_thread_message(&state.pool, &thread_id, "user", content).await?;
    let job = create_job_record(
        &state,
        &thread_id,
        &CreateJobRequest {
            title,
            target_kind: request.target_kind,
            target_name: sanitize_optional_string(request.target_name),
            base_branch: sanitize_optional_string(request.base_branch),
            prompt: content.to_string(),
            device_id: request.device_id,
            execution_intent: Some(execution_intent.clone()),
        },
    )
    .await?;
    let acknowledgement = insert_thread_message_with_status(
        &state.pool,
        &thread_id,
        "system",
        &format!(
            "Created job `{}` with status `{}` on device `{}` for {} `{}` in {} mode.",
            job.short_id,
            job.status,
            job.device_id
                .clone()
                .unwrap_or_else(|| "unassigned".to_string()),
            match job.target_kind_enum() {
                JobTargetKind::Repository => "repository",
                JobTargetKind::Capability => "capability",
            },
            job.target_name(),
            execution_intent_note(&execution_intent),
        ),
        "workflow.dispatch.created",
    )
    .await?;

    touch_thread(&state.pool, &thread_id).await?;
    publish_ui_event(
        &state,
        job_ui_event(&thread_id, &job.id, job.device_id.as_deref()),
    );

    Ok((
        StatusCode::CREATED,
        Json(ChatDispatchResponse {
            message,
            acknowledgement,
            job,
        }),
    ))
}

pub(crate) async fn list_thread_jobs(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<crate::models::JobRecord>>, AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;
    let jobs = load_thread_jobs(&state.pool, &thread_id).await?;
    Ok(Json(jobs))
}

pub(crate) async fn create_job(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<crate::models::JobDetail>), AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;
    let job = create_job_record(&state, &thread_id, &request).await?;
    touch_thread(&state.pool, &thread_id).await?;
    publish_ui_event(
        &state,
        job_ui_event(&thread_id, &job.id, job.device_id.as_deref()),
    );

    let detail = load_job_detail_from_record(&state, job).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

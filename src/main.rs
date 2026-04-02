use anyhow::Context;
use async_nats::Client as NatsClient;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{FromRow, PgPool, postgres::PgPoolOptions, types::Json as SqlxJson};
use std::{collections::HashSet, env, net::SocketAddr, time::Duration};
use tokio::time::timeout;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};
use ulid::Ulid;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    nats: NatsClient,
    http: HttpClient,
    notes_url: String,
    assistant: AssistantRuntime,
}

#[derive(Clone)]
struct AssistantRuntime {
    api_key: Option<String>,
    model: String,
    base_url: String,
}

#[derive(Debug, Serialize, FromRow)]
struct ThreadSummary {
    id: String,
    title: String,
    status: String,
    current_summary_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    message_count: i64,
}

#[derive(Debug, Serialize, FromRow)]
struct ThreadRecord {
    id: String,
    title: String,
    status: String,
    current_summary_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
struct MessageRecord {
    id: String,
    thread_id: String,
    role: String,
    content: String,
    status: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Clone, FromRow)]
struct JobRecord {
    id: String,
    short_id: String,
    correlation_id: String,
    thread_id: String,
    title: String,
    status: String,
    result: Option<String>,
    failure_class: Option<String>,
    repo_name: String,
    device_id: Option<String>,
    branch_name: Option<String>,
    base_branch: Option<String>,
    parent_job_id: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, FromRow)]
struct JobEventRow {
    id: String,
    job_id: String,
    correlation_id: String,
    event_type: String,
    payload_json: SqlxJson<Value>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct JobEventRecord {
    id: String,
    job_id: String,
    correlation_id: String,
    event_type: String,
    payload_json: Value,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct ThreadDetail {
    #[serde(flatten)]
    thread: ThreadRecord,
    messages: Vec<MessageRecord>,
    jobs: Vec<JobRecord>,
    related_notes: Vec<NoteRecord>,
}

#[derive(Debug, Serialize)]
struct JobDetail {
    #[serde(flatten)]
    job: JobRecord,
    execution_report_json: Value,
    summary: Option<SummaryRecord>,
    approvals: Vec<ApprovalRecord>,
    related_notes: Vec<NoteRecord>,
    events: Vec<JobEventRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct NoteRecord {
    note_id: String,
    title: String,
    slug: String,
    summary: String,
    tags: Vec<String>,
    aliases: Vec<String>,
    note_type: String,
    source_kind: Option<String>,
    source_id: Option<String>,
    current_revision_id: String,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
struct SummaryRecord {
    id: String,
    scope: String,
    source_id: String,
    version: i32,
    content: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
struct ApprovalRecord {
    id: String,
    thread_id: String,
    job_id: String,
    action_type: String,
    status: String,
    summary: String,
    resolved_by: Option<String>,
    resolution_reason: Option<String>,
    created_at: DateTime<Utc>,
    resolved_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
struct JobStateRow {
    current_summary_id: Option<String>,
    execution_report_json: SqlxJson<Value>,
}

#[derive(Debug, Deserialize)]
struct CreateThreadRequest {
    title: String,
}

#[derive(Debug, Deserialize)]
struct CreateMessageRequest {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct CreateChatDispatchRequest {
    content: String,
    title: Option<String>,
    repo_name: String,
    base_branch: Option<String>,
    device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateThreadChatRequest {
    content: String,
}

#[derive(Debug, Deserialize)]
struct CreateJobRequest {
    title: String,
    repo_name: String,
    base_branch: Option<String>,
    request_text: String,
    device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResolveApprovalRequest {
    status: String,
    resolved_by: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PromoteJobNoteRequest {
    title: Option<String>,
    summary: Option<String>,
    body_markdown: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    aliases: Vec<String>,
    note_type: Option<String>,
}

#[derive(Debug, Serialize)]
struct PromoteNoteRequest {
    note_id: Option<String>,
    source_kind: Option<String>,
    source_id: Option<String>,
    title: Option<String>,
    slug: Option<String>,
    summary: Option<String>,
    body_markdown: String,
    tags: Vec<String>,
    aliases: Vec<String>,
    note_type: Option<String>,
    frontmatter: Option<Value>,
    authored_by: Option<NoteAuthor>,
    source_references: Vec<NoteSourceReference>,
}

#[derive(Debug, Serialize)]
struct NoteAuthor {
    actor_type: String,
    actor_id: String,
    display_name: Option<String>,
}

#[derive(Debug, Serialize)]
struct NoteSourceReference {
    source_kind: String,
    source_id: String,
    label: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatDispatchResponse {
    message: MessageRecord,
    acknowledgement: MessageRecord,
    job: JobRecord,
}

#[derive(Debug, Serialize)]
struct ChatReplyResponse {
    user_message: MessageRecord,
    assistant_message: MessageRecord,
}

#[derive(Debug, Deserialize)]
struct RegisterDeviceRequest {
    name: String,
    primary_flag: bool,
    #[serde(default)]
    allowed_repos: Vec<String>,
    #[serde(default)]
    capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeDeviceRequest {
    job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AvailabilitySnapshot {
    probe_id: String,
    job_id: Option<String>,
    device_id: String,
    available: bool,
    reason: String,
    responded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct DeviceMetadata {
    #[serde(default)]
    allowed_repos: Vec<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    registered_at: Option<DateTime<Utc>>,
    last_seen_at: Option<DateTime<Utc>>,
    last_probe: Option<AvailabilitySnapshot>,
}

#[derive(Debug, FromRow)]
struct DeviceRow {
    id: String,
    name: String,
    primary_flag: bool,
    metadata: SqlxJson<DeviceMetadata>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct DeviceRecord {
    id: String,
    name: String,
    primary_flag: bool,
    allowed_repos: Vec<String>,
    capabilities: Vec<String>,
    registered_at: DateTime<Utc>,
    last_seen_at: DateTime<Utc>,
    last_probe: Option<AvailabilitySnapshot>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct AvailabilityProbeMessage {
    probe_id: String,
    job_id: Option<String>,
    device_id: String,
    sent_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JobDispatchMessage {
    job_id: String,
    short_id: String,
    correlation_id: String,
    thread_id: String,
    title: String,
    device_id: String,
    repo_name: String,
    base_branch: String,
    branch_name: String,
    request_text: String,
    dispatched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JobLifecycleEvent {
    job_id: String,
    correlation_id: String,
    device_id: String,
    event_type: String,
    status: Option<String>,
    result: Option<String>,
    failure_class: Option<String>,
    worktree_path: Option<String>,
    detail: Option<String>,
    payload_json: Option<Value>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug)]
struct AppError {
    status: StatusCode,
    error: anyhow::Error,
}

impl AppError {
    fn bad_request(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: message.into(),
        }
    }

    fn not_found(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: message.into(),
        }
    }

    fn conflict(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: message.into(),
        }
    }

    fn gateway_timeout(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            error: message.into(),
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.error.to_string(),
            }),
        )
            .into_response()
    }
}

fn init_tracing(service_name: &'static str) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let log_format = env::var("ELOWEN_LOG_FORMAT").unwrap_or_else(|_| "plain".to_string());
    let builder = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(true);

    if log_format.eq_ignore_ascii_case("json") {
        builder
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .flatten_event(true)
            .with_ansi(false)
            .init();
    } else {
        builder.with_ansi(true).init();
    }

    info!(service = service_name, log_format = %log_format, "tracing initialized");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing("elowen-api");

    let database_url = env::var("ELOWEN_DATABASE_URL").context("missing ELOWEN_DATABASE_URL")?;
    let nats_url = env::var("ELOWEN_NATS_URL").context("missing ELOWEN_NATS_URL")?;
    let notes_url = env::var("ELOWEN_NOTES_URL")
        .unwrap_or_else(|_| "http://elowen-notes:8080".to_string())
        .trim_end_matches('/')
        .to_string();
    let assistant_api_key = env::var("OPENAI_API_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let assistant_model = env::var("ELOWEN_ASSISTANT_MODEL")
        .unwrap_or_else(|_| "gpt-4.1-mini".to_string())
        .trim()
        .to_string();
    let assistant_base_url = env::var("ELOWEN_ASSISTANT_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
        .trim_end_matches('/')
        .to_string();

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("failed to connect to Postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("failed to run API migrations")?;

    let nats = async_nats::connect(&nats_url)
        .await
        .context("failed to connect to NATS")?;
    let http = HttpClient::builder()
        .build()
        .context("failed to build HTTP client")?;

    let event_pool = pool.clone();
    let event_nats = nats.clone();
    tokio::spawn(async move {
        if let Err(error) = consume_job_lifecycle_events(event_pool, event_nats).await {
            warn!(error = %error, "job lifecycle consumer stopped");
        }
    });

    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let address = SocketAddr::from(([0, 0, 0, 0], port));

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/threads", get(list_threads).post(create_thread))
        .route("/api/v1/threads/{thread_id}", get(get_thread))
        .route("/api/v1/threads/{thread_id}/messages", post(create_message))
        .route("/api/v1/threads/{thread_id}/chat", post(create_thread_chat))
        .route(
            "/api/v1/threads/{thread_id}/chat-dispatch",
            post(create_chat_dispatch),
        )
        .route("/api/v1/threads/{thread_id}/notes", get(get_thread_notes))
        .route(
            "/api/v1/threads/{thread_id}/jobs",
            get(list_thread_jobs).post(create_job),
        )
        .route("/api/v1/jobs", get(list_jobs))
        .route("/api/v1/jobs/{job_id}", get(get_job))
        .route("/api/v1/jobs/{job_id}/notes", get(get_job_notes))
        .route(
            "/api/v1/jobs/{job_id}/notes/promote",
            post(promote_job_note),
        )
        .route(
            "/api/v1/approvals/{approval_id}/resolve",
            post(resolve_approval),
        )
        .route("/api/v1/devices", get(list_devices))
        .route(
            "/api/v1/devices/{device_id}",
            get(get_device).put(register_device),
        )
        .route(
            "/api/v1/devices/{device_id}/availability-probe",
            post(probe_device),
        )
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([
                    axum::http::Method::GET,
                    axum::http::Method::POST,
                    axum::http::Method::PUT,
                ])
                .allow_headers(Any),
        )
        .with_state(AppState {
            pool,
            nats,
            http,
            notes_url,
            assistant: AssistantRuntime {
                api_key: assistant_api_key,
                model: assistant_model,
                base_url: assistant_base_url,
            },
        });

    info!(%address, "starting elowen-api");

    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn list_threads(State(state): State<AppState>) -> Result<Json<Vec<ThreadSummary>>, AppError> {
    let threads = sqlx::query_as::<_, ThreadSummary>(
        r#"
        select
            t.id,
            t.title,
            t.status,
            t.current_summary_id,
            t.created_at,
            t.updated_at,
            coalesce(count(m.id), 0)::bigint as message_count
        from threads t
        left join messages m on m.thread_id = t.id
        group by t.id
        order by t.updated_at desc, t.created_at desc
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(threads))
}

async fn get_thread(
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

async fn create_thread(
    State(state): State<AppState>,
    Json(request): Json<CreateThreadRequest>,
) -> Result<(StatusCode, Json<ThreadDetail>), AppError> {
    let title = request.title.trim();
    if title.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "thread title is required"
        )));
    }

    let thread_id = Ulid::new().to_string();
    let thread = sqlx::query_as::<_, ThreadRecord>(
        r#"
        insert into threads (id, title, status)
        values ($1, $2, 'open')
        returning id, title, status, current_summary_id, created_at, updated_at
        "#,
    )
    .bind(&thread_id)
    .bind(title)
    .fetch_one(&state.pool)
    .await?;

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

async fn create_message(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CreateMessageRequest>,
) -> Result<(StatusCode, Json<MessageRecord>), AppError> {
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

    Ok((StatusCode::CREATED, Json(message)))
}

async fn create_thread_chat(
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
    let related_notes = load_related_thread_notes(&state, &thread, &messages, &jobs).await?;
    let assistant_reply =
        generate_conversational_reply(&state, &thread, &messages, &jobs, &related_notes).await?;
    let assistant_message =
        insert_thread_message(&state.pool, &thread_id, "assistant", &assistant_reply).await?;

    touch_thread(&state.pool, &thread_id).await?;

    Ok((
        StatusCode::CREATED,
        Json(ChatReplyResponse {
            user_message,
            assistant_message,
        }),
    ))
}

async fn create_chat_dispatch(
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

    let repo_name = request.repo_name.trim();
    if repo_name.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "repo name is required"
        )));
    }

    let title = sanitize_optional_string(request.title)
        .unwrap_or_else(|| derive_job_title_from_message(content));
    let base_branch =
        sanitize_optional_string(request.base_branch).unwrap_or_else(|| "main".to_string());

    let message = insert_thread_message(&state.pool, &thread_id, "user", content).await?;
    let job = create_job_record(
        &state,
        &thread_id,
        &CreateJobRequest {
            title,
            repo_name: repo_name.to_string(),
            base_branch: Some(base_branch),
            request_text: content.to_string(),
            device_id: request.device_id,
        },
    )
    .await?;
    let acknowledgement = insert_thread_message(
        &state.pool,
        &thread_id,
        "system",
        &format!(
            "Created job `{}` with status `{}` on device `{}` for repo `{}`.",
            job.short_id,
            job.status,
            job.device_id
                .clone()
                .unwrap_or_else(|| "unassigned".to_string()),
            job.repo_name
        ),
    )
    .await?;

    touch_thread(&state.pool, &thread_id).await?;

    Ok((
        StatusCode::CREATED,
        Json(ChatDispatchResponse {
            message,
            acknowledgement,
            job,
        }),
    ))
}

async fn list_thread_jobs(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
) -> Result<Json<Vec<JobRecord>>, AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;
    let jobs = load_thread_jobs(&state.pool, &thread_id).await?;
    Ok(Json(jobs))
}

async fn create_job(
    State(state): State<AppState>,
    Path(thread_id): Path<String>,
    Json(request): Json<CreateJobRequest>,
) -> Result<(StatusCode, Json<JobDetail>), AppError> {
    ensure_thread_exists(&state.pool, &thread_id).await?;
    let job = create_job_record(&state, &thread_id, &request).await?;
    touch_thread(&state.pool, &thread_id).await?;

    let detail = load_job_detail(&state, &job.id).await?;
    Ok((StatusCode::CREATED, Json(detail)))
}

async fn create_job_record(
    state: &AppState,
    thread_id: &str,
    request: &CreateJobRequest,
) -> Result<JobRecord, AppError> {
    let title = request.title.trim();
    let repo_name = request.repo_name.trim();
    let request_text = request.request_text.trim();
    let base_branch =
        sanitize_optional_string(request.base_branch.clone()).unwrap_or_else(|| "main".to_string());

    if title.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "job title is required"
        )));
    }

    if repo_name.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "repo name is required"
        )));
    }

    if request_text.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "job request text is required"
        )));
    }

    let target_device =
        select_dispatch_device(&state.pool, request.device_id.as_deref(), repo_name).await?;
    let target_device_id = target_device.id.clone();
    let job_id = Ulid::new().to_string();
    let correlation_id = Ulid::new().to_string();
    let short_id = job_id
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let branch_name = format!("codex/{}-{}", short_id, slugify(title));
    info!(
        job_id = %job_id,
        correlation_id = %correlation_id,
        thread_id = %thread_id,
        repo_name = %repo_name,
        device_id = %target_device_id,
        "creating job"
    );

    sqlx::query_as::<_, JobRecord>(
        r#"
        insert into jobs (
            id,
            short_id,
            correlation_id,
            title,
            thread_id,
            status,
            repo_name,
            device_id,
            branch_name,
            base_branch
        )
        values ($1, $2, $3, $4, $5, 'probing', $6, $7, $8, $9)
        returning
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            status,
            result,
            failure_class,
            repo_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        "#,
    )
    .bind(&job_id)
    .bind(&short_id)
    .bind(&correlation_id)
    .bind(title)
    .bind(&thread_id)
    .bind(repo_name)
    .bind(&target_device_id)
    .bind(&branch_name)
    .bind(&base_branch)
    .fetch_one(&state.pool)
    .await?;

    insert_job_event(
        &state.pool,
        &job_id,
        &correlation_id,
        "job.created",
        json!({
            "correlation_id": correlation_id.clone(),
            "request_text": request_text,
            "repo_name": repo_name,
            "device_id": target_device_id,
            "base_branch": base_branch.clone(),
            "branch_name": branch_name.clone(),
        }),
    )
    .await?;

    let availability =
        probe_device_via_nats(&state.nats, &target_device.id, Some(job_id.clone())).await?;

    insert_job_event(
        &state.pool,
        &job_id,
        &correlation_id,
        "job.probe_result",
        json!({
            "correlation_id": correlation_id.clone(),
            "available": availability.available,
            "reason": availability.reason,
            "probe_id": availability.probe_id,
            "responded_at": availability.responded_at,
        }),
    )
    .await?;

    if !availability.available {
        update_job_state(&state.pool, &job_id, "pending", None, None, None).await?;
        return load_job_record(&state.pool, &job_id).await;
    }

    let dispatch = JobDispatchMessage {
        job_id: job_id.clone(),
        short_id,
        correlation_id: correlation_id.clone(),
        thread_id: thread_id.to_string(),
        title: title.to_string(),
        device_id: target_device.id.clone(),
        repo_name: repo_name.to_string(),
        base_branch: base_branch.clone(),
        branch_name: branch_name.clone(),
        request_text: request_text.to_string(),
        dispatched_at: Utc::now(),
    };
    let dispatch_subject = format!("elowen.jobs.dispatch.{}", target_device.id);
    let dispatch_payload =
        serde_json::to_vec(&dispatch).context("failed to serialize job dispatch")?;

    if let Err(error) = state
        .nats
        .publish(dispatch_subject.clone(), dispatch_payload.into())
        .await
    {
        update_job_state(
            &state.pool,
            &job_id,
            "failed",
            Some("failure".to_string()),
            Some("infrastructure".to_string()),
            None,
        )
        .await?;
        insert_job_event(
            &state.pool,
            &job_id,
            &correlation_id,
            "job.dispatch_failed",
            json!({
                "correlation_id": correlation_id.clone(),
                "subject": dispatch_subject,
                "error": error.to_string(),
            }),
        )
        .await?;
        return Err(AppError::from(anyhow::anyhow!(
            "failed to publish job dispatch"
        )));
    }

    update_job_state(&state.pool, &job_id, "dispatched", None, None, None).await?;
    insert_job_event(
        &state.pool,
        &job_id,
        &correlation_id,
        "job.dispatched",
        json!({
            "correlation_id": correlation_id.clone(),
            "subject": dispatch_subject,
            "device_id": target_device.id.clone(),
            "repo_name": repo_name,
            "base_branch": base_branch.clone(),
            "branch_name": branch_name.clone(),
        }),
    )
    .await?;
    load_job_record(&state.pool, &job_id).await
}

async fn list_jobs(State(state): State<AppState>) -> Result<Json<Vec<JobRecord>>, AppError> {
    let jobs = sqlx::query_as::<_, JobRecord>(
        r#"
        select
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            status,
            result,
            failure_class,
            repo_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        from jobs
        order by created_at desc, id desc
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(jobs))
}

async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<JobDetail>, AppError> {
    Ok(Json(load_job_detail(&state, &job_id).await?))
}

async fn get_thread_notes(
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

async fn get_job_notes(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<Vec<NoteRecord>>, AppError> {
    let job = load_job_record(&state.pool, &job_id).await?;
    let summary = load_current_job_summary(&state.pool, &job_id).await?;
    Ok(Json(
        load_related_job_notes(&state, &job, summary.as_ref()).await?,
    ))
}

async fn promote_job_note(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
    Json(request): Json<PromoteJobNoteRequest>,
) -> Result<(StatusCode, Json<NoteRecord>), AppError> {
    let job = load_job_record(&state.pool, &job_id).await?;
    let summary = load_current_job_summary(&state.pool, &job_id).await?;
    let existing_note_id = search_notes(&state, None, Some("job"), Some(job.id.as_str()), 1)
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
                actor_type: "system".to_string(),
                actor_id: "elowen-api".to_string(),
                display_name: Some("Elowen API".to_string()),
            }),
            source_references,
        },
    )
    .await?;

    Ok((StatusCode::CREATED, Json(promoted)))
}

async fn resolve_approval(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(request): Json<ResolveApprovalRequest>,
) -> Result<Json<ApprovalRecord>, AppError> {
    let status = match request.status.trim().to_ascii_lowercase().as_str() {
        "approved" => "approved",
        "rejected" => "rejected",
        _ => {
            return Err(AppError::bad_request(anyhow::anyhow!(
                "approval status must be `approved` or `rejected`"
            )));
        }
    };
    let resolved_by = sanitize_optional_string(request.resolved_by);
    let reason = sanitize_optional_string(request.reason);

    let approval = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        update approvals
        set status = $2,
            resolved_by = $3,
            resolution_reason = $4,
            resolved_at = now(),
            updated_at = now()
        where id = $1
          and status = 'pending'
        returning
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        "#,
    )
    .bind(&approval_id)
    .bind(status)
    .bind(resolved_by.clone())
    .bind(reason.clone())
    .fetch_optional(&state.pool)
    .await?;

    let approval = match approval {
        Some(approval) => approval,
        None => {
            let existing = sqlx::query_scalar::<_, i64>(
                "select count(*)::bigint from approvals where id = $1",
            )
            .bind(&approval_id)
            .fetch_one(&state.pool)
            .await?;

            if existing == 0 {
                return Err(AppError::not_found(anyhow::anyhow!("approval not found")));
            }

            return Err(AppError::conflict(anyhow::anyhow!(
                "approval has already been resolved"
            )));
        }
    };

    let correlation_id = load_job_correlation_id(&state.pool, &approval.job_id).await?;
    insert_job_event(
        &state.pool,
        &approval.job_id,
        &correlation_id,
        &format!("approval.{status}"),
        json!({
            "correlation_id": correlation_id.clone(),
            "approval_id": approval.id,
            "action_type": approval.action_type,
            "resolved_by": approval.resolved_by,
            "reason": approval.resolution_reason,
        }),
    )
    .await?;

    update_job_status_only(&state.pool, &approval.job_id, "completed").await?;
    touch_thread_for_job(&state.pool, &approval.job_id).await?;

    Ok(Json(approval))
}

async fn list_devices(State(state): State<AppState>) -> Result<Json<Vec<DeviceRecord>>, AppError> {
    let devices = sqlx::query_as::<_, DeviceRow>(
        r#"
        select id, name, primary_flag, metadata, created_at, updated_at
        from devices
        order by primary_flag desc, updated_at desc, name asc
        "#,
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(
        devices.into_iter().map(device_record_from_row).collect(),
    ))
}

async fn get_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceRecord>, AppError> {
    let device = load_device_row(&state.pool, &device_id).await?;
    Ok(Json(device_record_from_row(device)))
}

async fn register_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(request): Json<RegisterDeviceRequest>,
) -> Result<(StatusCode, Json<DeviceRecord>), AppError> {
    let sanitized_device_id = device_id.trim().to_string();
    if sanitized_device_id.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "device id is required"
        )));
    }

    let name = request.name.trim();
    if name.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "device name is required"
        )));
    }

    let existing = load_device_row_optional(&state.pool, &sanitized_device_id).await?;
    let now = Utc::now();

    let metadata = DeviceMetadata {
        allowed_repos: sanitize_string_list(request.allowed_repos),
        capabilities: sanitize_string_list(request.capabilities),
        registered_at: existing
            .as_ref()
            .and_then(|row| row.metadata.0.registered_at)
            .or(Some(now)),
        last_seen_at: Some(now),
        last_probe: existing
            .as_ref()
            .and_then(|row| row.metadata.0.last_probe.clone()),
    };

    let device = upsert_device_row(
        &state.pool,
        &sanitized_device_id,
        name,
        request.primary_flag,
        metadata,
    )
    .await?;

    let status = if existing.is_some() {
        StatusCode::OK
    } else {
        StatusCode::CREATED
    };

    Ok((status, Json(device_record_from_row(device))))
}

async fn probe_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(request): Json<ProbeDeviceRequest>,
) -> Result<Json<AvailabilitySnapshot>, AppError> {
    let device = load_device_row(&state.pool, &device_id).await?;

    let probe = AvailabilityProbeMessage {
        probe_id: Ulid::new().to_string(),
        job_id: sanitize_optional_string(request.job_id),
        device_id: device.id.clone(),
        sent_at: Utc::now(),
    };
    let subject = format!("elowen.devices.availability.probe.{}", device.id);
    let payload = serde_json::to_vec(&probe).context("failed to serialize probe")?;

    let message = timeout(
        Duration::from_secs(5),
        state.nats.request(subject, payload.into()),
    )
    .await
    .map_err(|_| AppError::gateway_timeout(anyhow::anyhow!("device probe timed out")))?
    .context("device probe request failed")?;

    let response: AvailabilitySnapshot = serde_json::from_slice(&message.payload)
        .context("failed to decode device probe response")?;

    if response.device_id != device.id || response.probe_id != probe.probe_id {
        return Err(AppError::from(anyhow::anyhow!(
            "device probe response did not match request"
        )));
    }

    let mut metadata = device.metadata.0.clone();
    metadata.last_seen_at = Some(response.responded_at);
    metadata.last_probe = Some(response.clone());

    upsert_device_row(
        &state.pool,
        &device.id,
        &device.name,
        device.primary_flag,
        metadata,
    )
    .await?;

    Ok(Json(response))
}

async fn probe_device_via_nats(
    nats: &NatsClient,
    device_id: &str,
    job_id: Option<String>,
) -> Result<AvailabilitySnapshot, AppError> {
    let probe = AvailabilityProbeMessage {
        probe_id: Ulid::new().to_string(),
        job_id,
        device_id: device_id.to_string(),
        sent_at: Utc::now(),
    };
    let subject = format!("elowen.devices.availability.probe.{device_id}");
    let payload = serde_json::to_vec(&probe).context("failed to serialize probe")?;

    let message = timeout(
        Duration::from_secs(5),
        nats.request(subject, payload.into()),
    )
    .await
    .map_err(|_| AppError::gateway_timeout(anyhow::anyhow!("device probe timed out")))?
    .context("device probe request failed")?;

    let response: AvailabilitySnapshot = serde_json::from_slice(&message.payload)
        .context("failed to decode device probe response")?;

    if response.device_id != device_id || response.probe_id != probe.probe_id {
        return Err(AppError::from(anyhow::anyhow!(
            "device probe response did not match request"
        )));
    }

    Ok(response)
}

async fn consume_job_lifecycle_events(pool: PgPool, nats: NatsClient) -> anyhow::Result<()> {
    let subject = "elowen.jobs.events".to_string();
    let mut subscription = nats
        .subscribe(subject.clone())
        .await
        .context("failed to subscribe to job lifecycle events")?;

    info!(subject = %subject, "consuming job lifecycle events");

    while let Some(message) = subscription.next().await {
        let event: JobLifecycleEvent = match serde_json::from_slice(&message.payload) {
            Ok(event) => event,
            Err(error) => {
                warn!(error = %error, "failed to decode job lifecycle event");
                continue;
            }
        };

        if let Err(error) = persist_job_lifecycle_event(&pool, &event).await {
            warn!(
                job_id = %event.job_id,
                correlation_id = %event.correlation_id,
                event_type = %event.event_type,
                error = ?error,
                "failed to persist job lifecycle event"
            );
        } else {
            info!(
                job_id = %event.job_id,
                correlation_id = %event.correlation_id,
                event_type = %event.event_type,
                "persisted job lifecycle event"
            );
        }
    }

    Ok(())
}

async fn persist_job_lifecycle_event(
    pool: &PgPool,
    event: &JobLifecycleEvent,
) -> Result<(), AppError> {
    let payload_value = event.payload_json.clone().unwrap_or_else(|| json!({}));
    let mut payload = serde_json::Map::new();
    payload.insert(
        "correlation_id".to_string(),
        Value::String(event.correlation_id.clone()),
    );
    payload.insert(
        "device_id".to_string(),
        Value::String(event.device_id.clone()),
    );

    if let Some(status) = &event.status {
        payload.insert("status".to_string(), Value::String(status.clone()));
    }

    if let Some(result) = &event.result {
        payload.insert("result".to_string(), Value::String(result.clone()));
    }

    if let Some(failure_class) = &event.failure_class {
        payload.insert(
            "failure_class".to_string(),
            Value::String(failure_class.clone()),
        );
    }

    if let Some(worktree_path) = &event.worktree_path {
        payload.insert(
            "worktree_path".to_string(),
            Value::String(worktree_path.clone()),
        );
    }

    if let Some(detail) = &event.detail {
        payload.insert("detail".to_string(), Value::String(detail.clone()));
    }

    payload.insert("payload".to_string(), payload_value.clone());

    insert_job_event(
        pool,
        &event.job_id,
        &event.correlation_id,
        &event.event_type,
        Value::Object(payload),
    )
    .await?;

    if let Some(status) = event.status.as_deref() {
        let completed_at =
            matches!(status, "completed" | "failed" | "cancelled").then_some(event.created_at);
        update_job_state(
            pool,
            &event.job_id,
            status,
            event.result.clone(),
            event.failure_class.clone(),
            completed_at,
        )
        .await?;
    }

    if let Some(summary_markdown) = payload_value
        .get("summary_markdown")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let _ = store_job_summary(pool, &event.job_id, summary_markdown).await?;
    }

    if let Some(execution_report) = payload_value.get("execution_report") {
        update_job_execution_report(pool, &event.job_id, execution_report.clone()).await?;
    }

    if event.event_type == "job.awaiting_approval" {
        let thread_id = sqlx::query_scalar::<_, String>("select thread_id from jobs where id = $1")
            .bind(&event.job_id)
            .fetch_one(pool)
            .await?;
        let approval_summary = payload_value
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Approve push for completed job");
        let approval =
            upsert_pending_push_approval(pool, &thread_id, &event.job_id, approval_summary).await?;
        insert_job_event(
            pool,
            &event.job_id,
            &event.correlation_id,
            "approval.requested",
            json!({
                "correlation_id": event.correlation_id.clone(),
                "approval_id": approval.id,
                "action_type": approval.action_type,
                "summary": approval.summary,
            }),
        )
        .await?;
    }

    maybe_post_thread_assistant_reply(pool, event).await?;
    touch_thread_for_job(pool, &event.job_id).await?;
    Ok(())
}

async fn load_thread_record(pool: &PgPool, thread_id: &str) -> Result<ThreadRecord, AppError> {
    sqlx::query_as::<_, ThreadRecord>(
        r#"
        select id, title, status, current_summary_id, created_at, updated_at
        from threads
        where id = $1
        "#,
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow::anyhow!("thread not found")))
}

async fn load_thread_messages(
    pool: &PgPool,
    thread_id: &str,
) -> Result<Vec<MessageRecord>, AppError> {
    let messages = sqlx::query_as::<_, MessageRecord>(
        r#"
        select id, thread_id, role, content, status, created_at, updated_at
        from messages
        where thread_id = $1
        order by created_at asc, id asc
        "#,
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;

    Ok(messages)
}

async fn load_thread_jobs(pool: &PgPool, thread_id: &str) -> Result<Vec<JobRecord>, AppError> {
    let jobs = sqlx::query_as::<_, JobRecord>(
        r#"
        select
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            status,
            result,
            failure_class,
            repo_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        from jobs
        where thread_id = $1
        order by created_at desc, id desc
        "#,
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;

    Ok(jobs)
}

async fn load_job_record(pool: &PgPool, job_id: &str) -> Result<JobRecord, AppError> {
    sqlx::query_as::<_, JobRecord>(
        r#"
        select
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            status,
            result,
            failure_class,
            repo_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        from jobs
        where id = $1
        "#,
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow::anyhow!("job not found")))
}

async fn load_job_correlation_id(pool: &PgPool, job_id: &str) -> Result<String, AppError> {
    sqlx::query_scalar::<_, String>("select correlation_id from jobs where id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::not_found(anyhow::anyhow!("job not found")))
}

async fn load_current_job_summary(
    pool: &PgPool,
    job_id: &str,
) -> Result<Option<SummaryRecord>, AppError> {
    let summary_id = sqlx::query_scalar::<_, Option<String>>(
        "select current_summary_id from jobs where id = $1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    match summary_id {
        Some(summary_id) => Ok(Some(load_summary(pool, &summary_id).await?)),
        None => Ok(None),
    }
}

async fn load_job_detail(state: &AppState, job_id: &str) -> Result<JobDetail, AppError> {
    let job = load_job_record(&state.pool, job_id).await?;
    let job_state = sqlx::query_as::<_, JobStateRow>(
        r#"
        select current_summary_id, execution_report_json
        from jobs
        where id = $1
        "#,
    )
    .bind(job_id)
    .fetch_one(&state.pool)
    .await?;

    let summary = match job_state.current_summary_id {
        Some(summary_id) => Some(load_summary(&state.pool, &summary_id).await?),
        None => None,
    };

    let approvals = load_job_approvals(&state.pool, job_id).await?;
    let related_notes = load_related_job_notes(state, &job, summary.as_ref()).await?;

    let events = sqlx::query_as::<_, JobEventRow>(
        r#"
        select id, job_id, correlation_id, event_type, payload_json, created_at
        from job_events
        where job_id = $1
        order by created_at asc, id asc
        "#,
    )
    .bind(job_id)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .map(job_event_from_row)
    .collect();

    Ok(JobDetail {
        job,
        execution_report_json: job_state.execution_report_json.0,
        summary,
        approvals,
        related_notes,
        events,
    })
}

async fn insert_job_event(
    pool: &PgPool,
    job_id: &str,
    correlation_id: &str,
    event_type: &str,
    payload_json: Value,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        insert into job_events (id, job_id, correlation_id, event_type, payload_json)
        values ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(Ulid::new().to_string())
    .bind(job_id)
    .bind(correlation_id)
    .bind(event_type)
    .bind(SqlxJson(payload_json))
    .execute(pool)
    .await?;

    Ok(())
}

async fn update_job_state(
    pool: &PgPool,
    job_id: &str,
    status: &str,
    result: Option<String>,
    failure_class: Option<String>,
    completed_at: Option<DateTime<Utc>>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update jobs
        set status = $2,
            result = $3,
            failure_class = $4,
            completed_at = $5,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(status)
    .bind(result)
    .bind(failure_class)
    .bind(completed_at)
    .execute(pool)
    .await?;

    Ok(())
}

async fn update_job_status_only(pool: &PgPool, job_id: &str, status: &str) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update jobs
        set status = $2,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(status)
    .execute(pool)
    .await?;

    Ok(())
}

async fn update_job_execution_report(
    pool: &PgPool,
    job_id: &str,
    execution_report_json: Value,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update jobs
        set execution_report_json = $2,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(SqlxJson(execution_report_json))
    .execute(pool)
    .await?;

    Ok(())
}

async fn store_job_summary(
    pool: &PgPool,
    job_id: &str,
    content: &str,
) -> Result<SummaryRecord, AppError> {
    let version = sqlx::query_scalar::<_, i64>(
        r#"
        select coalesce(max(version), 0)::bigint + 1
        from summaries
        where scope = 'job' and source_id = $1
        "#,
    )
    .bind(job_id)
    .fetch_one(pool)
    .await?;
    let summary_id = Ulid::new().to_string();

    let summary = sqlx::query_as::<_, SummaryRecord>(
        r#"
        insert into summaries (id, scope, source_id, version, content)
        values ($1, 'job', $2, $3, $4)
        returning id, scope, source_id, version, content, created_at
        "#,
    )
    .bind(&summary_id)
    .bind(job_id)
    .bind(version as i32)
    .bind(content)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        r#"
        update jobs
        set current_summary_id = $2,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(&summary_id)
    .execute(pool)
    .await?;

    Ok(summary)
}

async fn load_summary(pool: &PgPool, summary_id: &str) -> Result<SummaryRecord, AppError> {
    sqlx::query_as::<_, SummaryRecord>(
        r#"
        select id, scope, source_id, version, content, created_at
        from summaries
        where id = $1
        "#,
    )
    .bind(summary_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow::anyhow!("summary not found")))
}

async fn load_job_approvals(pool: &PgPool, job_id: &str) -> Result<Vec<ApprovalRecord>, AppError> {
    let approvals = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        select
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        from approvals
        where job_id = $1
        order by created_at desc, id desc
        "#,
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?;

    Ok(approvals)
}

async fn load_related_thread_notes(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
) -> Result<Vec<NoteRecord>, AppError> {
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

async fn load_related_job_notes(
    state: &AppState,
    job: &JobRecord,
    summary: Option<&SummaryRecord>,
) -> Result<Vec<NoteRecord>, AppError> {
    let promoted = search_notes(state, None, Some("job"), Some(job.id.as_str()), 8).await?;
    let query = build_job_notes_query(job, summary);
    let related = search_notes(state, query.as_deref(), None, None, 6).await?;
    Ok(merge_note_sets(promoted, related))
}

async fn search_notes(
    state: &AppState,
    query: Option<&str>,
    source_kind: Option<&str>,
    source_id: Option<&str>,
    limit: usize,
) -> Result<Vec<NoteRecord>, AppError> {
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
        .context("failed to query notes service")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow::anyhow!(
            "notes search failed with status {status}: {body}"
        )));
    }

    response
        .json::<Vec<NoteRecord>>()
        .await
        .context("failed to decode notes search response")
        .map_err(AppError::from)
}

async fn promote_note_to_service(
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
        .context("failed to send note promotion request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow::anyhow!(
            "note promotion failed with status {status}: {body}"
        )));
    }

    let detail = response
        .json::<Value>()
        .await
        .context("failed to decode note promotion response")?;
    let note = detail.get("note").cloned().unwrap_or(detail);

    serde_json::from_value::<NoteRecord>(note)
        .context("failed to parse promoted note response")
        .map_err(AppError::from)
}

async fn generate_conversational_reply(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[NoteRecord],
) -> Result<String, AppError> {
    if state.assistant.api_key.is_some() {
        match request_assistant_reply(state, thread, messages, jobs, related_notes).await {
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
    ))
}

async fn request_assistant_reply(
    state: &AppState,
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[NoteRecord],
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
            "input": build_conversation_input(thread, messages, jobs, related_notes),
            "max_output_tokens": 600,
        }))
        .send()
        .await
        .context("failed to send assistant response request")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(AppError::from(anyhow::anyhow!(
            "assistant response request failed with status {status}: {body}"
        )));
    }

    let body = response
        .json::<Value>()
        .await
        .context("failed to decode assistant response")?;
    extract_response_text(&body).ok_or_else(|| {
        AppError::from(anyhow::anyhow!(
            "assistant response did not include any text output"
        ))
    })
}

fn build_conversation_instructions() -> &'static str {
    "You are Elowen, the orchestrator-side assistant in Workflow #2 conversational mode. \
Reply conversationally and concisely. Use only the provided thread, job, and notes context. \
Do not claim to have dispatched a laptop job, modified code, run tests, or inspected a worktree unless the context explicitly says that already happened. \
When the user appears to want real code execution, planning, or repo changes, explain that execution happens through an explicit handoff into Workflow #1 and suggest using the dispatch controls when they are ready. \
If the context is incomplete, say so plainly rather than inventing details."
}

fn build_conversation_input(
    thread: &ThreadRecord,
    messages: &[MessageRecord],
    jobs: &[JobRecord],
    related_notes: &[NoteRecord],
) -> String {
    let latest_user = messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| summarize_text(&message.content, 600))
        .unwrap_or_default();

    format!(
        "Thread title: {title}\n\
Thread status: {status}\n\
\n\
Latest user message:\n{latest_user}\n\
\n\
Recent thread messages:\n{message_context}\n\
\n\
Recent related jobs:\n{job_context}\n\
\n\
Related notes:\n{note_context}\n",
        title = thread.title,
        status = thread.status,
        latest_user = latest_user,
        message_context = format_message_context(messages, 10),
        job_context = format_job_context(jobs, 3),
        note_context = format_note_context(related_notes, 4),
    )
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
            format!(
                "- [{}] {}",
                message.role,
                summarize_text(&message.content, 280)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_job_context(jobs: &[JobRecord], limit: usize) -> String {
    if jobs.is_empty() {
        return "- none".to_string();
    }

    jobs.iter()
        .take(limit)
        .map(|job| {
            format!(
                "- job `{}` in repo `{}` is `{}`{}{}",
                job.short_id,
                job.repo_name,
                job.status,
                job.result
                    .as_deref()
                    .map(|result| format!(", result `{result}`"))
                    .unwrap_or_default(),
                job.branch_name
                    .as_deref()
                    .map(|branch| format!(", branch `{branch}`"))
                    .unwrap_or_default()
            )
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
                "- note `{}`: {}",
                note.title,
                summarize_text(&note.summary, 200)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
            "I’m in conversational mode for thread `{}` right now, so I have not created a laptop job yet. \
I can help refine the request or you can use the explicit dispatch controls to hand this off into Workflow #1 when you’re ready.{}",
            thread.title, repo_hint
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

fn looks_like_execution_request(value: &str) -> bool {
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

fn merge_note_sets(primary: Vec<NoteRecord>, secondary: Vec<NoteRecord>) -> Vec<NoteRecord> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    for note in primary.into_iter().chain(secondary) {
        if seen.insert(note.note_id.clone()) {
            merged.push(note);
        }
    }

    merged
}

fn build_thread_notes_query(thread: &ThreadRecord, messages: &[MessageRecord]) -> Option<String> {
    let mut terms = vec![thread.title.clone()];
    if let Some(message) = messages.iter().rev().find(|message| message.role == "user") {
        terms.push(summarize_text(&message.content, 120));
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
        terms.push(summarize_text(&summary.content, 120));
    }

    let query = terms
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!query.is_empty()).then_some(query)
}

fn summarize_text(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= limit {
        trimmed.to_string()
    } else {
        trimmed.chars().take(limit).collect::<String>()
    }
}

async fn upsert_pending_push_approval(
    pool: &PgPool,
    thread_id: &str,
    job_id: &str,
    summary: &str,
) -> Result<ApprovalRecord, AppError> {
    if let Some(approval) = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        select
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        from approvals
        where job_id = $1
          and action_type = 'push'
          and status = 'pending'
        order by created_at desc, id desc
        limit 1
        "#,
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(approval);
    }

    let approval_id = Ulid::new().to_string();
    let approval = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        insert into approvals (id, thread_id, job_id, action_type, status, summary)
        values ($1, $2, $3, 'push', 'pending', $4)
        returning
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        "#,
    )
    .bind(&approval_id)
    .bind(thread_id)
    .bind(job_id)
    .bind(summary)
    .fetch_one(pool)
    .await?;

    Ok(approval)
}

async fn ensure_thread_exists(pool: &PgPool, thread_id: &str) -> Result<(), AppError> {
    let count = sqlx::query_scalar::<_, i64>("select count(*)::bigint from threads where id = $1")
        .bind(thread_id)
        .fetch_one(pool)
        .await?;

    if count == 0 {
        return Err(AppError::not_found(anyhow::anyhow!("thread not found")));
    }

    Ok(())
}

async fn insert_thread_message(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
) -> Result<MessageRecord, AppError> {
    insert_thread_message_with_status(pool, thread_id, role, content, "committed").await
}

async fn insert_thread_message_with_status(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
) -> Result<MessageRecord, AppError> {
    let message_id = Ulid::new().to_string();
    let message = sqlx::query_as::<_, MessageRecord>(
        r#"
        insert into messages (id, thread_id, role, content, status)
        values ($1, $2, $3, $4, $5)
        returning id, thread_id, role, content, status, created_at, updated_at
        "#,
    )
    .bind(&message_id)
    .bind(thread_id)
    .bind(role)
    .bind(content)
    .bind(status)
    .fetch_one(pool)
    .await?;

    Ok(message)
}

async fn maybe_insert_thread_message_with_status(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
) -> Result<Option<MessageRecord>, AppError> {
    let existing = sqlx::query_scalar::<_, String>(
        "select id from messages where thread_id = $1 and status = $2 limit 1",
    )
    .bind(thread_id)
    .bind(status)
    .fetch_optional(pool)
    .await?;

    if existing.is_some() {
        return Ok(None);
    }

    Ok(Some(
        insert_thread_message_with_status(pool, thread_id, role, content, status).await?,
    ))
}

async fn touch_thread(pool: &PgPool, thread_id: &str) -> Result<(), AppError> {
    sqlx::query("update threads set updated_at = now() where id = $1")
        .bind(thread_id)
        .execute(pool)
        .await?;
    Ok(())
}

async fn maybe_post_thread_assistant_reply(
    pool: &PgPool,
    event: &JobLifecycleEvent,
) -> Result<(), AppError> {
    let Some((status_marker, content)) = build_thread_assistant_reply(pool, event).await? else {
        return Ok(());
    };

    let thread_id = sqlx::query_scalar::<_, String>("select thread_id from jobs where id = $1")
        .bind(&event.job_id)
        .fetch_one(pool)
        .await?;

    let inserted = maybe_insert_thread_message_with_status(
        pool,
        &thread_id,
        "assistant",
        &content,
        &status_marker,
    )
    .await?;

    if inserted.is_some() {
        touch_thread(pool, &thread_id).await?;
    }

    Ok(())
}

async fn build_thread_assistant_reply(
    pool: &PgPool,
    event: &JobLifecycleEvent,
) -> Result<Option<(String, String)>, AppError> {
    let job = load_job_record(pool, &event.job_id).await?;
    let summary = load_current_job_summary(pool, &event.job_id).await?;
    let execution_report = load_job_execution_report(pool, &event.job_id).await?;
    let build_status = execution_report
        .as_ref()
        .and_then(|report| execution_report_status(report, "build"));
    let test_status = execution_report
        .as_ref()
        .and_then(|report| execution_report_status(report, "test"));
    let changed_entries = execution_report
        .as_ref()
        .and_then(|report| execution_report_changed_entries(report));
    let detail = event
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let reply = match event.event_type.as_str() {
        "job.started" => Some((
            format!("job_event:{}:started", event.job_id),
            format!(
                "I started working on job `{}` for repo `{}` on device `{}`.",
                job.short_id, job.repo_name, event.device_id
            ),
        )),
        "job.awaiting_approval" => {
            let approval_summary = event
                .payload_json
                .as_ref()
                .and_then(|payload| payload.get("summary"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("Push is waiting for approval.");

            Some((
                format!("job_event:{}:awaiting_approval", event.job_id),
                format_success_reply(
                    &job,
                    build_status,
                    test_status,
                    changed_entries,
                    approval_summary,
                ),
            ))
        }
        "job.completed" if event.result.as_deref() != Some("success") => Some((
            format!("job_event:{}:completed", event.job_id),
            format_failure_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                detail,
                summary.as_ref(),
            ),
        )),
        "job.failed" => Some((
            format!("job_event:{}:failed", event.job_id),
            format_failure_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                detail,
                summary.as_ref(),
            ),
        )),
        _ => None,
    };

    Ok(reply)
}

async fn load_job_execution_report(pool: &PgPool, job_id: &str) -> Result<Option<Value>, AppError> {
    let report = sqlx::query_scalar::<_, SqlxJson<Value>>(
        "select execution_report_json from jobs where id = $1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .map(|value| value.0);

    Ok(report.filter(|value| value != &json!({})))
}

fn execution_report_status<'a>(report: &'a Value, key: &str) -> Option<&'a str> {
    report.get(key)?.get("status")?.as_str()
}

fn execution_report_changed_entries(report: &Value) -> Option<usize> {
    report
        .get("changed_files")?
        .as_array()
        .map(|items| items.len())
}

fn format_success_reply(
    job: &JobRecord,
    build_status: Option<&str>,
    test_status: Option<&str>,
    changed_entries: Option<usize>,
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

    lines.push(String::new());
    lines.push(approval_summary.to_string());
    lines.join("\n")
}

fn format_failure_reply(
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

async fn touch_thread_for_job(pool: &PgPool, job_id: &str) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update threads
        set updated_at = now()
        where id = (select thread_id from jobs where id = $1)
        "#,
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn select_dispatch_device(
    pool: &PgPool,
    requested_device_id: Option<&str>,
    repo_name: &str,
) -> Result<DeviceRecord, AppError> {
    if let Some(device_id) =
        requested_device_id.and_then(|value| sanitize_optional_string(Some(value.to_string())))
    {
        let device = device_record_from_row(load_device_row(pool, &device_id).await?);
        ensure_repo_allowed(&device, repo_name)?;
        return Ok(device);
    }

    let devices = sqlx::query_as::<_, DeviceRow>(
        r#"
        select id, name, primary_flag, metadata, created_at, updated_at
        from devices
        order by primary_flag desc, updated_at desc, name asc
        "#,
    )
    .fetch_all(pool)
    .await?;

    for device in devices {
        let record = device_record_from_row(device);
        if ensure_repo_allowed(&record, repo_name).is_ok() {
            return Ok(record);
        }
    }

    Err(AppError::conflict(anyhow::anyhow!(
        "no registered device is eligible for the requested repository"
    )))
}

fn ensure_repo_allowed(device: &DeviceRecord, repo_name: &str) -> Result<(), AppError> {
    if device.allowed_repos.is_empty() || device.allowed_repos.iter().any(|repo| repo == repo_name)
    {
        return Ok(());
    }

    Err(AppError::bad_request(anyhow::anyhow!(
        "device is not allowed to run the requested repository"
    )))
}

async fn load_device_row(pool: &PgPool, device_id: &str) -> Result<DeviceRow, AppError> {
    load_device_row_optional(pool, device_id)
        .await?
        .ok_or_else(|| AppError::not_found(anyhow::anyhow!("device not found")))
}

async fn load_device_row_optional(
    pool: &PgPool,
    device_id: &str,
) -> Result<Option<DeviceRow>, AppError> {
    let device = sqlx::query_as::<_, DeviceRow>(
        r#"
        select id, name, primary_flag, metadata, created_at, updated_at
        from devices
        where id = $1
        "#,
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?;

    Ok(device)
}

async fn upsert_device_row(
    pool: &PgPool,
    device_id: &str,
    name: &str,
    primary_flag: bool,
    metadata: DeviceMetadata,
) -> Result<DeviceRow, AppError> {
    let device = sqlx::query_as::<_, DeviceRow>(
        r#"
        insert into devices (id, name, primary_flag, metadata)
        values ($1, $2, $3, $4)
        on conflict (id) do update set
            name = excluded.name,
            primary_flag = excluded.primary_flag,
            metadata = excluded.metadata,
            updated_at = now()
        returning id, name, primary_flag, metadata, created_at, updated_at
        "#,
    )
    .bind(device_id)
    .bind(name)
    .bind(primary_flag)
    .bind(SqlxJson(metadata))
    .fetch_one(pool)
    .await?;

    Ok(device)
}

fn device_record_from_row(row: DeviceRow) -> DeviceRecord {
    let metadata = row.metadata.0;
    let registered_at = metadata.registered_at.unwrap_or(row.created_at);
    let last_seen_at = metadata.last_seen_at.unwrap_or(row.updated_at);

    DeviceRecord {
        id: row.id,
        name: row.name,
        primary_flag: row.primary_flag,
        allowed_repos: metadata.allowed_repos,
        capabilities: metadata.capabilities,
        registered_at,
        last_seen_at,
        last_probe: metadata.last_probe,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn job_event_from_row(row: JobEventRow) -> JobEventRecord {
    JobEventRecord {
        id: row.id,
        job_id: row.job_id,
        correlation_id: row.correlation_id,
        event_type: row.event_type,
        payload_json: row.payload_json.0,
        created_at: row.created_at,
    }
}

fn sanitize_string_list(values: Vec<String>) -> Vec<String> {
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

fn sanitize_optional_string(value: Option<String>) -> Option<String> {
    value.and_then(|candidate| {
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn derive_job_title_from_message(content: &str) -> String {
    let trimmed = content.trim();
    let mut title = trimmed
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("New coding task")
        .trim()
        .trim_end_matches(['.', '!', '?'])
        .to_string();

    if title.chars().count() > 72 {
        title = title.chars().take(72).collect::<String>();
    }

    if title.is_empty() {
        "New coding task".to_string()
    } else {
        title
    }
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut truncated = value.trim().chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
}

fn slugify(value: &str) -> String {
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

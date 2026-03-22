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
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, postgres::PgPoolOptions, types::Json as SqlxJson};
use std::{env, net::SocketAddr, time::Duration};
use tokio::time::timeout;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use ulid::Ulid;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    nats: NatsClient,
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

#[derive(Debug, Serialize)]
struct ThreadDetail {
    #[serde(flatten)]
    thread: ThreadRecord,
    messages: Vec<MessageRecord>,
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

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let database_url = env::var("ELOWEN_DATABASE_URL").context("missing ELOWEN_DATABASE_URL")?;
    let nats_url = env::var("ELOWEN_NATS_URL").context("missing ELOWEN_NATS_URL")?;

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
        .with_state(AppState { pool, nats });

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
    let thread = sqlx::query_as::<_, ThreadRecord>(
        r#"
        select id, title, status, current_summary_id, created_at, updated_at
        from threads
        where id = $1
        "#,
    )
    .bind(&thread_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow::anyhow!("thread not found")))?;

    let messages = sqlx::query_as::<_, MessageRecord>(
        r#"
        select id, thread_id, role, content, status, created_at, updated_at
        from messages
        where thread_id = $1
        order by created_at asc, id asc
        "#,
    )
    .bind(&thread_id)
    .fetch_all(&state.pool)
    .await?;

    Ok(Json(ThreadDetail { thread, messages }))
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

    let thread_exists =
        sqlx::query_scalar::<_, i64>("select count(*)::bigint from threads where id = $1")
            .bind(&thread_id)
            .fetch_one(&state.pool)
            .await?;

    if thread_exists == 0 {
        return Err(AppError::not_found(anyhow::anyhow!("thread not found")));
    }

    let message_id = Ulid::new().to_string();
    let message = sqlx::query_as::<_, MessageRecord>(
        r#"
        insert into messages (id, thread_id, role, content, status)
        values ($1, $2, $3, $4, 'committed')
        returning id, thread_id, role, content, status, created_at, updated_at
        "#,
    )
    .bind(&message_id)
    .bind(&thread_id)
    .bind(&request.role)
    .bind(content)
    .fetch_one(&state.pool)
    .await?;

    sqlx::query("update threads set updated_at = now() where id = $1")
        .bind(&thread_id)
        .execute(&state.pool)
        .await?;

    Ok((StatusCode::CREATED, Json(message)))
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

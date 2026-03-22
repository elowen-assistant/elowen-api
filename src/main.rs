use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, postgres::PgPoolOptions};
use std::{env, net::SocketAddr};
use tower_http::cors::{Any, CorsLayer};
use tracing::info;
use ulid::Ulid;

#[derive(Clone)]
struct AppState {
    pool: PgPool,
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
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let database_url = env::var("ELOWEN_DATABASE_URL").context("missing ELOWEN_DATABASE_URL")?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&database_url)
        .await
        .context("failed to connect to Postgres")?;

    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .context("failed to run API migrations")?;

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
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([axum::http::Method::GET, axum::http::Method::POST])
                .allow_headers(Any),
        )
        .with_state(AppState { pool });

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

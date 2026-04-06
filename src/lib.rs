//! Elowen orchestration API.

mod error;
mod formatting;
mod models;
mod state;

use anyhow::Context;
use async_nats::Client as NatsClient;
use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post, put},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use rand::RngCore;
use reqwest::Client as HttpClient;
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Json as SqlxJson};
use std::{collections::HashSet, env, net::SocketAddr, time::Duration};
use tokio::time::timeout;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};
use ulid::Ulid;

use error::AppError;
use formatting::{
    derive_job_title_from_message, execution_intent_note, execution_report_changed_entries,
    execution_report_last_message, execution_report_status, format_failure_reply,
    format_read_only_success_reply, format_success_reply, format_success_without_push_reply,
    primary_result_excerpt, sanitize_chat_result_text, sanitize_optional_string,
    sanitize_string_list, slugify,
};
use models::*;
use state::{AppState, AssistantRuntime, AuthRuntime, TrustRuntime};

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

/// Starts the orchestration API.
pub async fn run() -> anyhow::Result<()> {
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
    let auth_password = env::var("ELOWEN_UI_PASSWORD")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let auth_operator_label = env::var("ELOWEN_UI_OPERATOR_LABEL")
        .unwrap_or_else(|_| "Operator".to_string())
        .trim()
        .to_string();
    let auth_cookie_name = env::var("ELOWEN_UI_COOKIE_NAME")
        .unwrap_or_else(|_| "elowen_session".to_string())
        .trim()
        .to_string();
    let auth_session_days = env::var("ELOWEN_UI_SESSION_DAYS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(14);
    let orchestrator_signing_key = env::var("ELOWEN_ORCHESTRATOR_SIGNING_KEY")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let require_trusted_edge_registration = env::var("ELOWEN_REQUIRE_TRUSTED_EDGE_REGISTRATION")
        .ok()
        .map(|value| parse_bool(&value))
        .unwrap_or(false);

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

    let state = AppState {
        pool,
        nats,
        http,
        notes_url,
        assistant: AssistantRuntime {
            api_key: assistant_api_key,
            model: assistant_model,
            base_url: assistant_base_url,
        },
        auth: AuthRuntime {
            password: auth_password,
            operator_label: auth_operator_label,
            cookie_name: auth_cookie_name,
            session_ttl: Duration::from_secs(auth_session_days * 24 * 60 * 60),
        },
        trust: TrustRuntime {
            orchestrator_signing_key,
            require_trusted_edge_registration,
        },
    };

    let public_api = Router::new()
        .route("/trust/registration-challenge", get(registration_challenge))
        .route("/devices/{device_id}", put(register_device));

    let protected_api = Router::new()
        .route("/threads", get(list_threads).post(create_thread))
        .route("/threads/{thread_id}", get(get_thread))
        .route("/threads/{thread_id}/messages", post(create_message))
        .route("/threads/{thread_id}/chat", post(create_thread_chat))
        .route(
            "/threads/{thread_id}/message-dispatch",
            post(dispatch_thread_message),
        )
        .route(
            "/threads/{thread_id}/chat-dispatch",
            post(create_chat_dispatch),
        )
        .route("/threads/{thread_id}/notes", get(get_thread_notes))
        .route(
            "/threads/{thread_id}/jobs",
            get(list_thread_jobs).post(create_job),
        )
        .route("/jobs", get(list_jobs))
        .route("/jobs/{job_id}", get(get_job))
        .route("/jobs/{job_id}/notes", get(get_job_notes))
        .route("/jobs/{job_id}/notes/promote", post(promote_job_note))
        .route("/approvals/{approval_id}/resolve", post(resolve_approval))
        .route("/devices", get(list_devices))
        .route("/devices/{device_id}", get(get_device))
        .route(
            "/devices/{device_id}/availability-probe",
            post(probe_device),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authenticated_session,
        ))
        .with_state(state.clone());

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/auth/session", get(get_auth_session))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .nest("/api/v1", public_api)
        .nest("/api/v1", protected_api)
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
        .with_state(state);

    info!(%address, "starting elowen-api");

    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_auth_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthSessionStatus>, AppError> {
    purge_expired_sessions(&state.pool).await?;

    let operator_label =
        current_session_operator_label(&state.pool, &state.auth.cookie_name, &headers).await?;

    Ok(Json(AuthSessionStatus {
        enabled: state.auth.enabled(),
        authenticated: operator_label.is_some() || !state.auth.enabled(),
        operator_label,
    }))
}

async fn registration_challenge(
    State(state): State<AppState>,
) -> Result<Json<RegistrationChallengeResponse>, AppError> {
    let signing_key = load_orchestrator_signing_key(&state)?;
    let challenge_id = Ulid::new().to_string();
    let mut challenge_bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut challenge_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(challenge_bytes);
    let issued_at = Utc::now();
    let payload = orchestrator_challenge_payload(&challenge_id, &challenge, issued_at);
    let signature = signing_key.sign(payload.as_bytes());

    Ok(Json(RegistrationChallengeResponse {
        challenge_id,
        challenge,
        issued_at,
        orchestrator_public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
        signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    }))
}

async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<(StatusCode, HeaderMap, Json<AuthSessionStatus>), AppError> {
    let expected_password =
        state.auth.password.as_ref().ok_or_else(|| {
            AppError::conflict(anyhow::anyhow!("web UI authentication is disabled"))
        })?;

    if request.password != *expected_password {
        return Err(AppError::unauthorized(anyhow::anyhow!("invalid password")));
    }

    purge_expired_sessions(&state.pool).await?;

    let token = new_session_token();
    let expires_at = Utc::now()
        + chrono::Duration::from_std(state.auth.session_ttl)
            .map_err(|error| AppError::from(anyhow::anyhow!(error)))?;

    sqlx::query(
        r#"
        insert into ui_sessions (token, operator_label, expires_at)
        values ($1, $2, $3)
        "#,
    )
    .bind(&token)
    .bind(&state.auth.operator_label)
    .bind(expires_at)
    .execute(&state.pool)
    .await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie_value(
            &state.auth.cookie_name,
            Some(&token),
            Some(state.auth.session_ttl),
        ))
        .map_err(AppError::from)?,
    );

    Ok((
        StatusCode::OK,
        headers,
        Json(AuthSessionStatus {
            enabled: true,
            authenticated: true,
            operator_label: Some(state.auth.operator_label.clone()),
        }),
    ))
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap, Json<AuthSessionStatus>), AppError> {
    if let Some(token) = session_token_from_headers(&state.auth.cookie_name, &headers) {
        sqlx::query("delete from ui_sessions where token = $1")
            .bind(token)
            .execute(&state.pool)
            .await?;
    }

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie_value(&state.auth.cookie_name, None, None))
            .map_err(AppError::from)?,
    );

    Ok((
        StatusCode::OK,
        response_headers,
        Json(AuthSessionStatus {
            enabled: state.auth.enabled(),
            authenticated: false,
            operator_label: None,
        }),
    ))
}

async fn require_authenticated_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if !state.auth.enabled() {
        return Ok(next.run(request).await);
    }

    purge_expired_sessions(&state.pool).await?;

    let authenticated =
        current_session_operator_label(&state.pool, &state.auth.cookie_name, request.headers())
            .await?
            .is_some();

    if !authenticated {
        return Err(AppError::unauthorized(anyhow::anyhow!("sign in required")));
    }

    Ok(next.run(request).await)
}

async fn current_session_operator_label(
    pool: &PgPool,
    cookie_name: &str,
    headers: &HeaderMap,
) -> Result<Option<String>, AppError> {
    let Some(token) = session_token_from_headers(cookie_name, headers) else {
        return Ok(None);
    };

    let operator_label = sqlx::query_scalar::<_, String>(
        r#"
        select operator_label
        from ui_sessions
        where token = $1
          and expires_at > now()
        "#,
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;

    Ok(operator_label)
}

async fn purge_expired_sessions(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("delete from ui_sessions where expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(())
}

fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn session_cookie_value(
    cookie_name: &str,
    token: Option<&str>,
    max_age: Option<Duration>,
) -> String {
    match (token, max_age) {
        (Some(token), Some(max_age)) => format!(
            "{cookie_name}={token}; HttpOnly; Path=/; SameSite=Strict; Max-Age={}",
            max_age.as_secs()
        ),
        _ => format!("{cookie_name}=; HttpOnly; Path=/; SameSite=Strict; Max-Age=0"),
    }
}

fn session_token_from_headers<'a>(cookie_name: &str, headers: &'a HeaderMap) -> Option<&'a str> {
    let raw_cookie = headers.get(header::COOKIE)?.to_str().ok()?;

    raw_cookie.split(';').find_map(|segment| {
        let trimmed = segment.trim();
        let (name, value) = trimmed.split_once('=')?;
        (name == cookie_name).then_some(value)
    })
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
    .bind(thread_id)
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
    let execution_draft = maybe_build_execution_draft(&messages, &jobs);
    let assistant_reply = generate_conversational_reply(
        &state,
        &thread,
        &messages,
        &jobs,
        &related_notes,
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

    Ok((
        StatusCode::CREATED,
        Json(ChatReplyResponse {
            user_message,
            assistant_message,
        }),
    ))
}

async fn dispatch_thread_message(
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
    let request_text = sanitize_optional_string(request.request_text.clone())
        .or_else(|| {
            embedded_draft
                .as_ref()
                .map(|draft| draft.request_text.clone())
        })
        .unwrap_or_else(|| source_message.content.trim().to_string());
    if request_text.is_empty() {
        return Err(AppError::bad_request(anyhow::anyhow!(
            "source message content is required"
        )));
    }

    let title = sanitize_optional_string(request.title)
        .or_else(|| embedded_draft.as_ref().map(|draft| draft.title.clone()))
        .unwrap_or_else(|| derive_job_title_from_message(&request_text));
    let repo_name = sanitize_optional_string(Some(request.repo_name.clone()))
        .or_else(|| {
            embedded_draft
                .as_ref()
                .and_then(|draft| draft.repo_name.clone())
        })
        .ok_or_else(|| AppError::bad_request(anyhow::anyhow!("repo name is required")))?;
    let base_branch = sanitize_optional_string(request.base_branch)
        .or_else(|| {
            embedded_draft
                .as_ref()
                .map(|draft| draft.base_branch.clone())
        })
        .unwrap_or_else(|| "main".to_string());
    let execution_intent = request
        .execution_intent
        .or_else(|| {
            embedded_draft
                .as_ref()
                .map(|draft| draft.execution_intent.clone())
        })
        .unwrap_or_else(|| infer_execution_intent(&request_text));

    let job = create_job_record(
        &state,
        &thread_id,
        &CreateJobRequest {
            title,
            repo_name: repo_name.clone(),
            base_branch: Some(base_branch),
            request_text: request_text.clone(),
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
            "Escalated {} message `{}` into job `{}` for repo `{}` in {} mode.\nRequest summary: {}",
            source_message.role,
            source_message.id,
            job.short_id,
            job.repo_name,
            execution_intent_note(&execution_intent),
            summarize_text(&request_text, 160)
        ),
        "workflow.handoff.created",
    )
    .await?;

    touch_thread(&state.pool, &thread_id).await?;

    Ok((
        StatusCode::CREATED,
        Json(MessageDispatchResponse {
            source_message,
            acknowledgement,
            job,
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
    let execution_intent = request
        .execution_intent
        .unwrap_or_else(|| infer_execution_intent(content));

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
            execution_intent: Some(execution_intent.clone()),
        },
    )
    .await?;
    let acknowledgement = insert_thread_message_with_status(
        &state.pool,
        &thread_id,
        "system",
        &format!(
            "Created job `{}` with status `{}` on device `{}` for repo `{}` in {} mode.",
            job.short_id,
            job.status,
            job.device_id
                .clone()
                .unwrap_or_else(|| "unassigned".to_string()),
            job.repo_name,
            execution_intent_note(&execution_intent),
        ),
        "workflow.dispatch.created",
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
    let execution_intent = request
        .execution_intent
        .clone()
        .unwrap_or_else(|| infer_execution_intent(request_text));

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
    .bind(thread_id)
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
            "execution_intent": execution_intent,
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
        execution_intent: execution_intent.clone(),
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
            "execution_intent": execution_intent,
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
    let existing_approval = load_approval_record(&state.pool, &approval_id).await?;
    if existing_approval.status != "pending" {
        return Err(AppError::conflict(anyhow::anyhow!(
            "approval has already been resolved"
        )));
    }
    let approval_job = load_job_record(&state.pool, &existing_approval.job_id).await?;

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

    let approval = approval.ok_or_else(|| {
        AppError::conflict(anyhow::anyhow!(
            "approval could not be updated from pending state"
        ))
    })?;

    if status == "approved" && approval.action_type == "push" {
        if approval_job.device_id.is_none() {
            reset_approval_to_pending(&state.pool, &approval.id).await?;
            return Err(AppError::conflict(anyhow::anyhow!(
                "approved push job is missing an assigned device"
            )));
        }
        if approval_job.branch_name.is_none() {
            reset_approval_to_pending(&state.pool, &approval.id).await?;
            return Err(AppError::conflict(anyhow::anyhow!(
                "approved push job is missing a branch name"
            )));
        }
        if let Err(error) =
            publish_push_approval_command(&state.nats, &approval, &approval_job).await
        {
            reset_approval_to_pending(&state.pool, &approval.id).await?;
            insert_job_event(
                &state.pool,
                &approval.job_id,
                &approval_job.correlation_id,
                "approval.dispatch_failed",
                json!({
                    "approval_id": approval.id,
                    "action_type": approval.action_type,
                    "error": error.error.to_string(),
                }),
            )
            .await?;
            return Err(AppError::from(anyhow::anyhow!(
                "failed to publish approved push command"
            )));
        }
    }

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

    if status == "rejected" || approval.action_type != "push" {
        update_job_status_only(&state.pool, &approval.job_id, "completed").await?;
    }
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
    let trusted_registration = verify_registration_trust(
        &state,
        &sanitized_device_id,
        name,
        request.primary_flag,
        now,
        request.trust.as_ref(),
    )?;
    let existing_metadata = existing.as_ref().map(|row| row.metadata.0.clone());

    let metadata = DeviceMetadata {
        allowed_repos: sanitize_string_list(request.allowed_repos),
        allowed_repo_roots: sanitize_string_list(request.allowed_repo_roots),
        discovered_repos: sanitize_string_list(request.discovered_repos),
        capabilities: sanitize_string_list(request.capabilities),
        registered_at: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.registered_at)
            .or(Some(now)),
        last_seen_at: Some(now),
        last_probe: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.last_probe.clone()),
        edge_public_key: trusted_registration
            .as_ref()
            .map(|registration| registration.edge_public_key.clone())
            .or_else(|| {
                existing_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.edge_public_key.clone())
            }),
        last_trusted_registration_at: trusted_registration
            .as_ref()
            .map(|registration| registration.registered_at)
            .or_else(|| {
                existing_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.last_trusted_registration_at)
            }),
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
        select id, thread_id, role, content, status, payload_json, created_at, updated_at
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

async fn load_thread_message(
    pool: &PgPool,
    thread_id: &str,
    message_id: &str,
) -> Result<MessageRecord, AppError> {
    sqlx::query_as::<_, MessageRecord>(
        r#"
        select id, thread_id, role, content, status, payload_json, created_at, updated_at
        from messages
        where thread_id = $1 and id = $2
        "#,
    )
    .bind(thread_id)
    .bind(message_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow::anyhow!("thread message not found")))
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

async fn load_approval_record(
    pool: &PgPool,
    approval_id: &str,
) -> Result<ApprovalRecord, AppError> {
    sqlx::query_as::<_, ApprovalRecord>(
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
        where id = $1
        "#,
    )
    .bind(approval_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow::anyhow!("approval not found")))
}

async fn reset_approval_to_pending(pool: &PgPool, approval_id: &str) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update approvals
        set status = 'pending',
            resolved_by = null,
            resolution_reason = null,
            resolved_at = null,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(approval_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn publish_push_approval_command(
    nats: &NatsClient,
    approval: &ApprovalRecord,
    job: &JobRecord,
) -> Result<(), AppError> {
    let device_id = job
        .device_id
        .as_deref()
        .ok_or_else(|| AppError::conflict(anyhow::anyhow!("job device is missing")))?;
    let branch_name = job
        .branch_name
        .as_deref()
        .ok_or_else(|| AppError::conflict(anyhow::anyhow!("job branch is missing")))?;
    let command = JobApprovalCommand {
        approval_id: approval.id.clone(),
        job_id: job.id.clone(),
        short_id: job.short_id.clone(),
        correlation_id: job.correlation_id.clone(),
        device_id: device_id.to_string(),
        repo_name: job.repo_name.clone(),
        branch_name: branch_name.to_string(),
        action_type: approval.action_type.clone(),
        approved_at: Utc::now(),
    };
    let subject = format!("elowen.jobs.approvals.{device_id}");
    let payload = serde_json::to_vec(&command).context("failed to serialize approval command")?;
    nats.publish(subject, payload.into())
        .await
        .context("failed to publish approval command")?;
    Ok(())
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
If a current execution draft is present, treat it as editable planning context and help refine it without pretending a job has already been dispatched. Preserve a read-only draft as read-only unless the user clearly asks to make repository changes. \
If the context is incomplete, say so plainly rather than inventing details. \
When prior jobs, approvals, summaries, or notes materially support the answer, mention them briefly using labels like [Job 01abcd12] or [Note Title]."
}

async fn build_conversation_input(
    pool: &PgPool,
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
    pending_approval: Option<ApprovalRecord>,
}

async fn load_conversation_job_context(
    pool: &PgPool,
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

fn message_execution_draft(message: &MessageRecord) -> Option<ExecutionDraft> {
    message
        .payload_json
        .get("execution_draft")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn build_message_payload(execution_draft: Option<&ExecutionDraft>) -> Value {
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
            "I’m in conversational mode for thread `{}` right now, so I have not created a laptop job yet. \
I can help refine the request or you can use the explicit dispatch controls to hand this off into Workflow #1 when you’re ready.{}",
            thread.title, repo_hint
        );
    }

    if let Some(draft) = execution_draft {
        return format!(
            "Iâ€™m still in conversational mode, and I prepared an execution draft for repo `{}` on branch `{}` from the latest request. You can keep refining it here before explicitly dispatching it into Workflow #1.",
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

fn infer_execution_intent(value: &str) -> ExecutionIntent {
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

fn maybe_build_execution_draft(
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

fn maybe_annotate_draft_reply(reply: String, execution_draft: Option<&ExecutionDraft>) -> String {
    let Some(execution_draft) = execution_draft else {
        return reply;
    };

    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return format!(
            "I prepared an execution draft for repo `{}` on branch `{}`. Review it below before dispatching it into Workflow #1.",
            execution_draft
                .repo_name
                .as_deref()
                .unwrap_or("unspecified"),
            execution_draft.base_branch
        );
    }

    format!(
        "{trimmed}\n\nI also drafted an execution handoff below so you can review and refine it before dispatching it into Workflow #1."
    )
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
    insert_thread_message_with_status_and_payload(pool, thread_id, role, content, status, json!({}))
        .await
}

async fn insert_thread_message_with_status_and_payload(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
    payload_json: Value,
) -> Result<MessageRecord, AppError> {
    let message_id = Ulid::new().to_string();
    let message = sqlx::query_as::<_, MessageRecord>(
        r#"
        insert into messages (id, thread_id, role, content, status, payload_json)
        values ($1, $2, $3, $4, $5, $6)
        returning id, thread_id, role, content, status, payload_json, created_at, updated_at
        "#,
    )
    .bind(&message_id)
    .bind(thread_id)
    .bind(role)
    .bind(content)
    .bind(status)
    .bind(SqlxJson(payload_json))
    .fetch_one(pool)
    .await?;

    Ok(message)
}

async fn maybe_insert_thread_message_with_status_and_payload(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
    payload_json: Value,
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
        insert_thread_message_with_status_and_payload(
            pool,
            thread_id,
            role,
            content,
            status,
            payload_json,
        )
        .await?,
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
    let Some((status_marker, content, payload_json)) =
        build_thread_assistant_reply(pool, event).await?
    else {
        return Ok(());
    };

    let thread_id = sqlx::query_scalar::<_, String>("select thread_id from jobs where id = $1")
        .bind(&event.job_id)
        .fetch_one(pool)
        .await?;

    let inserted = maybe_insert_thread_message_with_status_and_payload(
        pool,
        &thread_id,
        "assistant",
        &content,
        &status_marker,
        payload_json,
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
) -> Result<Option<(String, String, Value)>, AppError> {
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
        .and_then(execution_report_changed_entries);
    let last_message = execution_report
        .as_ref()
        .and_then(|report| execution_report_last_message(report));
    let sanitized_last_message = last_message.map(sanitize_chat_result_text);
    let execution_intent = execution_report
        .as_ref()
        .and_then(|report| report.get("execution_intent").cloned())
        .and_then(|value| serde_json::from_value::<ExecutionIntent>(value).ok());
    let detail = event
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let make_completion_payload = |details: String| {
        json!({
            "job_result": {
                "job_id": job.id,
                "job_short_id": job.short_id,
                "details": details,
            }
        })
    };

    let reply = match event.event_type.as_str() {
        "job.started" => Some((
            format!("job_event:{}:started", event.job_id),
            format!(
                "I started working on job `{}` for repo `{}` on device `{}`.",
                job.short_id, job.repo_name, event.device_id
            ),
            json!({}),
        )),
        "job.push_started" => Some((
            format!("job_event:{}:push_started", event.job_id),
            format!(
                "Push approval landed for job `{}`. I started pushing branch `{}` from device `{}`.",
                job.short_id,
                job.branch_name.as_deref().unwrap_or("unknown"),
                event.device_id
            ),
            json!({}),
        )),
        "job.push_completed" => Some((
            format!("job_event:{}:push_completed", event.job_id),
            format!(
                "I finished pushing branch `{}` for job `{}` to `origin`.",
                job.branch_name.as_deref().unwrap_or("unknown"),
                job.short_id
            ),
            json!({}),
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
            let details = format_success_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                sanitized_last_message.as_deref(),
                approval_summary,
            );
            let content = if let Some(last_message) = sanitized_last_message.as_deref() {
                format!(
                    "{}\n\nPush approval is pending.",
                    primary_result_excerpt(last_message)
                )
            } else {
                details.clone()
            };

            Some((
                format!("job_event:{}:awaiting_approval", event.job_id),
                content,
                make_completion_payload(details),
            ))
        }
        "job.completed"
            if event.result.as_deref() == Some("success")
                && matches!(execution_intent, Some(ExecutionIntent::ReadOnly)) =>
        {
            let details = format_read_only_success_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                sanitized_last_message.as_deref(),
            );
            Some((
                format!("job_event:{}:completed", event.job_id),
                sanitized_last_message
                    .as_deref()
                    .map(primary_result_excerpt)
                    .unwrap_or_else(|| details.clone()),
                make_completion_payload(details),
            ))
        }
        "job.completed"
            if event.result.as_deref() == Some("success")
                && !event
                    .payload_json
                    .as_ref()
                    .and_then(|payload| payload.get("push_required"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
        {
            let details = format_success_without_push_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                sanitized_last_message.as_deref(),
            );
            Some((
                format!("job_event:{}:completed", event.job_id),
                sanitized_last_message
                    .as_deref()
                    .map(primary_result_excerpt)
                    .unwrap_or_else(|| details.clone()),
                make_completion_payload(details),
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
            json!({}),
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
            json!({}),
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
    if !device_has_repo_scope(device) {
        return Ok(());
    }

    if device.allowed_repos.iter().any(|repo| repo == repo_name)
        || device.discovered_repos.iter().any(|repo| repo == repo_name)
    {
        return Ok(());
    }

    Err(AppError::bad_request(anyhow::anyhow!(
        "device is not allowed to run the requested repository"
    )))
}

fn device_has_repo_scope(device: &DeviceRecord) -> bool {
    !device.allowed_repos.is_empty()
        || !device.allowed_repo_roots.is_empty()
        || !device.discovered_repos.is_empty()
}

struct TrustedRegistration {
    edge_public_key: String,
    registered_at: DateTime<Utc>,
}

fn verify_registration_trust(
    state: &AppState,
    device_id: &str,
    name: &str,
    primary_flag: bool,
    now: DateTime<Utc>,
    proof: Option<&DeviceRegistrationTrustProof>,
) -> Result<Option<TrustedRegistration>, AppError> {
    let Some(proof) = proof else {
        if state.trust.require_trusted_edge_registration {
            return Err(AppError::unauthorized(anyhow::anyhow!(
                "trusted edge registration proof is required"
            )));
        }

        return Ok(None);
    };

    let signing_key = load_orchestrator_signing_key(state)?;
    let orchestrator_public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
    let orchestrator_verifying_key =
        decode_verifying_key(&orchestrator_public_key, "orchestrator public key")?;
    let challenge_signature = decode_signature(
        &proof.orchestrator_signature,
        "orchestrator challenge signature",
    )?;
    let challenge_payload = orchestrator_challenge_payload(
        &proof.orchestrator_challenge_id,
        &proof.orchestrator_challenge,
        proof.orchestrator_challenge_issued_at,
    );

    orchestrator_verifying_key
        .verify(challenge_payload.as_bytes(), &challenge_signature)
        .map_err(|_| {
            AppError::unauthorized(anyhow::anyhow!(
                "orchestrator registration challenge signature is invalid"
            ))
        })?;

    let age = now.signed_duration_since(proof.orchestrator_challenge_issued_at);
    if age.num_seconds() < -60 || age.num_seconds() > 10 * 60 {
        return Err(AppError::unauthorized(anyhow::anyhow!(
            "orchestrator registration challenge is outside the allowed time window"
        )));
    }

    let edge_verifying_key = decode_verifying_key(&proof.edge_public_key, "edge public key")?;
    let edge_signature = decode_signature(&proof.edge_signature, "edge registration signature")?;
    let registration_payload = edge_registration_payload(device_id, name, primary_flag, proof);

    edge_verifying_key
        .verify(registration_payload.as_bytes(), &edge_signature)
        .map_err(|_| {
            AppError::unauthorized(anyhow::anyhow!("edge registration signature is invalid"))
        })?;

    Ok(Some(TrustedRegistration {
        edge_public_key: proof.edge_public_key.clone(),
        registered_at: now,
    }))
}

fn load_orchestrator_signing_key(state: &AppState) -> Result<SigningKey, AppError> {
    let key = state
        .trust
        .orchestrator_signing_key
        .as_deref()
        .ok_or_else(|| {
            AppError::conflict(anyhow::anyhow!(
                "orchestrator signing key is not configured"
            ))
        })?;

    decode_signing_key(key, "orchestrator signing key")
}

fn decode_signing_key(value: &str, label: &str) -> Result<SigningKey, AppError> {
    let bytes = decode_base64_bytes(value, label)?;
    let key_bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        AppError::bad_request(anyhow::anyhow!(
            "{label} must decode to a 32-byte Ed25519 private key"
        ))
    })?;

    Ok(SigningKey::from_bytes(&key_bytes))
}

fn decode_verifying_key(value: &str, label: &str) -> Result<VerifyingKey, AppError> {
    let bytes = decode_base64_bytes(value, label)?;
    let key_bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        AppError::bad_request(anyhow::anyhow!(
            "{label} must decode to a 32-byte Ed25519 public key"
        ))
    })?;

    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| AppError::bad_request(anyhow::anyhow!("{label} is not a valid Ed25519 key")))
}

fn decode_signature(value: &str, label: &str) -> Result<Signature, AppError> {
    let bytes = decode_base64_bytes(value, label)?;
    Signature::from_slice(&bytes).map_err(|_| {
        AppError::bad_request(anyhow::anyhow!(
            "{label} must decode to a 64-byte Ed25519 signature"
        ))
    })
}

fn decode_base64_bytes(value: &str, label: &str) -> Result<Vec<u8>, AppError> {
    URL_SAFE_NO_PAD
        .decode(value.trim())
        .map_err(|_| AppError::bad_request(anyhow::anyhow!("{label} is not valid base64url")))
}

fn orchestrator_challenge_payload(
    challenge_id: &str,
    challenge: &str,
    issued_at: DateTime<Utc>,
) -> String {
    format!(
        "elowen-orchestrator-registration-challenge\n{challenge_id}\n{challenge}\n{}",
        issued_at.to_rfc3339()
    )
}

fn edge_registration_payload(
    device_id: &str,
    name: &str,
    primary_flag: bool,
    proof: &DeviceRegistrationTrustProof,
) -> String {
    format!(
        "elowen-edge-registration\n{device_id}\n{name}\n{primary_flag}\n{}\n{}\n{}\n{}",
        proof.orchestrator_challenge_id,
        proof.orchestrator_challenge,
        proof.orchestrator_challenge_issued_at.to_rfc3339(),
        proof.edge_public_key
    )
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
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
        allowed_repo_roots: metadata.allowed_repo_roots,
        discovered_repos: metadata.discovered_repos,
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

#[cfg(test)]
mod tests {
    use super::{device_has_repo_scope, ensure_repo_allowed};
    use crate::models::DeviceRecord;
    use chrono::Utc;

    fn sample_device() -> DeviceRecord {
        let now = Utc::now();
        DeviceRecord {
            id: "device-1".to_string(),
            name: "Elowen Laptop".to_string(),
            primary_flag: true,
            allowed_repos: Vec::new(),
            allowed_repo_roots: Vec::new(),
            discovered_repos: Vec::new(),
            capabilities: vec!["codex".to_string()],
            registered_at: now,
            last_seen_at: now,
            last_probe: None,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn devices_without_repo_scope_remain_unrestricted() {
        let device = sample_device();
        assert!(!device_has_repo_scope(&device));
        assert!(ensure_repo_allowed(&device, "any-repo").is_ok());
    }

    #[test]
    fn discovered_repo_allows_dispatch() {
        let mut device = sample_device();
        device.allowed_repo_roots = vec!["D:\\Projects".to_string()];
        device.discovered_repos = vec!["elowen-api".to_string()];

        assert!(device_has_repo_scope(&device));
        assert!(ensure_repo_allowed(&device, "elowen-api").is_ok());
        assert!(ensure_repo_allowed(&device, "other-repo").is_err());
    }
}

//! Application bootstrap for the orchestration API.

use anyhow::Context;
use axum::{
    Router,
    middleware::{self},
    routing::{get, post, put},
};
use reqwest::Client as HttpClient;
use sqlx::postgres::PgPoolOptions;
use std::{env, net::SocketAddr, time::Duration};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tracing::{info, warn};

use crate::{
    auth::AuthProvider,
    routes::{
        create_chat_dispatch, create_job, create_message, create_thread, create_thread_chat,
        dispatch_thread_message, get_auth_session, get_device, get_job, get_job_notes, get_thread,
        get_thread_notes, list_devices, list_jobs, list_thread_jobs, list_threads, login, logout,
        probe_device, promote_job_note, register_device, registration_challenge,
        require_admin_session, require_operator_session, require_viewer_session, resolve_approval,
        stream_ui_events,
    },
    services::lifecycle::consume_job_lifecycle_events,
    state::{AppState, AssistantRuntime, AuthRuntime, TrustRuntime},
    trust::parse_bool,
};

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
    let auth_config_path = env::var("ELOWEN_UI_AUTH_CONFIG_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
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
    let auth_cookie_secure = env::var("ELOWEN_UI_COOKIE_SECURE")
        .ok()
        .map(|value| parse_bool(&value))
        .unwrap_or(false);
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

    let (ui_events, _) = broadcast::channel(256);
    let event_pool = pool.clone();
    let event_nats = nats.clone();
    let event_ui_events = ui_events.clone();
    tokio::spawn(async move {
        if let Err(error) =
            consume_job_lifecycle_events(event_pool, event_nats, event_ui_events).await
        {
            warn!(error = %error, "job lifecycle consumer stopped");
        }
    });

    let port = env::var("PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(8080);
    let address = SocketAddr::from(([0, 0, 0, 0], port));
    let auth_provider =
        AuthProvider::from_config(auth_config_path, auth_password, auth_operator_label)
            .context("failed to configure UI authentication provider")?;

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
            provider: auth_provider,
            cookie_name: auth_cookie_name,
            cookie_secure: auth_cookie_secure,
            session_ttl: Duration::from_secs(auth_session_days * 24 * 60 * 60),
        },
        trust: TrustRuntime {
            orchestrator_signing_key,
            require_trusted_edge_registration,
        },
        ui_events,
    };

    let public_api = Router::new()
        .route("/trust/registration-challenge", get(registration_challenge))
        .route("/devices/{device_id}", put(register_device));

    let viewer_api = Router::new()
        .route("/threads", get(list_threads))
        .route("/threads/{thread_id}", get(get_thread))
        .route("/threads/{thread_id}/notes", get(get_thread_notes))
        .route("/threads/{thread_id}/jobs", get(list_thread_jobs))
        .route("/jobs", get(list_jobs))
        .route("/jobs/{job_id}", get(get_job))
        .route("/jobs/{job_id}/notes", get(get_job_notes))
        .route("/events", get(stream_ui_events))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_viewer_session,
        ))
        .with_state(state.clone());
    let operator_api = Router::new()
        .route("/threads", post(create_thread))
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
        .route("/threads/{thread_id}/jobs", post(create_job))
        .route("/jobs/{job_id}/notes/promote", post(promote_job_note))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_operator_session,
        ))
        .with_state(state.clone());
    let admin_api = Router::new()
        .route("/approvals/{approval_id}/resolve", post(resolve_approval))
        .route("/devices", get(list_devices))
        .route("/devices/{device_id}", get(get_device))
        .route(
            "/devices/{device_id}/availability-probe",
            post(probe_device),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_admin_session,
        ))
        .with_state(state.clone());

    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/api/v1/auth/session", get(get_auth_session))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/logout", post(logout))
        .nest("/api/v1", public_api)
        .nest("/api/v1", viewer_api)
        .nest("/api/v1", operator_api)
        .nest("/api/v1", admin_api)
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

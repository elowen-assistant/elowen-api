//! Shared runtime state for the orchestration API.

use async_nats::Client as NatsClient;
use reqwest::Client as HttpClient;
use sqlx::PgPool;
use std::time::Duration;

/// Runtime settings for orchestrator-side assistant replies.
#[derive(Clone)]
pub(crate) struct AssistantRuntime {
    pub(crate) api_key: Option<String>,
    pub(crate) model: String,
    pub(crate) base_url: String,
}

/// Runtime settings for the web UI session boundary.
#[derive(Clone)]
pub(crate) struct AuthRuntime {
    pub(crate) password: Option<String>,
    pub(crate) operator_label: String,
    pub(crate) cookie_name: String,
    pub(crate) session_ttl: Duration,
}

impl AuthRuntime {
    pub(crate) fn enabled(&self) -> bool {
        self.password.is_some()
    }
}

/// Shared application state injected into Axum handlers.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) pool: PgPool,
    pub(crate) nats: NatsClient,
    pub(crate) http: HttpClient,
    pub(crate) notes_url: String,
    pub(crate) assistant: AssistantRuntime,
    pub(crate) auth: AuthRuntime,
}

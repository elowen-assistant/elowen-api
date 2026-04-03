//! Shared runtime state for the orchestration API.

use async_nats::Client as NatsClient;
use reqwest::Client as HttpClient;
use sqlx::PgPool;

/// Runtime settings for orchestrator-side assistant replies.
#[derive(Clone)]
pub(crate) struct AssistantRuntime {
    pub(crate) api_key: Option<String>,
    pub(crate) model: String,
    pub(crate) base_url: String,
}

/// Shared application state injected into Axum handlers.
#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) pool: PgPool,
    pub(crate) nats: NatsClient,
    pub(crate) http: HttpClient,
    pub(crate) notes_url: String,
    pub(crate) assistant: AssistantRuntime,
}

//! Shared runtime state for the orchestration API.

use crate::auth::AuthProvider;
use crate::models::UiEvent;
use async_nats::Client as NatsClient;
use reqwest::Client as HttpClient;
use sqlx::PgPool;
use std::time::Duration;
use tokio::sync::broadcast;

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
    pub(crate) provider: AuthProvider,
    pub(crate) cookie_name: String,
    pub(crate) cookie_secure: bool,
    pub(crate) session_ttl: Duration,
}

impl AuthRuntime {
    pub(crate) fn enabled(&self) -> bool {
        self.provider.enabled()
    }
}

/// Runtime settings for optional edge/orchestrator trust enforcement.
#[derive(Clone)]
pub(crate) struct TrustRuntime {
    pub(crate) orchestrator_signing_keys: Vec<String>,
    pub(crate) require_trusted_edge_registration: bool,
    pub(crate) revoked_edge_public_keys: Vec<String>,
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
    pub(crate) trust: TrustRuntime,
    pub(crate) ui_events: broadcast::Sender<UiEvent>,
}

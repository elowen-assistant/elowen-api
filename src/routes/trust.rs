//! Orchestrator trust lifecycle routes.

use axum::{
    Extension, Json,
    extract::{Path, State},
};

use crate::{
    auth::SessionActor,
    error::AppError,
    models::{OrchestratorSignerStateRecord, TrustLifecycleActionRequest},
    routes::require_session_actor,
    services::ui_events::{publish_ui_event, signer_ui_event},
    state::AppState,
    trust::{list_orchestrator_signer_states, set_orchestrator_signer_status},
};

pub(crate) async fn list_orchestrator_signers(
    State(state): State<AppState>,
) -> Result<Json<Vec<OrchestratorSignerStateRecord>>, AppError> {
    Ok(Json(list_orchestrator_signer_states(&state).await?))
}

pub(crate) async fn stage_orchestrator_signer(
    State(state): State<AppState>,
    actor: Option<Extension<SessionActor>>,
    Path(key_id): Path<String>,
    Json(request): Json<TrustLifecycleActionRequest>,
) -> Result<Json<OrchestratorSignerStateRecord>, AppError> {
    let actor = require_session_actor(actor);
    let signer = set_orchestrator_signer_status(
        &state,
        &key_id,
        "staged",
        Some(&actor),
        request.reason.as_deref(),
    )
    .await?;
    publish_ui_event(&state, signer_ui_event(&key_id));
    Ok(Json(signer))
}

pub(crate) async fn activate_orchestrator_signer(
    State(state): State<AppState>,
    actor: Option<Extension<SessionActor>>,
    Path(key_id): Path<String>,
    Json(request): Json<TrustLifecycleActionRequest>,
) -> Result<Json<OrchestratorSignerStateRecord>, AppError> {
    let actor = require_session_actor(actor);
    let signer = set_orchestrator_signer_status(
        &state,
        &key_id,
        "active",
        Some(&actor),
        request.reason.as_deref(),
    )
    .await?;
    publish_ui_event(&state, signer_ui_event(&key_id));
    Ok(Json(signer))
}

pub(crate) async fn retire_orchestrator_signer(
    State(state): State<AppState>,
    actor: Option<Extension<SessionActor>>,
    Path(key_id): Path<String>,
    Json(request): Json<TrustLifecycleActionRequest>,
) -> Result<Json<OrchestratorSignerStateRecord>, AppError> {
    let actor = require_session_actor(actor);
    let signer = set_orchestrator_signer_status(
        &state,
        &key_id,
        "retired",
        Some(&actor),
        request.reason.as_deref(),
    )
    .await?;
    publish_ui_event(&state, signer_ui_event(&key_id));
    Ok(Json(signer))
}

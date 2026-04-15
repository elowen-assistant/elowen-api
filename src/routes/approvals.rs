//! Approval resolution routes.

use axum::{
    Json,
    extract::{Path, State},
};
use serde_json::json;

use crate::{
    db::{
        approvals::{load_approval_record, reset_approval_to_pending, resolve_approval_record},
        jobs::{
            insert_job_event, load_job_correlation_id, load_job_record, update_job_status_only,
        },
        threads::touch_thread_for_job,
    },
    error::AppError,
    models::{ApprovalRecord, ResolveApprovalRequest},
    services::{
        jobs::publish_push_approval_command,
        ui_events::{job_ui_event, publish_ui_event},
    },
    state::AppState,
};

pub(crate) async fn resolve_approval(
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
    let resolved_by = crate::formatting::sanitize_optional_string(request.resolved_by);
    let reason = crate::formatting::sanitize_optional_string(request.reason);
    let existing_approval = load_approval_record(&state.pool, &approval_id).await?;
    if existing_approval.status != "pending" {
        return Err(AppError::conflict(anyhow::anyhow!(
            "approval has already been resolved"
        )));
    }
    let approval_job = load_job_record(&state.pool, &existing_approval.job_id).await?;

    let approval = resolve_approval_record(
        &state.pool,
        &approval_id,
        status,
        resolved_by.clone(),
        reason.clone(),
    )
    .await?;

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
    publish_ui_event(
        &state,
        job_ui_event(
            &approval.thread_id,
            &approval.job_id,
            approval_job.device_id.as_deref(),
        ),
    );

    Ok(Json(approval))
}

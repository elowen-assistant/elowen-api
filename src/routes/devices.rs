//! Device and probe routes.

use anyhow::Context;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use tokio::time::timeout;
use ulid::Ulid;

use crate::{
    db::devices::{
        list_devices as load_devices, load_device_row, load_device_row_optional, upsert_device_row,
    },
    error::AppError,
    formatting::{sanitize_optional_string, sanitize_string_list},
    models::{
        AvailabilityProbeMessage, AvailabilitySnapshot, DeviceMetadata, DeviceRecord,
        ProbeDeviceRequest, RegisterDeviceRequest,
    },
    services::ui_events::{device_ui_event, publish_ui_event},
    state::AppState,
    trust::verify_registration_trust,
};

pub(crate) async fn list_devices(
    State(state): State<AppState>,
) -> Result<Json<Vec<DeviceRecord>>, AppError> {
    Ok(Json(load_devices(&state.pool).await?))
}

pub(crate) async fn get_device(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceRecord>, AppError> {
    let device: DeviceRecord = load_device_row(&state.pool, &device_id).await?.into();
    Ok(Json(device))
}

pub(crate) async fn register_device(
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

    let device: DeviceRecord = device.into();
    publish_ui_event(&state, device_ui_event(&device.id));

    Ok((status, Json(device)))
}

pub(crate) async fn probe_device(
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
        std::time::Duration::from_secs(5),
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

    publish_ui_event(&state, device_ui_event(&device.id));

    Ok(Json(response))
}

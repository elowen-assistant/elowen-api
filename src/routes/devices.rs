//! Device and probe routes.

use anyhow::Context;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use chrono::Utc;
use std::collections::BTreeMap;
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
        DeviceRepository, DeviceTrustMetadata, DeviceTrustStatus, ProbeDeviceRequest,
        RegisterDeviceRequest, RegistrationTrustIntent, RepositoryOption, RevokeDeviceTrustRequest,
    },
    services::jobs::selectable_repositories,
    services::ui_events::{device_ui_event, publish_ui_event},
    state::AppState,
    trust::{TrustedRegistration, verify_registration_trust},
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

pub(crate) async fn list_repositories(
    State(state): State<AppState>,
) -> Result<Json<Vec<RepositoryOption>>, AppError> {
    let devices = load_devices(&state.pool).await?;
    let mut counts = BTreeMap::<String, usize>::new();

    for device in devices {
        for repository in selectable_repositories(&device) {
            *counts.entry(repository.name).or_default() += 1;
        }
    }

    Ok(Json(
        counts
            .into_iter()
            .map(|(name, device_count)| RepositoryOption { name, device_count })
            .collect(),
    ))
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
    let existing_trust = existing_metadata
        .as_ref()
        .map(|metadata| {
            metadata.trust.clone().normalized(
                metadata.edge_public_key.clone(),
                metadata.last_trusted_registration_at,
            )
        })
        .unwrap_or_default();

    let mut trust = next_device_trust_state(existing_trust, trusted_registration.as_ref())?;
    trust.enrollment_kind = Some(enrollment_kind(
        request.primary_flag,
        trusted_registration
            .as_ref()
            .map(|registration| &registration.registration_intent),
        existing.is_some(),
    ));
    trust = finalize_trust_metadata(trust);

    let metadata = DeviceMetadata {
        allowed_repos: sanitize_string_list(request.allowed_repos),
        allowed_repo_roots: sanitize_string_list(request.allowed_repo_roots),
        hidden_repos: sanitize_string_list(request.hidden_repos),
        excluded_repo_paths: sanitize_string_list(request.excluded_repo_paths),
        discovered_repos: sanitize_string_list(request.discovered_repos),
        repositories: sanitize_device_repositories(request.repositories),
        capabilities: sanitize_string_list(request.capabilities),
        registered_at: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.registered_at)
            .or(Some(now)),
        last_seen_at: Some(now),
        last_probe: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.last_probe.clone()),
        trust,
        edge_public_key: None,
        last_trusted_registration_at: None,
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

pub(crate) async fn revoke_device_trust(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(request): Json<RevokeDeviceTrustRequest>,
) -> Result<Json<DeviceRecord>, AppError> {
    let device = load_device_row(&state.pool, &device_id).await?;
    let mut metadata = device.metadata.0.clone();
    let mut trust = metadata.trust.clone().normalized(
        metadata.edge_public_key.clone(),
        metadata.last_trusted_registration_at,
    );
    let target_key = request
        .edge_public_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| trust.current_edge_public_key.clone())
        .ok_or_else(|| {
            AppError::conflict(anyhow::anyhow!(
                "device does not have trusted key material to revoke"
            ))
        })?;

    if !trust
        .revoked_edge_public_keys
        .iter()
        .any(|value| value == &target_key)
    {
        trust.revoked_edge_public_keys.push(target_key.clone());
        trust.revoked_edge_public_keys.sort();
        trust.revoked_edge_public_keys.dedup();
    }
    trust.status = DeviceTrustStatus::Revoked;
    trust.revoked_at = Some(Utc::now());
    trust.reason = request
        .reason
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or(Some(format!("revoked key {target_key}")));
    trust.enrollment_kind = trust
        .enrollment_kind
        .clone()
        .or(Some(if device.primary_flag {
            "primary".to_string()
        } else {
            "additional_edge".to_string()
        }));
    trust = finalize_trust_metadata(trust);

    metadata.trust = trust;
    metadata.edge_public_key = None;
    metadata.last_trusted_registration_at = None;

    let device: DeviceRecord = upsert_device_row(
        &state.pool,
        &device.id,
        &device.name,
        device.primary_flag,
        metadata,
    )
    .await?
    .into();
    publish_ui_event(&state, device_ui_event(&device.id));

    Ok(Json(device))
}

fn sanitize_device_repositories(repositories: Vec<DeviceRepository>) -> Vec<DeviceRepository> {
    let mut sanitized = repositories
        .into_iter()
        .filter_map(|repository| {
            let name = repository.name.trim();
            if name.is_empty() {
                return None;
            }

            let mut branches = repository
                .branches
                .into_iter()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty())
                .collect::<Vec<_>>();
            branches.sort();
            branches.dedup();

            Some(DeviceRepository {
                name: name.to_string(),
                branches,
            })
        })
        .collect::<Vec<_>>();
    sanitized.sort();
    sanitized.dedup();
    sanitized
}

fn next_device_trust_state(
    mut existing: DeviceTrustMetadata,
    trusted_registration: Option<&TrustedRegistration>,
) -> Result<DeviceTrustMetadata, AppError> {
    if let Some(registered) = trusted_registration {
        if existing.revoked_at.is_some() {
            return Err(AppError::unauthorized(anyhow::anyhow!(
                "device trust has been revoked and cannot be re-enrolled without clearing revocation"
            )));
        }

        if existing
            .revoked_edge_public_keys
            .iter()
            .any(|value| value == &registered.edge_public_key)
        {
            return Err(AppError::unauthorized(anyhow::anyhow!(
                "edge public key has been revoked for this device"
            )));
        }

        match existing.current_edge_public_key.clone() {
            Some(current_key) if current_key == registered.edge_public_key => {
                existing.status = DeviceTrustStatus::Trusted;
            }
            Some(current_key) => {
                if matches!(
                    registered.registration_intent,
                    RegistrationTrustIntent::Enroll
                ) {
                    return Err(AppError::conflict(anyhow::anyhow!(
                        "trusted device is already enrolled with a different key; use an explicit rotation or re-enrollment intent"
                    )));
                }
                existing.previous_edge_public_keys.push(current_key);
                existing.previous_edge_public_keys.sort();
                existing.previous_edge_public_keys.dedup();
                existing.current_edge_public_key = Some(registered.edge_public_key.clone());
                existing.rotated_at = Some(registered.registered_at);
                existing.status = DeviceTrustStatus::Rotated;
            }
            None => {
                existing.current_edge_public_key = Some(registered.edge_public_key.clone());
                existing.status = DeviceTrustStatus::Trusted;
            }
        }

        existing.last_trusted_registration_at = Some(registered.registered_at);
        existing.last_orchestrator_key_id = Some(registered.orchestrator_key_id.clone());
        existing.last_orchestrator_public_key = Some(registered.orchestrator_public_key.clone());
        existing.last_registration_intent = Some(registered.registration_intent.clone());
        existing.status_reason = None;
        existing.reason = None;
        existing.revoked_at = None;
        existing.updated_at = Some(registered.registered_at);
        return Ok(existing);
    }

    if existing.current_edge_public_key.is_some() {
        existing.status = DeviceTrustStatus::AttentionRequired;
        existing.status_reason = Some(
            "trusted device re-registered without a trust proof; verify the edge trust configuration"
                .to_string(),
        );
    } else if existing.revoked_at.is_none() {
        existing.status = DeviceTrustStatus::Untrusted;
        existing.status_reason = None;
    }

    Ok(existing)
}

fn finalize_trust_metadata(mut trust: DeviceTrustMetadata) -> DeviceTrustMetadata {
    trust.updated_at = trust
        .updated_at
        .or(trust.revoked_at)
        .or(trust.rotated_at)
        .or(trust.last_trusted_registration_at);

    if trust.label.is_none() {
        trust.label = Some(match trust.status {
            DeviceTrustStatus::Trusted => "Trusted".to_string(),
            DeviceTrustStatus::Rotated => "Rotated".to_string(),
            DeviceTrustStatus::Revoked => "Revoked".to_string(),
            DeviceTrustStatus::Untrusted => "Untrusted".to_string(),
            DeviceTrustStatus::AttentionRequired => "Needs Attention".to_string(),
        });
    }

    trust.requires_attention = matches!(
        trust.status,
        DeviceTrustStatus::Revoked
            | DeviceTrustStatus::Untrusted
            | DeviceTrustStatus::AttentionRequired
    );
    trust.can_dispatch = Some(!trust.requires_attention);

    if trust.summary.is_none() {
        trust.summary = Some(match trust.status {
            DeviceTrustStatus::Trusted => "Trusted enrollment is active for this edge.".to_string(),
            DeviceTrustStatus::Rotated => {
                "Trust material was rotated and should be verified before dispatch.".to_string()
            }
            DeviceTrustStatus::Revoked => "Trust for this edge has been revoked.".to_string(),
            DeviceTrustStatus::Untrusted => {
                "This edge has not completed trusted enrollment yet.".to_string()
            }
            DeviceTrustStatus::AttentionRequired => {
                "This edge needs trust review before it should be used for sensitive work."
                    .to_string()
            }
        });
    }

    if trust.detail.is_none() && matches!(trust.status, DeviceTrustStatus::Rotated) {
        trust.detail = Some(
            "Confirm the new edge key belongs to the intended device before dispatching trusted work."
                .to_string(),
        );
    }

    if trust.reason.is_none() {
        trust.reason = trust.status_reason.clone();
    }

    trust
}

fn enrollment_kind(
    primary_flag: bool,
    registration_intent: Option<&RegistrationTrustIntent>,
    already_exists: bool,
) -> String {
    match registration_intent {
        Some(RegistrationTrustIntent::Rotate) => "rotation".to_string(),
        Some(RegistrationTrustIntent::Reenroll) => "re_enrollment".to_string(),
        _ if primary_flag && !already_exists => "primary".to_string(),
        _ if primary_flag => "primary".to_string(),
        _ => "additional_edge".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::next_device_trust_state;
    use crate::{
        models::{DeviceTrustMetadata, DeviceTrustStatus, RegistrationTrustIntent},
        trust::TrustedRegistration,
    };
    use axum::http::StatusCode;
    use chrono::Utc;

    fn trusted_registration(
        edge_public_key: &str,
        registration_intent: RegistrationTrustIntent,
    ) -> TrustedRegistration {
        TrustedRegistration {
            edge_public_key: edge_public_key.to_string(),
            registered_at: Utc::now(),
            orchestrator_key_id: "orchestrator-1-testkey".to_string(),
            orchestrator_public_key: "test-public-key".to_string(),
            registration_intent,
        }
    }

    #[test]
    fn trusted_registration_without_existing_key_becomes_trusted() {
        let next = next_device_trust_state(
            DeviceTrustMetadata::default(),
            Some(&trusted_registration(
                "edge-key-a",
                RegistrationTrustIntent::Enroll,
            )),
        )
        .expect("trusted registration should succeed");

        assert_eq!(next.status, DeviceTrustStatus::Trusted);
        assert_eq!(next.current_edge_public_key.as_deref(), Some("edge-key-a"));
        assert!(next.last_trusted_registration_at.is_some());
    }

    #[test]
    fn different_key_requires_explicit_rotation_or_reenrollment() {
        let existing = DeviceTrustMetadata {
            status: DeviceTrustStatus::Trusted,
            current_edge_public_key: Some("edge-key-a".to_string()),
            ..Default::default()
        };

        let error = next_device_trust_state(
            existing,
            Some(&trusted_registration(
                "edge-key-b",
                RegistrationTrustIntent::Enroll,
            )),
        )
        .expect_err("ambiguous key replacement should fail");

        assert_eq!(error.status, StatusCode::CONFLICT);
    }

    #[test]
    fn explicit_rotation_marks_device_rotated() {
        let existing = DeviceTrustMetadata {
            status: DeviceTrustStatus::Trusted,
            current_edge_public_key: Some("edge-key-a".to_string()),
            ..Default::default()
        };

        let next = next_device_trust_state(
            existing,
            Some(&trusted_registration(
                "edge-key-b",
                RegistrationTrustIntent::Rotate,
            )),
        )
        .expect("explicit rotation should succeed");

        assert_eq!(next.status, DeviceTrustStatus::Rotated);
        assert_eq!(next.current_edge_public_key.as_deref(), Some("edge-key-b"));
        assert_eq!(
            next.previous_edge_public_keys,
            vec!["edge-key-a".to_string()]
        );
        assert!(next.rotated_at.is_some());
    }

    #[test]
    fn unsigned_reregistration_from_trusted_device_needs_attention() {
        let existing = DeviceTrustMetadata {
            status: DeviceTrustStatus::Trusted,
            current_edge_public_key: Some("edge-key-a".to_string()),
            ..Default::default()
        };

        let next =
            next_device_trust_state(existing, None).expect("unsigned refresh should succeed");

        assert_eq!(next.status, DeviceTrustStatus::AttentionRequired);
        assert!(next.status_reason.is_some());
    }
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

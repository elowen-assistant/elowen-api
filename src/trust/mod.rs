//! Trusted registration helpers.

use anyhow::anyhow;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use std::collections::HashMap;

use crate::{
    db::trust::{list_signer_states, upsert_signer_state},
    error::AppError,
    models::{
        DeviceRegistrationTrustProof, OrchestratorSignerStateRecord, OrchestratorTrustSigner,
        RegistrationTrustIntent,
    },
    state::AppState,
};

#[derive(Debug, Clone)]
pub(crate) struct TrustedRegistration {
    pub(crate) edge_public_key: String,
    pub(crate) registered_at: DateTime<Utc>,
    pub(crate) orchestrator_key_id: String,
    pub(crate) orchestrator_public_key: String,
    pub(crate) registration_intent: RegistrationTrustIntent,
    pub(crate) previous_edge_public_key: Option<String>,
}

#[derive(Clone)]
struct ConfiguredOrchestratorSigner {
    key_id: String,
    public_key: String,
    signing_key: SigningKey,
    status: String,
}

pub(crate) async fn verify_registration_trust(
    state: &AppState,
    device_id: &str,
    name: &str,
    primary_flag: bool,
    now: DateTime<Utc>,
    proof: Option<&DeviceRegistrationTrustProof>,
) -> Result<Option<TrustedRegistration>, AppError> {
    let Some(proof) = proof else {
        if state.trust.require_trusted_edge_registration {
            return Err(AppError::unauthorized(anyhow!(
                "trusted edge registration proof is required"
            ))
            .with_code("trusted_registration_required"));
        }

        return Ok(None);
    };

    if state
        .trust
        .revoked_edge_public_keys
        .iter()
        .any(|key| key == &proof.edge_public_key)
    {
        return Err(
            AppError::unauthorized(anyhow!("edge public key has been revoked"))
                .with_code("edge_key_revoked"),
        );
    }

    let signers = load_orchestrator_signers(state).await?;
    let signer = match signers.iter().find(|candidate| {
        candidate.public_key == proof.orchestrator_public_key
            || candidate.key_id == proof.orchestrator_key_id
    }) {
        Some(signer)
            if signer.public_key == proof.orchestrator_public_key
                && signer.key_id == proof.orchestrator_key_id =>
        {
            signer
        }
        Some(_) => {
            return Err(AppError::unauthorized(anyhow!(
                "orchestrator signer identity did not match configured trust anchors"
            ))
            .with_code("orchestrator_signer_mismatch"));
        }
        None => {
            return Err(AppError::unauthorized(anyhow!(
                "orchestrator signer is not trusted for registration"
            ))
            .with_code("orchestrator_signer_untrusted"));
        }
    };
    if signer.status == "retired" {
        return Err(
            AppError::unauthorized(anyhow!("orchestrator signer is retired"))
                .with_code("orchestrator_signer_retired"),
        );
    }

    let challenge_signature = decode_signature(
        &proof.orchestrator_signature,
        "orchestrator challenge signature",
    )?;
    let challenge_payload = orchestrator_challenge_payload(
        &proof.orchestrator_challenge_id,
        &proof.orchestrator_challenge,
        proof.orchestrator_challenge_issued_at,
    );

    signer
        .signing_key
        .verifying_key()
        .verify(challenge_payload.as_bytes(), &challenge_signature)
        .map_err(|_| {
            AppError::unauthorized(anyhow!(
                "orchestrator registration challenge signature is invalid"
            ))
            .with_code("orchestrator_challenge_signature_invalid")
        })?;

    let age = now.signed_duration_since(proof.orchestrator_challenge_issued_at);
    if age.num_seconds() < -60 || age.num_seconds() > 10 * 60 {
        return Err(AppError::unauthorized(anyhow!(
            "orchestrator registration challenge is outside the allowed time window"
        ))
        .with_code("orchestrator_challenge_expired"));
    }

    let edge_verifying_key = decode_verifying_key(&proof.edge_public_key, "edge public key")?;
    let edge_signature = decode_signature(&proof.edge_signature, "edge registration signature")?;
    let registration_payload = edge_registration_payload(device_id, name, primary_flag, proof);

    edge_verifying_key
        .verify(registration_payload.as_bytes(), &edge_signature)
        .map_err(|_| {
            AppError::unauthorized(anyhow!("edge registration signature is invalid"))
                .with_code("edge_registration_signature_invalid")
        })?;

    let previous_edge_public_key = match (
        proof.previous_edge_public_key.as_deref(),
        proof.previous_edge_signature.as_deref(),
    ) {
        (Some(previous_public_key), Some(previous_signature)) => {
            let previous_verifying_key =
                decode_verifying_key(previous_public_key, "previous edge public key")?;
            let previous_signature =
                decode_signature(previous_signature, "previous edge registration signature")?;
            let previous_payload = edge_registration_payload_with_edge_key(
                device_id,
                name,
                primary_flag,
                proof,
                previous_public_key,
            );
            previous_verifying_key
                .verify(previous_payload.as_bytes(), &previous_signature)
                .map_err(|_| {
                    AppError::unauthorized(anyhow!(
                        "previous edge registration signature is invalid"
                    ))
                    .with_code("rotation_previous_signature_invalid")
                })?;
            Some(previous_public_key.to_string())
        }
        (None, None) => None,
        _ => {
            return Err(AppError::bad_request(anyhow!(
                "previous edge public key and signature must be supplied together"
            ))
            .with_code("rotation_previous_key_incomplete"));
        }
    };

    Ok(Some(TrustedRegistration {
        edge_public_key: proof.edge_public_key.clone(),
        registered_at: now,
        orchestrator_key_id: signer.key_id.clone(),
        orchestrator_public_key: signer.public_key.clone(),
        registration_intent: proof.registration_intent.clone(),
        previous_edge_public_key,
    }))
}

pub(crate) async fn load_active_orchestrator_signer(
    state: &AppState,
) -> Result<(String, String, SigningKey), AppError> {
    let signer = load_orchestrator_signers(state)
        .await?
        .into_iter()
        .find(|signer| signer.status == "active")
        .ok_or_else(|| AppError::conflict(anyhow!("orchestrator signing key is not configured")))?;
    Ok((signer.key_id, signer.public_key, signer.signing_key))
}

async fn load_orchestrator_signers(
    state: &AppState,
) -> Result<Vec<ConfiguredOrchestratorSigner>, AppError> {
    if state.trust.orchestrator_signing_keys.is_empty() {
        return Err(AppError::conflict(anyhow!(
            "orchestrator signing key is not configured"
        )));
    }

    let mut configured = state
        .trust
        .orchestrator_signing_keys
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let signing_key = decode_signing_key(value, "orchestrator signing key")?;
            let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
            Ok(ConfiguredOrchestratorSigner {
                key_id: orchestrator_key_id(&public_key, index),
                public_key,
                signing_key,
                status: String::new(),
            })
        })
        .collect::<Result<Vec<_>, AppError>>()?;

    sync_configured_signers(state, &configured).await?;
    let states = list_signer_states(&state.pool).await?;

    for signer in &mut configured {
        signer.status = states
            .iter()
            .find(|state| state.public_key == signer.public_key)
            .map(|state| state.status.clone())
            .unwrap_or_else(|| "staged".to_string());
    }

    Ok(configured)
}

pub(crate) async fn exported_orchestrator_signers(
    state: &AppState,
) -> Result<Vec<OrchestratorTrustSigner>, AppError> {
    Ok(load_orchestrator_signers(state)
        .await?
        .into_iter()
        .filter(|signer| signer.status != "retired")
        .map(|signer| OrchestratorTrustSigner {
            key_id: signer.key_id,
            public_key: signer.public_key,
            active: signer.status == "active",
        })
        .collect())
}

pub(crate) async fn list_orchestrator_signer_states(
    state: &AppState,
) -> Result<Vec<OrchestratorSignerStateRecord>, AppError> {
    let configured = configured_orchestrator_signers(state)?;
    sync_configured_signers(state, &configured).await?;
    list_signer_states(&state.pool).await
}

pub(crate) async fn set_orchestrator_signer_status(
    state: &AppState,
    key_id: &str,
    status: &str,
    actor: Option<&crate::auth::SessionActor>,
    reason: Option<&str>,
) -> Result<OrchestratorSignerStateRecord, AppError> {
    let configured = configured_orchestrator_signers(state)?;
    sync_configured_signers(state, &configured).await?;
    let Some(signer) = configured.iter().find(|signer| signer.key_id == key_id) else {
        return Err(
            AppError::not_found(anyhow!("orchestrator signer not found"))
                .with_code("orchestrator_signer_not_configured"),
        );
    };
    if status == "active" {
        let existing_statuses = list_signer_states(&state.pool)
            .await?
            .into_iter()
            .map(|state| (state.key_id, state.status))
            .collect::<HashMap<_, _>>();
        for other in configured.iter().filter(|other| other.key_id != key_id) {
            if existing_statuses
                .get(&other.key_id)
                .is_some_and(|status| status == "retired")
            {
                continue;
            }
            let _ = upsert_signer_state(
                &state.pool,
                &other.key_id,
                &other.public_key,
                "staged",
                actor,
                Some("deactivated by signer activation"),
            )
            .await?;
        }
    }
    upsert_signer_state(
        &state.pool,
        &signer.key_id,
        &signer.public_key,
        status,
        actor,
        reason,
    )
    .await
}

fn configured_orchestrator_signers(
    state: &AppState,
) -> Result<Vec<ConfiguredOrchestratorSigner>, AppError> {
    state
        .trust
        .orchestrator_signing_keys
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let signing_key = decode_signing_key(value, "orchestrator signing key")?;
            let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
            Ok(ConfiguredOrchestratorSigner {
                key_id: orchestrator_key_id(&public_key, index),
                public_key,
                signing_key,
                status: String::new(),
            })
        })
        .collect()
}

async fn sync_configured_signers(
    state: &AppState,
    configured: &[ConfiguredOrchestratorSigner],
) -> Result<(), AppError> {
    let existing = list_signer_states(&state.pool).await?;
    let has_active = existing.iter().any(|state| state.status == "active");
    for (index, signer) in configured.iter().enumerate() {
        if existing
            .iter()
            .any(|state| state.public_key == signer.public_key)
        {
            continue;
        }
        let status = if !has_active && index == 0 {
            "active"
        } else {
            "staged"
        };
        upsert_signer_state(
            &state.pool,
            &signer.key_id,
            &signer.public_key,
            status,
            None,
            Some("synchronized from configured orchestrator signing keys"),
        )
        .await?;
    }
    Ok(())
}

pub(crate) fn orchestrator_key_id(public_key: &str, index: usize) -> String {
    let prefix = public_key.chars().take(12).collect::<String>();
    format!("orchestrator-{}-{prefix}", index + 1)
}

pub(crate) fn decode_signing_key(value: &str, label: &str) -> Result<SigningKey, AppError> {
    let bytes = decode_base64_bytes(value, label)?;
    let key_bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        AppError::bad_request(anyhow!(
            "{label} must decode to a 32-byte Ed25519 private key"
        ))
    })?;

    Ok(SigningKey::from_bytes(&key_bytes))
}

pub(crate) fn decode_verifying_key(value: &str, label: &str) -> Result<VerifyingKey, AppError> {
    let bytes = decode_base64_bytes(value, label)?;
    let key_bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        AppError::bad_request(anyhow!(
            "{label} must decode to a 32-byte Ed25519 public key"
        ))
    })?;

    VerifyingKey::from_bytes(&key_bytes)
        .map_err(|_| AppError::bad_request(anyhow!("{label} is not a valid Ed25519 key")))
}

pub(crate) fn decode_signature(value: &str, label: &str) -> Result<Signature, AppError> {
    let bytes = decode_base64_bytes(value, label)?;
    Signature::from_slice(&bytes).map_err(|_| {
        AppError::bad_request(anyhow!(
            "{label} must decode to a 64-byte Ed25519 signature"
        ))
    })
}

pub(crate) fn decode_base64_bytes(value: &str, label: &str) -> Result<Vec<u8>, AppError> {
    URL_SAFE_NO_PAD
        .decode(value.trim())
        .map_err(|_| AppError::bad_request(anyhow!("{label} is not valid base64url")))
}

pub(crate) fn orchestrator_challenge_payload(
    challenge_id: &str,
    challenge: &str,
    issued_at: DateTime<Utc>,
) -> String {
    format!(
        "elowen-orchestrator-registration-challenge\n{challenge_id}\n{challenge}\n{}",
        issued_at.to_rfc3339()
    )
}

pub(crate) fn edge_registration_payload(
    device_id: &str,
    name: &str,
    primary_flag: bool,
    proof: &DeviceRegistrationTrustProof,
) -> String {
    format!(
        "elowen-edge-registration\n{device_id}\n{name}\n{primary_flag}\n{}\n{}\n{}\n{}\n{}\n{}",
        proof.orchestrator_challenge_id,
        proof.orchestrator_challenge,
        proof.orchestrator_challenge_issued_at.to_rfc3339(),
        proof.orchestrator_key_id,
        proof.orchestrator_public_key,
        proof.edge_public_key
    )
}

fn edge_registration_payload_with_edge_key(
    device_id: &str,
    name: &str,
    primary_flag: bool,
    proof: &DeviceRegistrationTrustProof,
    edge_public_key: &str,
) -> String {
    format!(
        "elowen-edge-registration\n{device_id}\n{name}\n{primary_flag}\n{}\n{}\n{}\n{}\n{}\n{}",
        proof.orchestrator_challenge_id,
        proof.orchestrator_challenge,
        proof.orchestrator_challenge_issued_at.to_rfc3339(),
        proof.orchestrator_key_id,
        proof.orchestrator_public_key,
        edge_public_key
    )
}

pub(crate) fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

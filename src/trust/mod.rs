//! Trusted registration helpers.

use anyhow::anyhow;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};

use crate::{
    error::AppError,
    models::{DeviceRegistrationTrustProof, OrchestratorTrustSigner, RegistrationTrustIntent},
    state::AppState,
};

#[derive(Debug, Clone)]
pub(crate) struct TrustedRegistration {
    pub(crate) edge_public_key: String,
    pub(crate) registered_at: DateTime<Utc>,
    pub(crate) orchestrator_key_id: String,
    pub(crate) orchestrator_public_key: String,
    pub(crate) registration_intent: RegistrationTrustIntent,
}

struct ConfiguredOrchestratorSigner {
    key_id: String,
    public_key: String,
    signing_key: SigningKey,
}

pub(crate) fn verify_registration_trust(
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
            )));
        }

        return Ok(None);
    };

    if state
        .trust
        .revoked_edge_public_keys
        .iter()
        .any(|key| key == &proof.edge_public_key)
    {
        return Err(AppError::unauthorized(anyhow!(
            "edge public key has been revoked"
        )));
    }

    let signers = load_orchestrator_signers(state)?;
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
            )));
        }
        None => {
            return Err(AppError::unauthorized(anyhow!(
                "orchestrator signer is not trusted for registration"
            )));
        }
    };

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
        })?;

    let age = now.signed_duration_since(proof.orchestrator_challenge_issued_at);
    if age.num_seconds() < -60 || age.num_seconds() > 10 * 60 {
        return Err(AppError::unauthorized(anyhow!(
            "orchestrator registration challenge is outside the allowed time window"
        )));
    }

    let edge_verifying_key = decode_verifying_key(&proof.edge_public_key, "edge public key")?;
    let edge_signature = decode_signature(&proof.edge_signature, "edge registration signature")?;
    let registration_payload = edge_registration_payload(device_id, name, primary_flag, proof);

    edge_verifying_key
        .verify(registration_payload.as_bytes(), &edge_signature)
        .map_err(|_| AppError::unauthorized(anyhow!("edge registration signature is invalid")))?;

    Ok(Some(TrustedRegistration {
        edge_public_key: proof.edge_public_key.clone(),
        registered_at: now,
        orchestrator_key_id: signer.key_id.clone(),
        orchestrator_public_key: signer.public_key.clone(),
        registration_intent: proof.registration_intent.clone(),
    }))
}

pub(crate) fn load_active_orchestrator_signer(
    state: &AppState,
) -> Result<(String, String, SigningKey), AppError> {
    let signer = load_orchestrator_signers(state)?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::conflict(anyhow!("orchestrator signing key is not configured")))?;
    Ok((signer.key_id, signer.public_key, signer.signing_key))
}

fn load_orchestrator_signers(
    state: &AppState,
) -> Result<Vec<ConfiguredOrchestratorSigner>, AppError> {
    if state.trust.orchestrator_signing_keys.is_empty() {
        return Err(AppError::conflict(anyhow!(
            "orchestrator signing key is not configured"
        )));
    }

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
            })
        })
        .collect()
}

pub(crate) fn exported_orchestrator_signers(
    state: &AppState,
) -> Result<Vec<OrchestratorTrustSigner>, AppError> {
    Ok(load_orchestrator_signers(state)?
        .into_iter()
        .enumerate()
        .map(|(index, signer)| OrchestratorTrustSigner {
            key_id: signer.key_id,
            public_key: signer.public_key,
            active: index == 0,
        })
        .collect())
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

pub(crate) fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

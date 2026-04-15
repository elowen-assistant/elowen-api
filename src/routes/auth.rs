//! Authentication, trusted registration, and realtime event routes.

use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use ed25519_dalek::Signer;
use rand::RngCore;
use sqlx::PgPool;
use std::time::Duration;
use ulid::Ulid;

use crate::{
    error::AppError,
    models::{AuthSessionStatus, LoginRequest, RegistrationChallengeResponse},
    services::ui_events::stream_from_broadcast,
    state::AppState,
    trust::{load_orchestrator_signing_key, orchestrator_challenge_payload},
};

pub(crate) async fn get_auth_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthSessionStatus>, AppError> {
    purge_expired_sessions(&state.pool).await?;

    let operator_label =
        current_session_operator_label(&state.pool, &state.auth.cookie_name, &headers).await?;

    Ok(Json(AuthSessionStatus {
        enabled: state.auth.enabled(),
        authenticated: operator_label.is_some() || !state.auth.enabled(),
        operator_label,
    }))
}

pub(crate) async fn registration_challenge(
    State(state): State<AppState>,
) -> Result<Json<RegistrationChallengeResponse>, AppError> {
    let signing_key = load_orchestrator_signing_key(&state)?;
    let challenge_id = Ulid::new().to_string();
    let mut challenge_bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut challenge_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(challenge_bytes);
    let issued_at = Utc::now();
    let payload = orchestrator_challenge_payload(&challenge_id, &challenge, issued_at);
    let signature = signing_key.sign(payload.as_bytes());

    Ok(Json(RegistrationChallengeResponse {
        challenge_id,
        challenge,
        issued_at,
        orchestrator_public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes()),
        signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    }))
}

pub(crate) async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> Result<(StatusCode, HeaderMap, Json<AuthSessionStatus>), AppError> {
    let expected_password =
        state.auth.password.as_ref().ok_or_else(|| {
            AppError::conflict(anyhow::anyhow!("web UI authentication is disabled"))
        })?;

    if request.password != *expected_password {
        return Err(AppError::unauthorized(anyhow::anyhow!("invalid password")));
    }

    purge_expired_sessions(&state.pool).await?;

    let token = new_session_token();
    let expires_at = Utc::now()
        + chrono::Duration::from_std(state.auth.session_ttl)
            .map_err(|error| AppError::from(anyhow::anyhow!(error)))?;

    sqlx::query(
        r#"
        insert into ui_sessions (token, operator_label, expires_at)
        values ($1, $2, $3)
        "#,
    )
    .bind(&token)
    .bind(&state.auth.operator_label)
    .bind(expires_at)
    .execute(&state.pool)
    .await?;

    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie_value(
            &state.auth.cookie_name,
            Some(&token),
            Some(state.auth.session_ttl),
        ))
        .map_err(AppError::from)?,
    );

    Ok((
        StatusCode::OK,
        headers,
        Json(AuthSessionStatus {
            enabled: true,
            authenticated: true,
            operator_label: Some(state.auth.operator_label.clone()),
        }),
    ))
}

pub(crate) async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(StatusCode, HeaderMap, Json<AuthSessionStatus>), AppError> {
    if let Some(token) = session_token_from_headers(&state.auth.cookie_name, &headers) {
        sqlx::query("delete from ui_sessions where token = $1")
            .bind(token)
            .execute(&state.pool)
            .await?;
    }

    let mut response_headers = HeaderMap::new();
    response_headers.insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie_value(&state.auth.cookie_name, None, None))
            .map_err(AppError::from)?,
    );

    Ok((
        StatusCode::OK,
        response_headers,
        Json(AuthSessionStatus {
            enabled: state.auth.enabled(),
            authenticated: false,
            operator_label: None,
        }),
    ))
}

pub(crate) async fn stream_ui_events(
    State(state): State<AppState>,
) -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    stream_from_broadcast(state.ui_events.subscribe())
}

pub(crate) async fn require_authenticated_session(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    if !state.auth.enabled() {
        return Ok(next.run(request).await);
    }

    purge_expired_sessions(&state.pool).await?;

    let authenticated =
        current_session_operator_label(&state.pool, &state.auth.cookie_name, request.headers())
            .await?
            .is_some();

    if !authenticated {
        return Err(AppError::unauthorized(anyhow::anyhow!("sign in required")));
    }

    Ok(next.run(request).await)
}

async fn current_session_operator_label(
    pool: &PgPool,
    cookie_name: &str,
    headers: &HeaderMap,
) -> Result<Option<String>, AppError> {
    let Some(token) = session_token_from_headers(cookie_name, headers) else {
        return Ok(None);
    };

    let operator_label = sqlx::query_scalar::<_, String>(
        r#"
        select operator_label
        from ui_sessions
        where token = $1
          and expires_at > now()
        "#,
    )
    .bind(token)
    .fetch_optional(pool)
    .await?;

    Ok(operator_label)
}

async fn purge_expired_sessions(pool: &PgPool) -> Result<(), AppError> {
    sqlx::query("delete from ui_sessions where expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(())
}

fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn session_cookie_value(
    cookie_name: &str,
    token: Option<&str>,
    max_age: Option<Duration>,
) -> String {
    match (token, max_age) {
        (Some(token), Some(max_age)) => format!(
            "{cookie_name}={token}; HttpOnly; Path=/; SameSite=Strict; Max-Age={}",
            max_age.as_secs()
        ),
        _ => format!("{cookie_name}=; HttpOnly; Path=/; SameSite=Strict; Max-Age=0"),
    }
}

fn session_token_from_headers<'a>(cookie_name: &str, headers: &'a HeaderMap) -> Option<&'a str> {
    let raw_cookie = headers.get(header::COOKIE)?.to_str().ok()?;

    raw_cookie.split(';').find_map(|segment| {
        let trimmed = segment.trim();
        let (name, value) = trimmed.split_once('=')?;
        (name == cookie_name).then_some(value)
    })
}

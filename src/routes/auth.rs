//! Authentication, trusted registration, and realtime event routes.

use anyhow::anyhow;
use axum::{
    Extension, Json,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::Response,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use ed25519_dalek::Signer;
use rand::RngCore;
use std::time::Duration;
use ulid::Ulid;

use crate::{
    auth::{AuthRole, SessionActor},
    error::AppError,
    models::{AuthSessionStatus, LoginRequest, RegistrationChallengeResponse},
    services::ui_events::stream_from_broadcast,
    state::AppState,
    trust::{load_orchestrator_signing_key, orchestrator_challenge_payload},
};

const DISABLED_AUTH_USERNAME: &str = "local-operator";
const DISABLED_AUTH_DISPLAY_NAME: &str = "Local Operator";

pub(crate) async fn get_auth_session(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AuthSessionStatus>, AppError> {
    purge_expired_sessions(&state).await?;

    let actor = if state.auth.enabled() {
        current_session_actor(&state, &headers).await?
    } else {
        None
    };

    Ok(Json(build_auth_session_status(&state, actor)))
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
    let actor = state
        .auth
        .provider
        .authenticate(request.username.as_deref(), &request.password)?;

    purge_expired_sessions(&state).await?;

    let token = new_session_token();
    let now = Utc::now();
    let expires_at = now
        + chrono::Duration::from_std(state.auth.session_ttl)
            .map_err(|error| AppError::from(anyhow!(error)))?;

    sqlx::query(
        r#"
        insert into ui_sessions (
            token,
            operator_label,
            account_username,
            display_name,
            role,
            last_seen_at,
            expires_at
        )
        values ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(&token)
    .bind(&actor.display_name)
    .bind(&actor.username)
    .bind(&actor.display_name)
    .bind(actor_role_value(&actor.role))
    .bind(now)
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
            state.auth.cookie_secure,
        ))
        .map_err(AppError::from)?,
    );

    Ok((
        StatusCode::OK,
        headers,
        Json(build_auth_session_status(&state, Some(actor))),
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
        HeaderValue::from_str(&session_cookie_value(
            &state.auth.cookie_name,
            None,
            None,
            state.auth.cookie_secure,
        ))
        .map_err(AppError::from)?,
    );

    Ok((
        StatusCode::OK,
        response_headers,
        Json(build_auth_session_status(&state, None)),
    ))
}

pub(crate) async fn stream_ui_events(
    State(state): State<AppState>,
) -> axum::response::Sse<
    impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    stream_from_broadcast(state.ui_events.subscribe())
}

pub(crate) async fn require_viewer_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    authorize_request(&state, &mut request, AuthRole::Viewer).await?;
    Ok(next.run(request).await)
}

pub(crate) async fn require_operator_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    authorize_request(&state, &mut request, AuthRole::Operator).await?;
    Ok(next.run(request).await)
}

pub(crate) async fn require_admin_session(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    authorize_request(&state, &mut request, AuthRole::Admin).await?;
    Ok(next.run(request).await)
}

pub(crate) fn require_session_actor(actor: Option<Extension<SessionActor>>) -> SessionActor {
    actor.map(|actor| actor.0).unwrap_or_else(disabled_actor)
}

fn build_auth_session_status(state: &AppState, actor: Option<SessionActor>) -> AuthSessionStatus {
    let enabled = state.auth.enabled();
    let permissions = if enabled {
        actor
            .as_ref()
            .map(SessionActor::permissions)
            .unwrap_or_default()
    } else {
        AuthRole::Admin.permissions()
    };

    AuthSessionStatus {
        enabled,
        auth_mode: state.auth.provider.mode(),
        authenticated: actor.is_some() || !enabled,
        actor,
        permissions,
    }
}

async fn authorize_request(
    state: &AppState,
    request: &mut Request,
    required_role: AuthRole,
) -> Result<(), AppError> {
    purge_expired_sessions(state).await?;

    let actor = if state.auth.enabled() {
        current_session_actor(state, request.headers())
            .await?
            .ok_or_else(|| AppError::unauthorized(anyhow!("sign in required")))?
    } else {
        disabled_actor()
    };

    if !actor.role.allows(&required_role) {
        return Err(AppError::forbidden(anyhow!(
            "the signed-in account is not allowed to perform this action"
        )));
    }

    request.extensions_mut().insert(actor);
    Ok(())
}

async fn current_session_actor(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<SessionActor>, AppError> {
    let Some(token) = session_token_from_headers(&state.auth.cookie_name, headers) else {
        return Ok(None);
    };

    let Some(account_username) = sqlx::query_scalar::<_, String>(
        r#"
        select account_username
        from ui_sessions
        where token = $1
          and expires_at > now()
        "#,
    )
    .bind(token)
    .fetch_optional(&state.pool)
    .await?
    else {
        return Ok(None);
    };

    let Some(actor) = state.auth.provider.resolve_actor(&account_username)? else {
        sqlx::query("delete from ui_sessions where token = $1")
            .bind(token)
            .execute(&state.pool)
            .await?;
        return Ok(None);
    };

    touch_session(state, token, &actor).await?;
    Ok(Some(actor))
}

async fn touch_session(
    state: &AppState,
    token: &str,
    actor: &SessionActor,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update ui_sessions
        set operator_label = $2,
            display_name = $2,
            role = $3,
            last_seen_at = now()
        where token = $1
        "#,
    )
    .bind(token)
    .bind(&actor.display_name)
    .bind(actor_role_value(&actor.role))
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn purge_expired_sessions(state: &AppState) -> Result<(), AppError> {
    sqlx::query("delete from ui_sessions where expires_at <= now()")
        .execute(&state.pool)
        .await?;
    Ok(())
}

fn disabled_actor() -> SessionActor {
    SessionActor {
        username: DISABLED_AUTH_USERNAME.to_string(),
        display_name: DISABLED_AUTH_DISPLAY_NAME.to_string(),
        role: AuthRole::Admin,
    }
}

fn actor_role_value(role: &AuthRole) -> &'static str {
    match role {
        AuthRole::Viewer => "viewer",
        AuthRole::Operator => "operator",
        AuthRole::Admin => "admin",
    }
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
    secure: bool,
) -> String {
    let secure = if secure { "; Secure" } else { "" };

    match (token, max_age) {
        (Some(token), Some(max_age)) => format!(
            "{cookie_name}={token}; HttpOnly; Path=/; SameSite=Strict{secure}; Max-Age={}",
            max_age.as_secs()
        ),
        _ => format!("{cookie_name}=; HttpOnly; Path=/; SameSite=Strict{secure}; Max-Age=0"),
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

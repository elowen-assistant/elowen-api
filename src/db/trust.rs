//! Trust lifecycle persistence helpers.

use sqlx::{PgPool, types::Json as SqlxJson};

use crate::{
    auth::{AuthRole, SessionActor},
    error::AppError,
    models::{DeviceTrustEventRecord, OrchestratorSignerStateRecord},
};

pub(crate) struct NewDeviceTrustEvent<'a> {
    pub(crate) device_id: &'a str,
    pub(crate) event_type: &'a str,
    pub(crate) actor: Option<&'a SessionActor>,
    pub(crate) reason: Option<&'a str>,
    pub(crate) previous_status: Option<&'a str>,
    pub(crate) next_status: Option<&'a str>,
    pub(crate) edge_public_key: Option<&'a str>,
    pub(crate) previous_edge_public_key: Option<&'a str>,
    pub(crate) orchestrator_key_id: Option<&'a str>,
    pub(crate) orchestrator_public_key: Option<&'a str>,
    pub(crate) payload_json: serde_json::Value,
}

pub(crate) async fn insert_device_trust_event(
    pool: &PgPool,
    event: NewDeviceTrustEvent<'_>,
) -> Result<DeviceTrustEventRecord, AppError> {
    let id = ulid::Ulid::new().to_string();
    let actor_role = event.actor.map(|actor| actor_role_value(&actor.role));
    let record = sqlx::query_as::<_, DeviceTrustEventRow>(
        r#"
        insert into device_trust_events (
            id,
            device_id,
            event_type,
            actor_username,
            actor_display_name,
            actor_role,
            reason,
            previous_status,
            next_status,
            edge_public_key,
            previous_edge_public_key,
            orchestrator_key_id,
            orchestrator_public_key,
            payload_json
        )
        values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        returning id, device_id, event_type, actor_username, actor_display_name, actor_role,
                  reason, previous_status, next_status, edge_public_key,
                  previous_edge_public_key, orchestrator_key_id, orchestrator_public_key,
                  payload_json, created_at
        "#,
    )
    .bind(id)
    .bind(event.device_id)
    .bind(event.event_type)
    .bind(event.actor.map(|actor| actor.username.clone()))
    .bind(event.actor.map(|actor| actor.display_name.clone()))
    .bind(actor_role)
    .bind(event.reason)
    .bind(event.previous_status)
    .bind(event.next_status)
    .bind(event.edge_public_key)
    .bind(event.previous_edge_public_key)
    .bind(event.orchestrator_key_id)
    .bind(event.orchestrator_public_key)
    .bind(SqlxJson(event.payload_json))
    .fetch_one(pool)
    .await?;

    Ok(record.into())
}

pub(crate) async fn list_device_trust_events(
    pool: &PgPool,
    device_id: &str,
) -> Result<Vec<DeviceTrustEventRecord>, AppError> {
    let rows = sqlx::query_as::<_, DeviceTrustEventRow>(
        r#"
        select id, device_id, event_type, actor_username, actor_display_name, actor_role,
               reason, previous_status, next_status, edge_public_key,
               previous_edge_public_key, orchestrator_key_id, orchestrator_public_key,
               payload_json, created_at
        from device_trust_events
        where device_id = $1
        order by created_at desc, id desc
        limit 100
        "#,
    )
    .bind(device_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

pub(crate) async fn upsert_signer_state(
    pool: &PgPool,
    key_id: &str,
    public_key: &str,
    status: &str,
    actor: Option<&SessionActor>,
    reason: Option<&str>,
) -> Result<OrchestratorSignerStateRecord, AppError> {
    let actor_role = actor.map(|actor| actor_role_value(&actor.role));
    let row = sqlx::query_as::<_, OrchestratorSignerStateRow>(
        r#"
        insert into orchestrator_signer_states (
            public_key,
            key_id,
            status,
            actor_username,
            actor_display_name,
            actor_role,
            reason,
            staged_at,
            activated_at,
            retired_at
        )
        values (
            $1, $2, $3, $4, $5, $6, $7,
            case when $3 = 'staged' then now() else null end,
            case when $3 = 'active' then now() else null end,
            case when $3 = 'retired' then now() else null end
        )
        on conflict (public_key) do update set
            key_id = excluded.key_id,
            status = excluded.status,
            actor_username = excluded.actor_username,
            actor_display_name = excluded.actor_display_name,
            actor_role = excluded.actor_role,
            reason = excluded.reason,
            staged_at = case when excluded.status = 'staged' then coalesce(orchestrator_signer_states.staged_at, now()) else orchestrator_signer_states.staged_at end,
            activated_at = case when excluded.status = 'active' then now() else orchestrator_signer_states.activated_at end,
            retired_at = case when excluded.status = 'retired' then now() else orchestrator_signer_states.retired_at end,
            updated_at = now()
        returning key_id, public_key, status, actor_username, actor_display_name, actor_role,
                  reason, staged_at, activated_at, retired_at, updated_at
        "#,
    )
    .bind(public_key)
    .bind(key_id)
    .bind(status)
    .bind(actor.map(|actor| actor.username.clone()))
    .bind(actor.map(|actor| actor.display_name.clone()))
    .bind(actor_role)
    .bind(reason)
    .fetch_one(pool)
    .await?;

    Ok(row.into())
}

pub(crate) async fn list_signer_states(
    pool: &PgPool,
) -> Result<Vec<OrchestratorSignerStateRecord>, AppError> {
    let rows = sqlx::query_as::<_, OrchestratorSignerStateRow>(
        r#"
        select key_id, public_key, status, actor_username, actor_display_name, actor_role,
               reason, staged_at, activated_at, retired_at, updated_at
        from orchestrator_signer_states
        order by
            case status when 'active' then 0 when 'staged' then 1 else 2 end,
            updated_at desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(Into::into).collect())
}

fn actor_role_value(role: &AuthRole) -> &'static str {
    match role {
        AuthRole::Viewer => "viewer",
        AuthRole::Operator => "operator",
        AuthRole::Admin => "admin",
    }
}

#[derive(sqlx::FromRow)]
struct DeviceTrustEventRow {
    id: String,
    device_id: String,
    event_type: String,
    actor_username: Option<String>,
    actor_display_name: Option<String>,
    actor_role: Option<String>,
    reason: Option<String>,
    previous_status: Option<String>,
    next_status: Option<String>,
    edge_public_key: Option<String>,
    previous_edge_public_key: Option<String>,
    orchestrator_key_id: Option<String>,
    orchestrator_public_key: Option<String>,
    payload_json: SqlxJson<serde_json::Value>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<DeviceTrustEventRow> for DeviceTrustEventRecord {
    fn from(row: DeviceTrustEventRow) -> Self {
        Self {
            id: row.id,
            device_id: row.device_id,
            event_type: row.event_type,
            actor_username: row.actor_username,
            actor_display_name: row.actor_display_name,
            actor_role: row.actor_role,
            reason: row.reason,
            previous_status: row.previous_status,
            next_status: row.next_status,
            edge_public_key: row.edge_public_key,
            previous_edge_public_key: row.previous_edge_public_key,
            orchestrator_key_id: row.orchestrator_key_id,
            orchestrator_public_key: row.orchestrator_public_key,
            payload_json: row.payload_json.0,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct OrchestratorSignerStateRow {
    key_id: String,
    public_key: String,
    status: String,
    actor_username: Option<String>,
    actor_display_name: Option<String>,
    actor_role: Option<String>,
    reason: Option<String>,
    staged_at: Option<chrono::DateTime<chrono::Utc>>,
    activated_at: Option<chrono::DateTime<chrono::Utc>>,
    retired_at: Option<chrono::DateTime<chrono::Utc>>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

impl From<OrchestratorSignerStateRow> for OrchestratorSignerStateRecord {
    fn from(row: OrchestratorSignerStateRow) -> Self {
        let active = row.status == "active";
        Self {
            key_id: row.key_id,
            public_key: row.public_key,
            status: row.status,
            active,
            actor_username: row.actor_username,
            actor_display_name: row.actor_display_name,
            actor_role: row.actor_role,
            reason: row.reason,
            staged_at: row.staged_at,
            activated_at: row.activated_at,
            retired_at: row.retired_at,
            updated_at: row.updated_at,
        }
    }
}

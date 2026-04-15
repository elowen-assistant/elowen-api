//! Thread and message persistence helpers.

use anyhow::anyhow;
use serde_json::{Value, json};
use sqlx::{PgPool, types::Json as SqlxJson};
use ulid::Ulid;

use crate::{
    error::AppError,
    models::{MessageRecord, ThreadRecord, ThreadSummary},
};

pub(crate) async fn list_threads(pool: &PgPool) -> Result<Vec<ThreadSummary>, AppError> {
    let threads = sqlx::query_as::<_, ThreadSummary>(
        r#"
        select
            t.id,
            t.title,
            t.status,
            t.current_summary_id,
            t.created_at,
            t.updated_at,
            coalesce(count(m.id), 0)::bigint as message_count
        from threads t
        left join messages m on m.thread_id = t.id
        group by t.id
        order by t.updated_at desc, t.created_at desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(threads)
}

pub(crate) async fn load_thread_record(
    pool: &PgPool,
    thread_id: &str,
) -> Result<ThreadRecord, AppError> {
    sqlx::query_as::<_, ThreadRecord>(
        r#"
        select id, title, status, current_summary_id, created_at, updated_at
        from threads
        where id = $1
        "#,
    )
    .bind(thread_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow!("thread not found")))
}

pub(crate) async fn create_thread(pool: &PgPool, title: &str) -> Result<ThreadRecord, AppError> {
    let thread_id = Ulid::new().to_string();
    sqlx::query_as::<_, ThreadRecord>(
        r#"
        insert into threads (id, title, status)
        values ($1, $2, 'open')
        returning id, title, status, current_summary_id, created_at, updated_at
        "#,
    )
    .bind(thread_id)
    .bind(title)
    .fetch_one(pool)
    .await
    .map_err(AppError::from)
}

pub(crate) async fn load_thread_messages(
    pool: &PgPool,
    thread_id: &str,
) -> Result<Vec<MessageRecord>, AppError> {
    let messages = sqlx::query_as::<_, MessageRecord>(
        r#"
        select id, thread_id, role, content, status, payload_json, created_at, updated_at
        from messages
        where thread_id = $1
        order by created_at asc, id asc
        "#,
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;

    Ok(messages)
}

pub(crate) async fn load_thread_message(
    pool: &PgPool,
    thread_id: &str,
    message_id: &str,
) -> Result<MessageRecord, AppError> {
    sqlx::query_as::<_, MessageRecord>(
        r#"
        select id, thread_id, role, content, status, payload_json, created_at, updated_at
        from messages
        where thread_id = $1 and id = $2
        "#,
    )
    .bind(thread_id)
    .bind(message_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow!("thread message not found")))
}

pub(crate) async fn ensure_thread_exists(pool: &PgPool, thread_id: &str) -> Result<(), AppError> {
    let count = sqlx::query_scalar::<_, i64>("select count(*)::bigint from threads where id = $1")
        .bind(thread_id)
        .fetch_one(pool)
        .await?;

    if count == 0 {
        return Err(AppError::not_found(anyhow!("thread not found")));
    }

    Ok(())
}

pub(crate) async fn insert_thread_message(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
) -> Result<MessageRecord, AppError> {
    insert_thread_message_with_status(pool, thread_id, role, content, "committed").await
}

pub(crate) async fn insert_thread_message_with_status(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
) -> Result<MessageRecord, AppError> {
    insert_thread_message_with_status_and_payload(pool, thread_id, role, content, status, json!({}))
        .await
}

pub(crate) async fn insert_thread_message_with_status_and_payload(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
    payload_json: Value,
) -> Result<MessageRecord, AppError> {
    let message_id = Ulid::new().to_string();
    let message = sqlx::query_as::<_, MessageRecord>(
        r#"
        insert into messages (id, thread_id, role, content, status, payload_json)
        values ($1, $2, $3, $4, $5, $6)
        returning id, thread_id, role, content, status, payload_json, created_at, updated_at
        "#,
    )
    .bind(&message_id)
    .bind(thread_id)
    .bind(role)
    .bind(content)
    .bind(status)
    .bind(SqlxJson(payload_json))
    .fetch_one(pool)
    .await?;

    Ok(message)
}

pub(crate) async fn maybe_insert_thread_message_with_status_and_payload(
    pool: &PgPool,
    thread_id: &str,
    role: &str,
    content: &str,
    status: &str,
    payload_json: Value,
) -> Result<Option<MessageRecord>, AppError> {
    let existing = sqlx::query_scalar::<_, String>(
        "select id from messages where thread_id = $1 and status = $2 limit 1",
    )
    .bind(thread_id)
    .bind(status)
    .fetch_optional(pool)
    .await?;

    if existing.is_some() {
        return Ok(None);
    }

    Ok(Some(
        insert_thread_message_with_status_and_payload(
            pool,
            thread_id,
            role,
            content,
            status,
            payload_json,
        )
        .await?,
    ))
}

pub(crate) async fn touch_thread(pool: &PgPool, thread_id: &str) -> Result<(), AppError> {
    sqlx::query("update threads set updated_at = now() where id = $1")
        .bind(thread_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub(crate) async fn touch_thread_for_job(pool: &PgPool, job_id: &str) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update threads
        set updated_at = now()
        where id = (select thread_id from jobs where id = $1)
        "#,
    )
    .bind(job_id)
    .execute(pool)
    .await?;
    Ok(())
}

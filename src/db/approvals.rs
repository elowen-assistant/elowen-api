//! Approval persistence helpers.

use anyhow::anyhow;
use sqlx::PgPool;
use ulid::Ulid;

use crate::{error::AppError, models::ApprovalRecord};

pub(crate) async fn load_job_approvals(
    pool: &PgPool,
    job_id: &str,
) -> Result<Vec<ApprovalRecord>, AppError> {
    let approvals = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        select
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolved_by_display_name,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        from approvals
        where job_id = $1
        order by created_at desc, id desc
        "#,
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?;

    Ok(approvals)
}

pub(crate) async fn load_approval_record(
    pool: &PgPool,
    approval_id: &str,
) -> Result<ApprovalRecord, AppError> {
    sqlx::query_as::<_, ApprovalRecord>(
        r#"
        select
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolved_by_display_name,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        from approvals
        where id = $1
        "#,
    )
    .bind(approval_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow!("approval not found")))
}

pub(crate) async fn resolve_approval_record(
    pool: &PgPool,
    approval_id: &str,
    status: &str,
    resolved_by: Option<String>,
    resolved_by_display_name: Option<String>,
    reason: Option<String>,
) -> Result<ApprovalRecord, AppError> {
    let approval = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        update approvals
        set status = $2,
            resolved_by = $3,
            resolved_by_display_name = $4,
            resolution_reason = $5,
            resolved_at = now(),
            updated_at = now()
        where id = $1
          and status = 'pending'
        returning
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolved_by_display_name,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        "#,
    )
    .bind(approval_id)
    .bind(status)
    .bind(resolved_by)
    .bind(resolved_by_display_name)
    .bind(reason)
    .fetch_optional(pool)
    .await?;

    approval.ok_or_else(|| {
        AppError::conflict(anyhow!("approval could not be updated from pending state"))
    })
}

pub(crate) async fn reset_approval_to_pending(
    pool: &PgPool,
    approval_id: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update approvals
        set status = 'pending',
            resolved_by = null,
            resolved_by_display_name = null,
            resolution_reason = null,
            resolved_at = null,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(approval_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn upsert_pending_push_approval(
    pool: &PgPool,
    thread_id: &str,
    job_id: &str,
    summary: &str,
) -> Result<ApprovalRecord, AppError> {
    if let Some(approval) = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        select
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolved_by_display_name,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        from approvals
        where job_id = $1
          and action_type = 'push'
          and status = 'pending'
        order by created_at desc, id desc
        limit 1
        "#,
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(approval);
    }

    let approval_id = Ulid::new().to_string();
    let approval = sqlx::query_as::<_, ApprovalRecord>(
        r#"
        insert into approvals (id, thread_id, job_id, action_type, status, summary)
        values ($1, $2, $3, 'push', 'pending', $4)
        returning
            id,
            thread_id,
            job_id,
            action_type,
            status,
            summary,
            resolved_by,
            resolved_by_display_name,
            resolution_reason,
            created_at,
            resolved_at,
            updated_at
        "#,
    )
    .bind(&approval_id)
    .bind(thread_id)
    .bind(job_id)
    .bind(summary)
    .fetch_one(pool)
    .await?;

    Ok(approval)
}

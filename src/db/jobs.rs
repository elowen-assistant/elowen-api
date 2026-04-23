//! Job and summary persistence helpers.

use anyhow::anyhow;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, types::Json as SqlxJson};
use ulid::Ulid;

use crate::{
    error::AppError,
    models::{
        ApprovalRecord, JobDetail, JobEventRecord, JobEventRow, JobRecord, JobStateRow,
        SummaryRecord,
    },
};

pub(crate) async fn list_jobs(pool: &PgPool) -> Result<Vec<JobRecord>, AppError> {
    let jobs = sqlx::query_as::<_, JobRecord>(
        r#"
        select
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            target_kind,
            status,
            result,
            failure_class,
            repo_name,
            capability_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        from jobs
        order by created_at desc, id desc
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(jobs)
}

pub(crate) async fn load_thread_jobs(
    pool: &PgPool,
    thread_id: &str,
) -> Result<Vec<JobRecord>, AppError> {
    let jobs = sqlx::query_as::<_, JobRecord>(
        r#"
        select
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            target_kind,
            status,
            result,
            failure_class,
            repo_name,
            capability_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        from jobs
        where thread_id = $1
        order by created_at desc, id desc
        "#,
    )
    .bind(thread_id)
    .fetch_all(pool)
    .await?;

    Ok(jobs)
}

pub(crate) async fn load_job_record(pool: &PgPool, job_id: &str) -> Result<JobRecord, AppError> {
    sqlx::query_as::<_, JobRecord>(
        r#"
        select
            id,
            short_id,
            correlation_id,
            thread_id,
            title,
            target_kind,
            status,
            result,
            failure_class,
            repo_name,
            capability_name,
            device_id,
            branch_name,
            base_branch,
            parent_job_id,
            created_at,
            updated_at,
            completed_at
        from jobs
        where id = $1
        "#,
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow!("job not found")))
}

pub(crate) async fn load_job_correlation_id(
    pool: &PgPool,
    job_id: &str,
) -> Result<String, AppError> {
    sqlx::query_scalar::<_, String>("select correlation_id from jobs where id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::not_found(anyhow!("job not found")))
}

pub(crate) async fn load_job_thread_id(pool: &PgPool, job_id: &str) -> Result<String, AppError> {
    sqlx::query_scalar::<_, String>("select thread_id from jobs where id = $1")
        .bind(job_id)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| AppError::not_found(anyhow!("job not found")))
}

pub(crate) async fn load_current_job_summary(
    pool: &PgPool,
    job_id: &str,
) -> Result<Option<SummaryRecord>, AppError> {
    let summary_id = sqlx::query_scalar::<_, Option<String>>(
        "select current_summary_id from jobs where id = $1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .flatten();

    match summary_id {
        Some(summary_id) => Ok(Some(load_summary(pool, &summary_id).await?)),
        None => Ok(None),
    }
}

pub(crate) async fn load_job_state_row(
    pool: &PgPool,
    job_id: &str,
) -> Result<JobStateRow, AppError> {
    sqlx::query_as::<_, JobStateRow>(
        r#"
        select current_summary_id, execution_report_json
        from jobs
        where id = $1
        "#,
    )
    .bind(job_id)
    .fetch_one(pool)
    .await
    .map_err(AppError::from)
}

pub(crate) async fn load_job_events(
    pool: &PgPool,
    job_id: &str,
) -> Result<Vec<JobEventRecord>, AppError> {
    let events = sqlx::query_as::<_, JobEventRow>(
        r#"
        select id, job_id, correlation_id, event_type, payload_json, created_at
        from job_events
        where job_id = $1
        order by created_at asc, id asc
        "#,
    )
    .bind(job_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(Into::into)
    .collect();

    Ok(events)
}

pub(crate) async fn build_job_detail(
    pool: &PgPool,
    job: JobRecord,
    approvals: Vec<ApprovalRecord>,
    related_notes: Vec<crate::models::NoteRecord>,
) -> Result<JobDetail, AppError> {
    let job_state = load_job_state_row(pool, &job.id).await?;
    let summary = match job_state.current_summary_id {
        Some(summary_id) => Some(load_summary(pool, &summary_id).await?),
        None => None,
    };
    let events = load_job_events(pool, &job.id).await?;

    Ok(JobDetail {
        job,
        execution_report_json: job_state.execution_report_json.0,
        summary,
        approvals,
        related_notes,
        events,
    })
}

pub(crate) async fn insert_job_event(
    pool: &PgPool,
    job_id: &str,
    correlation_id: &str,
    event_type: &str,
    payload_json: Value,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        insert into job_events (id, job_id, correlation_id, event_type, payload_json)
        values ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(Ulid::new().to_string())
    .bind(job_id)
    .bind(correlation_id)
    .bind(event_type)
    .bind(SqlxJson(payload_json))
    .execute(pool)
    .await?;

    Ok(())
}

pub(crate) async fn update_job_state(
    pool: &PgPool,
    job_id: &str,
    status: &str,
    result: Option<String>,
    failure_class: Option<String>,
    completed_at: Option<DateTime<Utc>>,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update jobs
        set status = $2,
            result = $3,
            failure_class = $4,
            completed_at = $5,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(status)
    .bind(result)
    .bind(failure_class)
    .bind(completed_at)
    .execute(pool)
    .await?;

    Ok(())
}

pub(crate) async fn update_job_status_only(
    pool: &PgPool,
    job_id: &str,
    status: &str,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update jobs
        set status = $2,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(status)
    .execute(pool)
    .await?;

    Ok(())
}

pub(crate) async fn update_job_execution_report(
    pool: &PgPool,
    job_id: &str,
    execution_report_json: Value,
) -> Result<(), AppError> {
    sqlx::query(
        r#"
        update jobs
        set execution_report_json = $2,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(SqlxJson(execution_report_json))
    .execute(pool)
    .await?;

    Ok(())
}

pub(crate) async fn store_job_summary(
    pool: &PgPool,
    job_id: &str,
    content: &str,
) -> Result<SummaryRecord, AppError> {
    let version = sqlx::query_scalar::<_, i64>(
        r#"
        select coalesce(max(version), 0)::bigint + 1
        from summaries
        where scope = 'job' and source_id = $1
        "#,
    )
    .bind(job_id)
    .fetch_one(pool)
    .await?;
    let summary_id = Ulid::new().to_string();

    let summary = sqlx::query_as::<_, SummaryRecord>(
        r#"
        insert into summaries (id, scope, source_id, version, content)
        values ($1, 'job', $2, $3, $4)
        returning id, scope, source_id, version, content, created_at
        "#,
    )
    .bind(&summary_id)
    .bind(job_id)
    .bind(version as i32)
    .bind(content)
    .fetch_one(pool)
    .await?;

    sqlx::query(
        r#"
        update jobs
        set current_summary_id = $2,
            updated_at = now()
        where id = $1
        "#,
    )
    .bind(job_id)
    .bind(&summary_id)
    .execute(pool)
    .await?;

    Ok(summary)
}

pub(crate) async fn load_summary(
    pool: &PgPool,
    summary_id: &str,
) -> Result<SummaryRecord, AppError> {
    sqlx::query_as::<_, SummaryRecord>(
        r#"
        select id, scope, source_id, version, content, created_at
        from summaries
        where id = $1
        "#,
    )
    .bind(summary_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| AppError::not_found(anyhow!("summary not found")))
}

pub(crate) async fn load_job_execution_report(
    pool: &PgPool,
    job_id: &str,
) -> Result<Option<Value>, AppError> {
    let report = sqlx::query_scalar::<_, SqlxJson<Value>>(
        "select execution_report_json from jobs where id = $1",
    )
    .bind(job_id)
    .fetch_optional(pool)
    .await?
    .map(|value| value.0);

    Ok(report.filter(|value| value != &serde_json::json!({})))
}

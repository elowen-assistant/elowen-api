//! Job lifecycle persistence and thread-reply helpers.

use anyhow::Context;
use async_nats::Client as NatsClient;
use futures_util::StreamExt;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::broadcast;
use tracing::{info, warn};

use crate::{
    db::{
        approvals::upsert_pending_push_approval,
        jobs::{
            insert_job_event, load_current_job_summary, load_job_execution_report, load_job_record,
            load_job_thread_id, store_job_summary, update_job_execution_report, update_job_state,
        },
        threads::{
            maybe_insert_thread_message_with_status_and_payload, touch_thread, touch_thread_for_job,
        },
    },
    error::AppError,
    formatting::{
        execution_report_changed_entries, execution_report_last_message, execution_report_status,
        format_failure_reply, format_failure_result_summary, format_read_only_success_reply,
        format_success_reply, format_success_without_push_reply, primary_result_excerpt,
        sanitize_chat_result_text,
    },
    models::{ExecutionIntent, JobLifecycleEvent, UiEvent},
    services::ui_events::{job_ui_event, publish_ui_event_to},
};

pub(crate) async fn consume_job_lifecycle_events(
    pool: PgPool,
    nats: NatsClient,
    ui_events: broadcast::Sender<UiEvent>,
) -> anyhow::Result<()> {
    let subject = "elowen.jobs.events".to_string();
    let mut subscription = nats
        .subscribe(subject.clone())
        .await
        .context("failed to subscribe to job lifecycle events")?;

    info!(subject = %subject, "consuming job lifecycle events");

    while let Some(message) = subscription.next().await {
        let event: JobLifecycleEvent = match serde_json::from_slice(&message.payload) {
            Ok(event) => event,
            Err(error) => {
                warn!(error = %error, "failed to decode job lifecycle event");
                continue;
            }
        };

        if let Err(error) = persist_job_lifecycle_event(&pool, &ui_events, &event).await {
            warn!(
                job_id = %event.job_id,
                correlation_id = %event.correlation_id,
                event_type = %event.event_type,
                error = ?error,
                "failed to persist job lifecycle event"
            );
        } else {
            info!(
                job_id = %event.job_id,
                correlation_id = %event.correlation_id,
                event_type = %event.event_type,
                "persisted job lifecycle event"
            );
        }
    }

    Ok(())
}

pub(crate) async fn persist_job_lifecycle_event(
    pool: &PgPool,
    ui_events: &broadcast::Sender<UiEvent>,
    event: &JobLifecycleEvent,
) -> Result<(), AppError> {
    let payload_value = event.payload_json.clone().unwrap_or_else(|| json!({}));
    let mut payload = serde_json::Map::new();
    payload.insert(
        "correlation_id".to_string(),
        Value::String(event.correlation_id.clone()),
    );
    payload.insert(
        "device_id".to_string(),
        Value::String(event.device_id.clone()),
    );

    if let Some(status) = &event.status {
        payload.insert("status".to_string(), Value::String(status.clone()));
    }

    if let Some(result) = &event.result {
        payload.insert("result".to_string(), Value::String(result.clone()));
    }

    if let Some(failure_class) = &event.failure_class {
        payload.insert(
            "failure_class".to_string(),
            Value::String(failure_class.clone()),
        );
    }

    if let Some(worktree_path) = &event.worktree_path {
        payload.insert(
            "worktree_path".to_string(),
            Value::String(worktree_path.clone()),
        );
    }

    if let Some(detail) = &event.detail {
        payload.insert("detail".to_string(), Value::String(detail.clone()));
    }

    payload.insert("payload".to_string(), payload_value.clone());

    insert_job_event(
        pool,
        &event.job_id,
        &event.correlation_id,
        &event.event_type,
        Value::Object(payload),
    )
    .await?;

    if let Some(status) = event.status.as_deref() {
        let completed_at =
            matches!(status, "completed" | "failed" | "cancelled").then_some(event.created_at);
        update_job_state(
            pool,
            &event.job_id,
            status,
            event.result.clone(),
            event.failure_class.clone(),
            completed_at,
        )
        .await?;
    }

    if let Some(summary_markdown) = payload_value
        .get("summary_markdown")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let _ = store_job_summary(pool, &event.job_id, summary_markdown).await?;
    }

    if let Some(execution_report) = payload_value.get("execution_report") {
        update_job_execution_report(pool, &event.job_id, execution_report.clone()).await?;
    }

    if event.event_type == "job.awaiting_approval" {
        let thread_id = sqlx::query_scalar::<_, String>("select thread_id from jobs where id = $1")
            .bind(&event.job_id)
            .fetch_one(pool)
            .await?;
        let approval_summary = payload_value
            .get("summary")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("Approve push for completed job");
        let approval =
            upsert_pending_push_approval(pool, &thread_id, &event.job_id, approval_summary).await?;
        insert_job_event(
            pool,
            &event.job_id,
            &event.correlation_id,
            "approval.requested",
            json!({
                "correlation_id": event.correlation_id.clone(),
                "approval_id": approval.id,
                "action_type": approval.action_type,
                "summary": approval.summary,
            }),
        )
        .await?;
    }

    maybe_post_thread_assistant_reply(pool, event).await?;
    touch_thread_for_job(pool, &event.job_id).await?;
    let thread_id = load_job_thread_id(pool, &event.job_id).await?;
    publish_ui_event_to(
        ui_events,
        job_ui_event(&thread_id, &event.job_id, Some(&event.device_id)),
    );
    Ok(())
}

async fn maybe_post_thread_assistant_reply(
    pool: &PgPool,
    event: &JobLifecycleEvent,
) -> Result<(), AppError> {
    let Some((status_marker, content, payload_json)) =
        build_thread_assistant_reply(pool, event).await?
    else {
        return Ok(());
    };

    let thread_id = sqlx::query_scalar::<_, String>("select thread_id from jobs where id = $1")
        .bind(&event.job_id)
        .fetch_one(pool)
        .await?;

    let inserted = maybe_insert_thread_message_with_status_and_payload(
        pool,
        &thread_id,
        "assistant",
        &content,
        &status_marker,
        payload_json,
    )
    .await?;

    if inserted.is_some() {
        touch_thread(pool, &thread_id).await?;
    }

    Ok(())
}

async fn build_thread_assistant_reply(
    pool: &PgPool,
    event: &JobLifecycleEvent,
) -> Result<Option<(String, String, Value)>, AppError> {
    let job = load_job_record(pool, &event.job_id).await?;
    let summary = load_current_job_summary(pool, &event.job_id).await?;
    let execution_report = load_job_execution_report(pool, &event.job_id).await?;
    let build_status = execution_report
        .as_ref()
        .and_then(|report| execution_report_status(report, "build"));
    let test_status = execution_report
        .as_ref()
        .and_then(|report| execution_report_status(report, "test"));
    let changed_entries = execution_report
        .as_ref()
        .and_then(execution_report_changed_entries);
    let last_message = execution_report
        .as_ref()
        .and_then(|report| execution_report_last_message(report));
    let sanitized_last_message = last_message.map(sanitize_chat_result_text);
    let execution_intent = execution_report
        .as_ref()
        .and_then(|report| report.get("execution_intent").cloned())
        .and_then(|value| serde_json::from_value::<ExecutionIntent>(value).ok());
    let detail = event
        .detail
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let make_completion_payload = |details: String| {
        json!({
            "job_result": {
                "job_id": job.id,
                "job_short_id": job.short_id,
                "details": details,
            }
        })
    };

    let reply = match event.event_type.as_str() {
        "job.started" => Some((
            format!("job_event:{}:started", event.job_id),
            format!(
                "I started working on job `{}` for {} `{}` on device `{}`.",
                job.short_id,
                match job.target_kind_enum() {
                    crate::models::JobTargetKind::Repository => "repository",
                    crate::models::JobTargetKind::Capability => "capability",
                },
                job.target_name(),
                event.device_id
            ),
            json!({}),
        )),
        "job.push_started" => Some((
            format!("job_event:{}:push_started", event.job_id),
            format!(
                "Push approval landed for job `{}`. I started pushing branch `{}` from device `{}`.",
                job.short_id,
                job.branch_name.as_deref().unwrap_or("unknown"),
                event.device_id
            ),
            json!({}),
        )),
        "job.push_completed" => Some((
            format!("job_event:{}:push_completed", event.job_id),
            format!(
                "I finished pushing branch `{}` for job `{}` to `origin`.",
                job.branch_name.as_deref().unwrap_or("unknown"),
                job.short_id
            ),
            json!({}),
        )),
        "job.awaiting_approval" => {
            let approval_summary = event
                .payload_json
                .as_ref()
                .and_then(|payload| payload.get("summary"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("Push is waiting for approval.");
            let details = format_success_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                sanitized_last_message.as_deref(),
                approval_summary,
            );
            let content = if let Some(last_message) = sanitized_last_message.as_deref() {
                format!(
                    "{}\n\nPush approval is pending.",
                    primary_result_excerpt(last_message)
                )
            } else {
                format!(
                    "Job `{}` finished its main work and is waiting for push approval.",
                    job.short_id
                )
            };

            Some((
                format!("job_event:{}:awaiting_approval", event.job_id),
                content,
                make_completion_payload(details),
            ))
        }
        "job.completed"
            if event.result.as_deref() == Some("success")
                && matches!(execution_intent, Some(ExecutionIntent::ReadOnly)) =>
        {
            let details = format_read_only_success_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                sanitized_last_message.as_deref(),
            );
            Some((
                format!("job_event:{}:completed", event.job_id),
                sanitized_last_message
                    .as_deref()
                    .map(primary_result_excerpt)
                    .unwrap_or_else(|| {
                        format!("Read-only job `{}` finished successfully.", job.short_id)
                    }),
                make_completion_payload(details),
            ))
        }
        "job.completed"
            if event.result.as_deref() == Some("success")
                && !event
                    .payload_json
                    .as_ref()
                    .and_then(|payload| payload.get("push_required"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false) =>
        {
            let details = format_success_without_push_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                sanitized_last_message.as_deref(),
            );
            Some((
                format!("job_event:{}:completed", event.job_id),
                sanitized_last_message
                    .as_deref()
                    .map(primary_result_excerpt)
                    .unwrap_or_else(|| {
                        format!(
                            "Job `{}` finished successfully. No push approval was required.",
                            job.short_id
                        )
                    }),
                make_completion_payload(details),
            ))
        }
        "job.completed" if event.result.as_deref() != Some("success") => {
            let details = format_failure_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                detail,
                summary.as_ref(),
            );
            Some((
                format!("job_event:{}:completed", event.job_id),
                format_failure_result_summary(detail, summary.as_ref()),
                make_completion_payload(details),
            ))
        }
        "job.failed" => {
            let details = format_failure_reply(
                &job,
                build_status,
                test_status,
                changed_entries,
                detail,
                summary.as_ref(),
            );
            Some((
                format!("job_event:{}:failed", event.job_id),
                format_failure_result_summary(detail, summary.as_ref()),
                make_completion_payload(details),
            ))
        }
        _ => None,
    };

    Ok(reply)
}

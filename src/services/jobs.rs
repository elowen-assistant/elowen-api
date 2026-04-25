//! Job orchestration helpers used by route handlers.

use anyhow::{Context, anyhow};
use chrono::Utc;
use serde_json::json;
use std::collections::BTreeSet;
use ulid::Ulid;

use crate::{
    db::{
        approvals::load_job_approvals,
        devices::{list_devices, load_device_row},
        jobs::{build_job_detail, insert_job_event, load_job_record},
    },
    error::AppError,
    formatting::{sanitize_optional_string, slugify},
    models::{
        ApprovalRecord, CreateJobRequest, DeviceRepository, JobApprovalCommand, JobDetail,
        JobDispatchMessage, JobRecord, JobTargetKind,
    },
    services::notes::load_related_job_notes,
    state::AppState,
};

use super::conversation::infer_execution_intent;

pub(crate) async fn create_job_record(
    state: &AppState,
    thread_id: &str,
    request: &CreateJobRequest,
) -> Result<JobRecord, AppError> {
    let title = request.title.trim();
    let prompt = request.prompt.trim();
    let target_kind = request.target_kind.clone();
    let target_name = sanitize_optional_string(request.target_name.clone());
    let base_branch = sanitize_optional_string(request.base_branch.clone());
    let execution_intent = request
        .execution_intent
        .clone()
        .unwrap_or_else(|| infer_execution_intent(prompt));

    if title.is_empty() {
        return Err(AppError::bad_request(anyhow!("job title is required")));
    }

    if prompt.is_empty() {
        return Err(AppError::bad_request(anyhow!("job prompt is required")));
    }

    let target_name = target_name
        .clone()
        .ok_or_else(|| AppError::bad_request(anyhow!("target name is required")))?;
    let (repo_name, capability_name) = match &target_kind {
        JobTargetKind::Repository => (Some(target_name.clone()), None),
        JobTargetKind::Capability => (None, Some(target_name.clone())),
    };
    let base_branch = match &target_kind {
        JobTargetKind::Repository => Some(base_branch.unwrap_or_else(|| "main".to_string())),
        JobTargetKind::Capability => None,
    };

    let target_device = select_dispatch_device(
        &state.pool,
        request.device_id.as_deref(),
        &target_kind,
        &target_name,
    )
    .await?;
    let target_device_id = target_device.id.clone();
    let job_id = Ulid::new().to_string();
    let correlation_id = Ulid::new().to_string();
    let short_id = job_id
        .chars()
        .take(8)
        .collect::<String>()
        .to_ascii_lowercase();
    let branch_name = match &target_kind {
        JobTargetKind::Repository => Some(format!("codex/{}-{}", short_id, slugify(title))),
        JobTargetKind::Capability => None,
    };

    let _job = sqlx::query_as::<_, JobRecord>(
        r#"
        insert into jobs (
            id,
            short_id,
            correlation_id,
            title,
            thread_id,
            target_kind,
            status,
            repo_name,
            capability_name,
            device_id,
            branch_name,
            base_branch
        )
        values ($1, $2, $3, $4, $5, $6, 'probing', $7, $8, $9, $10, $11)
        returning
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
        "#,
    )
    .bind(&job_id)
    .bind(&short_id)
    .bind(&correlation_id)
    .bind(title)
    .bind(thread_id)
    .bind(target_kind.as_str())
    .bind(&repo_name)
    .bind(&capability_name)
    .bind(&target_device_id)
    .bind(&branch_name)
    .bind(&base_branch)
    .fetch_one(&state.pool)
    .await?;

    insert_job_event(
        &state.pool,
        &job_id,
        &correlation_id,
        "job.created",
        json!({
            "correlation_id": correlation_id.clone(),
            "target_kind": target_kind,
            "target_name": target_name,
            "prompt": prompt,
            "repo_name": repo_name,
            "capability_name": capability_name,
            "device_id": target_device_id,
            "base_branch": base_branch.clone(),
            "branch_name": branch_name.clone(),
            "execution_intent": execution_intent,
        }),
    )
    .await?;

    let availability = match crate::services::jobs::routes_shim::probe_device_via_nats(
        &state.nats,
        &target_device.id,
        Some(job_id.clone()),
    )
    .await
    {
        Ok(availability) => availability,
        Err(error) => {
            crate::db::jobs::update_job_state(&state.pool, &job_id, "pending", None, None, None)
                .await?;
            insert_job_event(
                &state.pool,
                &job_id,
                &correlation_id,
                "job.probe_result",
                json!({
                    "correlation_id": correlation_id.clone(),
                    "available": false,
                    "reason": error.error.to_string(),
                    "probe_error": true,
                }),
            )
            .await?;
            return load_job_record(&state.pool, &job_id).await;
        }
    };

    insert_job_event(
        &state.pool,
        &job_id,
        &correlation_id,
        "job.probe_result",
        json!({
            "correlation_id": correlation_id.clone(),
            "available": availability.available,
            "reason": availability.reason,
            "probe_id": availability.probe_id,
            "responded_at": availability.responded_at,
        }),
    )
    .await?;

    if !availability.available {
        crate::db::jobs::update_job_state(&state.pool, &job_id, "pending", None, None, None)
            .await?;
        return load_job_record(&state.pool, &job_id).await;
    }

    let dispatch = JobDispatchMessage {
        job_id: job_id.clone(),
        short_id,
        correlation_id: correlation_id.clone(),
        thread_id: thread_id.to_string(),
        title: title.to_string(),
        device_id: target_device.id.clone(),
        target_kind: target_kind.clone(),
        target_name: target_name.clone(),
        base_branch: base_branch.clone(),
        branch_name: branch_name.clone(),
        prompt: prompt.to_string(),
        execution_intent: execution_intent.clone(),
        dispatched_at: Utc::now(),
    };
    let dispatch_subject = format!("elowen.jobs.dispatch.{}", target_device.id);
    let dispatch_payload =
        serde_json::to_vec(&dispatch).context("failed to serialize job dispatch")?;

    if let Err(error) = state
        .nats
        .publish(dispatch_subject.clone(), dispatch_payload.into())
        .await
    {
        crate::db::jobs::update_job_state(
            &state.pool,
            &job_id,
            "failed",
            Some("failure".to_string()),
            Some("infrastructure".to_string()),
            None,
        )
        .await?;
        insert_job_event(
            &state.pool,
            &job_id,
            &correlation_id,
            "job.dispatch_failed",
            json!({
                "correlation_id": correlation_id.clone(),
                "subject": dispatch_subject,
                "error": error.to_string(),
            }),
        )
        .await?;
        return Err(AppError::from(anyhow!("failed to publish job dispatch")));
    }

    crate::db::jobs::update_job_state(&state.pool, &job_id, "dispatched", None, None, None).await?;
    insert_job_event(
        &state.pool,
        &job_id,
        &correlation_id,
        "job.dispatched",
        json!({
            "correlation_id": correlation_id.clone(),
            "subject": dispatch_subject,
            "device_id": target_device.id.clone(),
            "target_kind": target_kind,
            "target_name": target_name,
            "repo_name": repo_name,
            "capability_name": capability_name,
            "base_branch": base_branch.clone(),
            "branch_name": branch_name.clone(),
            "execution_intent": execution_intent,
        }),
    )
    .await?;
    load_job_record(&state.pool, &job_id).await
}

pub(crate) async fn load_job_detail(state: &AppState, job_id: &str) -> Result<JobDetail, AppError> {
    let job = load_job_record(&state.pool, job_id).await?;
    let approvals = load_job_approvals(&state.pool, job_id).await?;
    let summary = crate::db::jobs::load_current_job_summary(&state.pool, job_id).await?;
    let related_notes = load_related_job_notes(state, &job, summary.as_ref()).await?;
    build_job_detail(&state.pool, job, approvals, related_notes).await
}

pub(crate) async fn load_job_detail_from_record(
    state: &AppState,
    job: JobRecord,
) -> Result<JobDetail, AppError> {
    let approvals = load_job_approvals(&state.pool, &job.id).await?;
    let summary = crate::db::jobs::load_current_job_summary(&state.pool, &job.id).await?;
    let related_notes = load_related_job_notes(state, &job, summary.as_ref()).await?;
    build_job_detail(&state.pool, job, approvals, related_notes).await
}

pub(crate) async fn select_dispatch_device(
    pool: &sqlx::PgPool,
    requested_device_id: Option<&str>,
    target_kind: &JobTargetKind,
    target_name: &str,
) -> Result<crate::models::DeviceRecord, AppError> {
    if let Some(device_id) =
        requested_device_id.and_then(|value| sanitize_optional_string(Some(value.to_string())))
    {
        let device: crate::models::DeviceRecord = load_device_row(pool, &device_id).await?.into();
        ensure_device_trusted_for_dispatch(&device)?;
        ensure_target_allowed(&device, target_kind, target_name)?;
        return Ok(device);
    }

    let devices = list_devices(pool).await?;

    for record in devices {
        if ensure_device_trusted_for_dispatch(&record).is_err() {
            continue;
        }
        if ensure_target_allowed(&record, target_kind, target_name).is_ok() {
            return Ok(record);
        }
    }

    let detail = match target_kind {
        JobTargetKind::Repository => "repository",
        JobTargetKind::Capability => "capability",
    };
    Err(AppError::conflict(anyhow!(
        "no registered device is eligible for the requested {detail}"
    )))
}

pub(crate) fn ensure_device_trusted_for_dispatch(
    device: &crate::models::DeviceRecord,
) -> Result<(), AppError> {
    let status = match device.trust.status {
        crate::models::DeviceTrustStatus::Trusted => "trusted",
        crate::models::DeviceTrustStatus::Rotated => "rotated",
        crate::models::DeviceTrustStatus::Revoked => "revoked",
        crate::models::DeviceTrustStatus::Untrusted => "untrusted",
        crate::models::DeviceTrustStatus::AttentionRequired => "needs_attention",
    };
    if device.trust.can_dispatch == Some(false)
        || device.trust.requires_attention
        || status != "trusted"
    {
        return Err(AppError::conflict(anyhow!(
            "device `{}` is not trusted for dispatch; trust status is {status}",
            device.id
        ))
        .with_code("dispatch_device_not_trusted"));
    }
    Ok(())
}

fn ensure_target_allowed(
    device: &crate::models::DeviceRecord,
    target_kind: &JobTargetKind,
    target_name: &str,
) -> Result<(), AppError> {
    match target_kind {
        JobTargetKind::Repository => {
            if !device_has_repo_scope(device) {
                return Ok(());
            }

            if selectable_repo_names(device)
                .iter()
                .any(|repo| repo == target_name)
            {
                return Ok(());
            }

            Err(AppError::bad_request(anyhow!(
                "device is not allowed to run the requested repository"
            )))
        }
        JobTargetKind::Capability => {
            if device
                .capabilities
                .iter()
                .map(|capability| capability.trim())
                .any(|capability| capability == target_name)
            {
                return Ok(());
            }

            Err(AppError::bad_request(anyhow!(
                "device does not advertise the requested capability"
            )))
        }
    }
}

fn device_has_repo_scope(device: &crate::models::DeviceRecord) -> bool {
    !device.allowed_repos.is_empty()
        || !device.allowed_repo_roots.is_empty()
        || !device.discovered_repos.is_empty()
}

pub(crate) fn selectable_repo_names(device: &crate::models::DeviceRecord) -> Vec<String> {
    selectable_repositories(device)
        .into_iter()
        .map(|repository| repository.name)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub(crate) fn selectable_repositories(
    device: &crate::models::DeviceRecord,
) -> Vec<DeviceRepository> {
    let hidden = device
        .hidden_repos
        .iter()
        .map(|repo| repo.as_str())
        .collect::<BTreeSet<_>>();
    let mut repositories = BTreeSet::new();

    for repository in &device.repositories {
        let name = repository.name.trim();
        if name.is_empty() || hidden.contains(name) {
            continue;
        }

        let mut branches = repository
            .branches
            .iter()
            .map(|branch| branch.trim())
            .filter(|branch| !branch.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        branches.sort();
        branches.dedup();
        repositories.insert(DeviceRepository {
            name: name.to_string(),
            branches,
        });
    }

    for repo in device
        .allowed_repos
        .iter()
        .chain(device.discovered_repos.iter())
        .map(|repo| repo.trim())
        .filter(|repo| !repo.is_empty())
    {
        if hidden.contains(repo) {
            continue;
        }

        repositories.insert(DeviceRepository {
            name: repo.to_string(),
            branches: Vec::new(),
        });
    }

    repositories.into_iter().collect()
}

pub(crate) async fn publish_push_approval_command(
    nats: &async_nats::Client,
    approval: &ApprovalRecord,
    job: &JobRecord,
) -> Result<(), AppError> {
    let device_id = job
        .device_id
        .as_deref()
        .ok_or_else(|| AppError::conflict(anyhow!("job device is missing")))?;
    let branch_name = job
        .branch_name
        .as_deref()
        .ok_or_else(|| AppError::conflict(anyhow!("job branch is missing")))?;
    if !matches!(job.target_kind_enum(), JobTargetKind::Repository) {
        return Err(AppError::conflict(anyhow!(
            "push approval is only available for repository jobs"
        )));
    }
    let command = JobApprovalCommand {
        approval_id: approval.id.clone(),
        job_id: job.id.clone(),
        short_id: job.short_id.clone(),
        correlation_id: job.correlation_id.clone(),
        device_id: device_id.to_string(),
        target_kind: job.target_kind_enum(),
        target_name: job.repo_name.clone(),
        branch_name: Some(branch_name.to_string()),
        action_type: approval.action_type.clone(),
        approved_at: Utc::now(),
    };
    let subject = format!("elowen.jobs.approvals.{device_id}");
    let payload = serde_json::to_vec(&command).context("failed to serialize approval command")?;
    nats.publish(subject, payload.into())
        .await
        .context("failed to publish approval command")?;
    Ok(())
}

pub(crate) mod routes_shim {
    use anyhow::{Context, anyhow};
    use async_nats::Client as NatsClient;
    use chrono::Utc;
    use tokio::time::timeout;

    use crate::{
        error::AppError,
        models::{AvailabilityProbeMessage, AvailabilitySnapshot},
    };

    pub(crate) async fn probe_device_via_nats(
        nats: &NatsClient,
        device_id: &str,
        job_id: Option<String>,
    ) -> Result<AvailabilitySnapshot, AppError> {
        let probe = AvailabilityProbeMessage {
            probe_id: ulid::Ulid::new().to_string(),
            job_id,
            device_id: device_id.to_string(),
            sent_at: Utc::now(),
        };
        let subject = format!("elowen.devices.availability.probe.{device_id}");
        let payload = serde_json::to_vec(&probe).context("failed to serialize probe")?;

        let message = timeout(
            std::time::Duration::from_secs(5),
            nats.request(subject, payload.into()),
        )
        .await
        .map_err(|_| AppError::gateway_timeout(anyhow!("device probe timed out")))?
        .context("device probe request failed")?;

        let response: AvailabilitySnapshot = serde_json::from_slice(&message.payload)
            .context("failed to decode device probe response")?;

        if response.device_id != device_id || response.probe_id != probe.probe_id {
            return Err(AppError::from(anyhow!(
                "device probe response did not match request"
            )));
        }

        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::selectable_repo_names;
    use crate::models::{DeviceRecord, DeviceRepository, DeviceTrustMetadata};
    use chrono::Utc;

    fn sample_device() -> DeviceRecord {
        DeviceRecord {
            id: "device-1".to_string(),
            name: "Laptop".to_string(),
            primary_flag: true,
            allowed_repos: vec!["elowen-api".to_string()],
            allowed_repo_roots: vec!["D:\\Projects".to_string()],
            hidden_repos: vec!["elowen-ui".to_string()],
            excluded_repo_paths: vec!["D:\\Projects\\archive".to_string()],
            discovered_repos: vec![
                "elowen-api".to_string(),
                "elowen-ui".to_string(),
                "elowen-platform".to_string(),
            ],
            repositories: vec![
                DeviceRepository {
                    name: "elowen-api".to_string(),
                    branches: vec!["main".to_string()],
                },
                DeviceRepository {
                    name: "elowen-ui".to_string(),
                    branches: vec!["main".to_string()],
                },
                DeviceRepository {
                    name: "elowen-platform".to_string(),
                    branches: vec!["main".to_string()],
                },
            ],
            capabilities: vec!["codex".to_string()],
            registered_at: Utc::now(),
            last_seen_at: Utc::now(),
            last_probe: None,
            trust: DeviceTrustMetadata::default(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn selectable_repo_names_hide_hidden_repositories() {
        let names = selectable_repo_names(&sample_device());

        assert_eq!(
            names,
            vec!["elowen-api".to_string(), "elowen-platform".to_string()]
        );
    }
}

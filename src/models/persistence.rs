//! SQL-backed persistence rows used by the orchestration API.

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::{FromRow, types::Json as SqlxJson};

use super::transport::{DeviceMetadata, DeviceRecord, JobEventRecord};

#[derive(Debug, Serialize, FromRow)]
pub(crate) struct ThreadSummary {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) current_summary_id: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) message_count: i64,
}

#[derive(Debug, Serialize, FromRow)]
pub(crate) struct ThreadRecord {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) current_summary_id: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
pub(crate) struct MessageRecord {
    pub(crate) id: String,
    pub(crate) thread_id: String,
    pub(crate) role: String,
    pub(crate) content: String,
    pub(crate) status: String,
    pub(crate) payload_json: Value,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Clone, FromRow)]
pub(crate) struct JobRecord {
    pub(crate) id: String,
    pub(crate) short_id: String,
    pub(crate) correlation_id: String,
    pub(crate) thread_id: String,
    pub(crate) title: String,
    pub(crate) status: String,
    pub(crate) result: Option<String>,
    pub(crate) failure_class: Option<String>,
    pub(crate) repo_name: String,
    pub(crate) device_id: Option<String>,
    pub(crate) branch_name: Option<String>,
    pub(crate) base_branch: Option<String>,
    pub(crate) parent_job_id: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, FromRow)]
pub(crate) struct JobEventRow {
    pub(crate) id: String,
    pub(crate) job_id: String,
    pub(crate) correlation_id: String,
    pub(crate) event_type: String,
    pub(crate) payload_json: SqlxJson<Value>,
    pub(crate) created_at: DateTime<Utc>,
}

impl From<JobEventRow> for JobEventRecord {
    fn from(row: JobEventRow) -> Self {
        Self {
            id: row.id,
            job_id: row.job_id,
            correlation_id: row.correlation_id,
            event_type: row.event_type,
            payload_json: row.payload_json.0,
            created_at: row.created_at,
        }
    }
}

#[derive(Debug, Serialize, FromRow)]
pub(crate) struct SummaryRecord {
    pub(crate) id: String,
    pub(crate) scope: String,
    pub(crate) source_id: String,
    pub(crate) version: i32,
    pub(crate) content: String,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, FromRow)]
pub(crate) struct ApprovalRecord {
    pub(crate) id: String,
    pub(crate) thread_id: String,
    pub(crate) job_id: String,
    pub(crate) action_type: String,
    pub(crate) status: String,
    pub(crate) summary: String,
    pub(crate) resolved_by: Option<String>,
    pub(crate) resolved_by_display_name: Option<String>,
    pub(crate) resolution_reason: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) resolved_at: Option<DateTime<Utc>>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, FromRow)]
pub(crate) struct JobStateRow {
    pub(crate) current_summary_id: Option<String>,
    pub(crate) execution_report_json: SqlxJson<Value>,
}

#[derive(Debug, FromRow)]
pub(crate) struct DeviceRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) primary_flag: bool,
    pub(crate) metadata: SqlxJson<DeviceMetadata>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

impl From<DeviceRow> for DeviceRecord {
    fn from(row: DeviceRow) -> Self {
        let metadata = row.metadata.0;
        Self {
            id: row.id,
            name: row.name,
            primary_flag: row.primary_flag,
            allowed_repos: metadata.allowed_repos,
            allowed_repo_roots: metadata.allowed_repo_roots,
            discovered_repos: metadata.discovered_repos,
            capabilities: metadata.capabilities,
            registered_at: metadata.registered_at.unwrap_or(row.created_at),
            last_seen_at: metadata.last_seen_at.unwrap_or(row.updated_at),
            last_probe: metadata.last_probe,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }
}

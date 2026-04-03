//! DTOs, database rows, and wire models used by the orchestration API.
//!
//! Some of these types intentionally mirror DTOs in `elowen-ui`, `elowen-edge`,
//! and `elowen-notes`. Keep duplicated contracts in sync across repositories.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{FromRow, types::Json as SqlxJson};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionIntent {
    WorkspaceChange,
    ReadOnly,
}

impl ExecutionIntent {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::WorkspaceChange => "workspace_change",
            Self::ReadOnly => "read_only",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionDraft {
    pub(crate) title: String,
    pub(crate) repo_name: Option<String>,
    pub(crate) base_branch: String,
    pub(crate) request_text: String,
    pub(crate) execution_intent: ExecutionIntent,
    pub(crate) source_message_id: String,
    pub(crate) source_role: String,
    pub(crate) rationale: String,
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

#[derive(Debug, Serialize)]
pub(crate) struct JobEventRecord {
    pub(crate) id: String,
    pub(crate) job_id: String,
    pub(crate) correlation_id: String,
    pub(crate) event_type: String,
    pub(crate) payload_json: Value,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ThreadDetail {
    #[serde(flatten)]
    pub(crate) thread: ThreadRecord,
    pub(crate) messages: Vec<MessageRecord>,
    pub(crate) jobs: Vec<JobRecord>,
    pub(crate) related_notes: Vec<NoteRecord>,
}

#[derive(Debug, Serialize)]
pub(crate) struct JobDetail {
    #[serde(flatten)]
    pub(crate) job: JobRecord,
    pub(crate) execution_report_json: Value,
    pub(crate) summary: Option<SummaryRecord>,
    pub(crate) approvals: Vec<ApprovalRecord>,
    pub(crate) related_notes: Vec<NoteRecord>,
    pub(crate) events: Vec<JobEventRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct NoteRecord {
    pub(crate) note_id: String,
    pub(crate) title: String,
    pub(crate) slug: String,
    pub(crate) summary: String,
    pub(crate) tags: Vec<String>,
    pub(crate) aliases: Vec<String>,
    pub(crate) note_type: String,
    pub(crate) source_kind: Option<String>,
    pub(crate) source_id: Option<String>,
    pub(crate) current_revision_id: String,
    pub(crate) updated_at: DateTime<Utc>,
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

#[derive(Debug, Deserialize)]
pub(crate) struct CreateThreadRequest {
    pub(crate) title: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateMessageRequest {
    pub(crate) role: String,
    pub(crate) content: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateChatDispatchRequest {
    pub(crate) content: String,
    pub(crate) title: Option<String>,
    pub(crate) repo_name: String,
    pub(crate) base_branch: Option<String>,
    pub(crate) device_id: Option<String>,
    pub(crate) execution_intent: Option<ExecutionIntent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateThreadChatRequest {
    pub(crate) content: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DispatchThreadMessageRequest {
    pub(crate) source_message_id: String,
    pub(crate) title: Option<String>,
    pub(crate) repo_name: String,
    pub(crate) base_branch: Option<String>,
    pub(crate) device_id: Option<String>,
    pub(crate) request_text: Option<String>,
    pub(crate) execution_intent: Option<ExecutionIntent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateJobRequest {
    pub(crate) title: String,
    pub(crate) repo_name: String,
    pub(crate) base_branch: Option<String>,
    pub(crate) request_text: String,
    pub(crate) device_id: Option<String>,
    pub(crate) execution_intent: Option<ExecutionIntent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResolveApprovalRequest {
    pub(crate) status: String,
    pub(crate) resolved_by: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PromoteJobNoteRequest {
    pub(crate) title: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) body_markdown: Option<String>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) aliases: Vec<String>,
    pub(crate) note_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct PromoteNoteRequest {
    pub(crate) note_id: Option<String>,
    pub(crate) source_kind: Option<String>,
    pub(crate) source_id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) slug: Option<String>,
    pub(crate) summary: Option<String>,
    pub(crate) body_markdown: String,
    pub(crate) tags: Vec<String>,
    pub(crate) aliases: Vec<String>,
    pub(crate) note_type: Option<String>,
    pub(crate) frontmatter: Option<Value>,
    pub(crate) authored_by: Option<NoteAuthor>,
    pub(crate) source_references: Vec<NoteSourceReference>,
}

#[derive(Debug, Serialize)]
pub(crate) struct NoteAuthor {
    pub(crate) actor_type: String,
    pub(crate) actor_id: String,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct NoteSourceReference {
    pub(crate) source_kind: String,
    pub(crate) source_id: String,
    pub(crate) label: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatDispatchResponse {
    pub(crate) message: MessageRecord,
    pub(crate) acknowledgement: MessageRecord,
    pub(crate) job: JobRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatReplyResponse {
    pub(crate) user_message: MessageRecord,
    pub(crate) assistant_message: MessageRecord,
}

#[derive(Debug, Serialize)]
pub(crate) struct MessageDispatchResponse {
    pub(crate) source_message: MessageRecord,
    pub(crate) acknowledgement: MessageRecord,
    pub(crate) job: JobRecord,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RegisterDeviceRequest {
    pub(crate) name: String,
    pub(crate) primary_flag: bool,
    #[serde(default)]
    pub(crate) allowed_repos: Vec<String>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProbeDeviceRequest {
    pub(crate) job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AvailabilitySnapshot {
    pub(crate) probe_id: String,
    pub(crate) job_id: Option<String>,
    pub(crate) device_id: String,
    pub(crate) available: bool,
    pub(crate) reason: String,
    pub(crate) responded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct DeviceMetadata {
    #[serde(default)]
    pub(crate) allowed_repos: Vec<String>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    pub(crate) registered_at: Option<DateTime<Utc>>,
    pub(crate) last_seen_at: Option<DateTime<Utc>>,
    pub(crate) last_probe: Option<AvailabilitySnapshot>,
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

#[derive(Debug, Serialize)]
pub(crate) struct DeviceRecord {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) primary_flag: bool,
    pub(crate) allowed_repos: Vec<String>,
    pub(crate) capabilities: Vec<String>,
    pub(crate) registered_at: DateTime<Utc>,
    pub(crate) last_seen_at: DateTime<Utc>,
    pub(crate) last_probe: Option<AvailabilitySnapshot>,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct AvailabilityProbeMessage {
    pub(crate) probe_id: String,
    pub(crate) job_id: Option<String>,
    pub(crate) device_id: String,
    pub(crate) sent_at: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct JobDispatchMessage {
    pub(crate) job_id: String,
    pub(crate) short_id: String,
    pub(crate) correlation_id: String,
    pub(crate) thread_id: String,
    pub(crate) title: String,
    pub(crate) device_id: String,
    pub(crate) repo_name: String,
    pub(crate) base_branch: String,
    pub(crate) branch_name: String,
    pub(crate) request_text: String,
    pub(crate) execution_intent: ExecutionIntent,
    pub(crate) dispatched_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobLifecycleEvent {
    pub(crate) job_id: String,
    pub(crate) correlation_id: String,
    pub(crate) device_id: String,
    pub(crate) event_type: String,
    pub(crate) status: Option<String>,
    pub(crate) result: Option<String>,
    pub(crate) failure_class: Option<String>,
    pub(crate) worktree_path: Option<String>,
    pub(crate) detail: Option<String>,
    pub(crate) payload_json: Option<Value>,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct JobApprovalCommand {
    pub(crate) approval_id: String,
    pub(crate) job_id: String,
    pub(crate) short_id: String,
    pub(crate) correlation_id: String,
    pub(crate) device_id: String,
    pub(crate) repo_name: String,
    pub(crate) branch_name: String,
    pub(crate) action_type: String,
    pub(crate) approved_at: DateTime<Utc>,
}

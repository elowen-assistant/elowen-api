//! HTTP, NATS, and UI-facing wire models for the orchestration API.

use chrono::{DateTime, Utc};
pub(crate) use elowen_contracts::{
    AvailabilityProbeMessage, AvailabilitySnapshot, DeviceRegistrationTrustProof, DeviceRepository,
    ExecutionIntent, JobApprovalCommand, JobDispatchMessage, JobLifecycleEvent, JobTargetKind,
    OrchestratorTrustSigner, RegisterDeviceRequest, RegistrationChallengeResponse,
    RegistrationTrustIntent,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{AuthMode, AuthPermission, SessionActor};

use super::persistence::{ApprovalRecord, JobRecord, MessageRecord, SummaryRecord, ThreadRecord};

/// Lightweight browser notification that tells the UI which REST resource changed.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct UiEvent {
    pub(crate) event_type: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) job_id: Option<String>,
    pub(crate) device_id: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ExecutionDraft {
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) target_kind: JobTargetKind,
    pub(crate) target_name: String,
    pub(crate) base_branch: Option<String>,
    pub(crate) prompt: String,
    pub(crate) execution_intent: ExecutionIntent,
    pub(crate) source_message_id: String,
    pub(crate) source_role: String,
    pub(crate) rationale: String,
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
    #[serde(default)]
    pub(crate) relevance_score: f64,
    #[serde(default)]
    pub(crate) match_reasons: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct NoteRevisionRecord {
    pub(crate) revision_id: String,
    pub(crate) note_id: String,
    pub(crate) version: i32,
    pub(crate) summary: String,
    pub(crate) body_markdown: String,
    pub(crate) frontmatter: Value,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) previous_revision_id: Option<String>,
    pub(crate) authored_by: Option<NoteAuthor>,
    pub(crate) source_references: Vec<NoteSourceReference>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct NoteDetailRecord {
    pub(crate) note: NoteRecord,
    pub(crate) revision: NoteRevisionRecord,
}

#[derive(Debug, Clone)]
pub(crate) struct RelatedNoteContext {
    pub(crate) note: NoteRecord,
    pub(crate) memory_role: String,
    pub(crate) source_label: String,
    pub(crate) detail_excerpt: Option<String>,
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
    #[serde(default)]
    pub(crate) target_kind: JobTargetKind,
    pub(crate) target_name: Option<String>,
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
    #[serde(default)]
    pub(crate) target_kind: JobTargetKind,
    pub(crate) target_name: Option<String>,
    pub(crate) base_branch: Option<String>,
    pub(crate) device_id: Option<String>,
    pub(crate) prompt: Option<String>,
    pub(crate) execution_intent: Option<ExecutionIntent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateJobRequest {
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) target_kind: JobTargetKind,
    pub(crate) target_name: Option<String>,
    pub(crate) base_branch: Option<String>,
    pub(crate) prompt: String,
    pub(crate) device_id: Option<String>,
    pub(crate) execution_intent: Option<ExecutionIntent>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResolveApprovalRequest {
    pub(crate) status: String,
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

#[derive(Debug, Serialize, Deserialize, Clone)]
pub(crate) struct NoteAuthor {
    pub(crate) actor_type: String,
    pub(crate) actor_id: String,
    pub(crate) display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeviceTrustStatus {
    #[default]
    Untrusted,
    Trusted,
    Rotated,
    Revoked,
    AttentionRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) struct DeviceTrustMetadata {
    #[serde(default)]
    pub(crate) status: DeviceTrustStatus,
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) summary: Option<String>,
    #[serde(default)]
    pub(crate) detail: Option<String>,
    #[serde(default)]
    pub(crate) reason: Option<String>,
    #[serde(default)]
    pub(crate) enrollment_kind: Option<String>,
    #[serde(default)]
    pub(crate) current_edge_public_key: Option<String>,
    #[serde(default)]
    pub(crate) previous_edge_public_keys: Vec<String>,
    #[serde(default)]
    pub(crate) revoked_edge_public_keys: Vec<String>,
    #[serde(default)]
    pub(crate) last_trusted_registration_at: Option<DateTime<Utc>>,
    #[serde(default)]
    #[serde(alias = "last_rotation_at")]
    pub(crate) rotated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) revoked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) status_reason: Option<String>,
    #[serde(default)]
    pub(crate) last_orchestrator_key_id: Option<String>,
    #[serde(default)]
    pub(crate) last_orchestrator_public_key: Option<String>,
    #[serde(default)]
    pub(crate) last_registration_intent: Option<RegistrationTrustIntent>,
    #[serde(default)]
    pub(crate) updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) can_dispatch: Option<bool>,
    #[serde(default, alias = "attention_needed")]
    pub(crate) requires_attention: bool,
}

impl DeviceTrustMetadata {
    pub(crate) fn normalized(
        mut self,
        legacy_edge_public_key: Option<String>,
        legacy_last_trusted_registration_at: Option<DateTime<Utc>>,
    ) -> Self {
        if self.current_edge_public_key.is_none() {
            self.current_edge_public_key = legacy_edge_public_key;
        }

        if self.last_trusted_registration_at.is_none() {
            self.last_trusted_registration_at = legacy_last_trusted_registration_at;
        }

        self.previous_edge_public_keys.sort();
        self.previous_edge_public_keys.dedup();
        self.revoked_edge_public_keys.sort();
        self.revoked_edge_public_keys.dedup();

        if self.revoked_at.is_some() {
            self.status = DeviceTrustStatus::Revoked;
        } else if matches!(self.status, DeviceTrustStatus::Untrusted)
            && self.current_edge_public_key.is_some()
        {
            self.status = DeviceTrustStatus::Trusted;
        }

        self.label.get_or_insert_with(|| match self.status {
            DeviceTrustStatus::Trusted => "Trusted".to_string(),
            DeviceTrustStatus::Rotated => "Rotated".to_string(),
            DeviceTrustStatus::Revoked => "Revoked".to_string(),
            DeviceTrustStatus::Untrusted => "Untrusted".to_string(),
            DeviceTrustStatus::AttentionRequired => "Needs Attention".to_string(),
        });

        self
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProbeDeviceRequest {
    pub(crate) job_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct DeviceMetadata {
    #[serde(default)]
    pub(crate) allowed_repos: Vec<String>,
    #[serde(default)]
    pub(crate) allowed_repo_roots: Vec<String>,
    #[serde(default)]
    pub(crate) hidden_repos: Vec<String>,
    #[serde(default)]
    pub(crate) excluded_repo_paths: Vec<String>,
    #[serde(default)]
    pub(crate) discovered_repos: Vec<String>,
    #[serde(default)]
    pub(crate) repositories: Vec<DeviceRepository>,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    pub(crate) registered_at: Option<DateTime<Utc>>,
    pub(crate) last_seen_at: Option<DateTime<Utc>>,
    pub(crate) last_probe: Option<AvailabilitySnapshot>,
    #[serde(default)]
    pub(crate) trust: DeviceTrustMetadata,
    #[serde(default, skip_serializing)]
    pub(crate) edge_public_key: Option<String>,
    #[serde(default, skip_serializing)]
    pub(crate) last_trusted_registration_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct DeviceRecord {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) primary_flag: bool,
    pub(crate) allowed_repos: Vec<String>,
    pub(crate) allowed_repo_roots: Vec<String>,
    pub(crate) hidden_repos: Vec<String>,
    pub(crate) excluded_repo_paths: Vec<String>,
    pub(crate) discovered_repos: Vec<String>,
    pub(crate) repositories: Vec<DeviceRepository>,
    pub(crate) capabilities: Vec<String>,
    pub(crate) registered_at: DateTime<Utc>,
    pub(crate) last_seen_at: DateTime<Utc>,
    pub(crate) last_probe: Option<AvailabilitySnapshot>,
    pub(crate) trust: DeviceTrustMetadata,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RevokeDeviceTrustRequest {
    pub(crate) edge_public_key: Option<String>,
    pub(crate) reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RepositoryOption {
    pub(crate) name: String,
    pub(crate) device_count: usize,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoginRequest {
    pub(crate) username: Option<String>,
    pub(crate) password: String,
}

#[derive(Debug, Serialize)]
pub(crate) struct AuthSessionStatus {
    pub(crate) enabled: bool,
    pub(crate) auth_mode: AuthMode,
    pub(crate) authenticated: bool,
    pub(crate) actor: Option<SessionActor>,
    pub(crate) permissions: Vec<AuthPermission>,
}

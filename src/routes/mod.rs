//! HTTP route handlers grouped by domain.

mod approvals;
mod auth;
mod devices;
mod jobs;
mod notes;
mod threads;
mod trust;

pub(crate) use approvals::resolve_approval;
pub(crate) use auth::{
    get_auth_session, login, logout, registration_challenge, require_admin_session,
    require_operator_session, require_session_actor, require_viewer_session, stream_ui_events,
};
pub(crate) use devices::{
    clear_device_trust_revocation, confirm_device_trust_rotation, get_device,
    list_device_trust_history, list_devices, list_repositories, probe_device, register_device,
    resolve_device_trust_attention, revoke_device_trust,
};
pub(crate) use jobs::{get_job, list_jobs, retry_job};
pub(crate) use notes::{get_job_notes, get_thread_notes, promote_job_note};
pub(crate) use threads::{
    create_chat_dispatch, create_job, create_message, create_thread, create_thread_chat,
    dispatch_thread_message, get_thread, list_thread_jobs, list_threads,
};
pub(crate) use trust::{
    activate_orchestrator_signer, list_orchestrator_signers, retire_orchestrator_signer,
    stage_orchestrator_signer,
};

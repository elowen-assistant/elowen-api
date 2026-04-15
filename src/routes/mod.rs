//! HTTP route handlers grouped by domain.

mod approvals;
mod auth;
mod devices;
mod jobs;
mod notes;
mod threads;

pub(crate) use approvals::resolve_approval;
pub(crate) use auth::{
    get_auth_session, login, logout, registration_challenge, require_authenticated_session,
    stream_ui_events,
};
pub(crate) use devices::{get_device, list_devices, probe_device, register_device};
pub(crate) use jobs::{get_job, list_jobs};
pub(crate) use notes::{get_job_notes, get_thread_notes, promote_job_note};
pub(crate) use threads::{
    create_chat_dispatch, create_job, create_message, create_thread, create_thread_chat,
    dispatch_thread_message, get_thread, list_thread_jobs, list_threads,
};

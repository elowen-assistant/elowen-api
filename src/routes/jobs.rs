//! Job detail and listing routes.

use axum::{
    Json,
    extract::{Path, State},
};

use crate::{
    db::jobs::list_jobs as load_jobs,
    error::AppError,
    models::{JobDetail, JobRecord},
    services::{
        jobs::{load_job_detail, retry_job_dispatch},
        ui_events::{job_ui_event, publish_ui_event},
    },
    state::AppState,
};

pub(crate) async fn list_jobs(
    State(state): State<AppState>,
) -> Result<Json<Vec<JobRecord>>, AppError> {
    Ok(Json(load_jobs(&state.pool).await?))
}

pub(crate) async fn get_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<JobDetail>, AppError> {
    Ok(Json(load_job_detail(&state, &job_id).await?))
}

pub(crate) async fn retry_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<JobDetail>, AppError> {
    let job = retry_job_dispatch(&state, &job_id).await?;
    publish_ui_event(
        &state,
        job_ui_event(&job.thread_id, &job.id, job.device_id.as_deref()),
    );
    Ok(Json(load_job_detail(&state, &job.id).await?))
}

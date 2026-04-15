//! Job detail and listing routes.

use axum::{
    Json,
    extract::{Path, State},
};

use crate::{
    db::jobs::list_jobs as load_jobs,
    error::AppError,
    models::{JobDetail, JobRecord},
    services::jobs::load_job_detail,
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

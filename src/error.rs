//! Error mapping for the orchestration API.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

/// API error response body.
#[derive(Debug, Serialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: String,
}

/// Application error that carries an HTTP status code.
#[derive(Debug)]
pub(crate) struct AppError {
    pub(crate) status: StatusCode,
    pub(crate) error: anyhow::Error,
}

impl AppError {
    pub(crate) fn bad_request(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: message.into(),
        }
    }

    pub(crate) fn not_found(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: message.into(),
        }
    }

    pub(crate) fn conflict(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: message.into(),
        }
    }

    pub(crate) fn unauthorized(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            error: message.into(),
        }
    }

    pub(crate) fn gateway_timeout(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            error: message.into(),
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.error.to_string(),
            }),
        )
            .into_response()
    }
}

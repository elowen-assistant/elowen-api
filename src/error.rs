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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) code: Option<String>,
}

/// Application error that carries an HTTP status code.
#[derive(Debug)]
pub(crate) struct AppError {
    pub(crate) status: StatusCode,
    pub(crate) error: anyhow::Error,
    pub(crate) code: Option<String>,
}

impl AppError {
    pub(crate) fn bad_request(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: message.into(),
            code: None,
        }
    }

    pub(crate) fn not_found(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: message.into(),
            code: None,
        }
    }

    pub(crate) fn conflict(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: message.into(),
            code: None,
        }
    }

    pub(crate) fn unauthorized(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            error: message.into(),
            code: None,
        }
    }

    pub(crate) fn forbidden(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            error: message.into(),
            code: None,
        }
    }

    pub(crate) fn gateway_timeout(message: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::GATEWAY_TIMEOUT,
            error: message.into(),
            code: None,
        }
    }

    pub(crate) fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
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
            code: None,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.error.to_string(),
                code: self.code,
            }),
        )
            .into_response()
    }
}

//! HTTP error type for handlers. Holds a status code + a JSON body so
//! every error response looks the same on the wire:
//!
//!   { "error": "short_machine_code", "message": "human-readable detail" }
//!
//! Handlers return `Result<T, ApiError>` and use `?` against sqlx /
//! serde / anyhow errors via the `From` impls below. Anything not
//! explicitly classified maps to a 500 with a generic message - the
//! detailed cause is logged but not exposed to the client (no
//! information leakage from internal failures).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    message: &'a str,
}

impl ApiError {
    pub fn bad_request(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: msg.into(),
        }
    }

    pub fn not_found(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code,
            message: msg.into(),
        }
    }

    pub fn unauthorized(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code,
            message: msg.into(),
        }
    }

    pub fn forbidden(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code,
            message: msg.into(),
        }
    }

    pub fn conflict(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            message: msg.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // 5xx: log the detail server-side but only return a generic
        // message to the client. 4xx: surface the message - clients need
        // to know what they did wrong.
        let public_message = if self.status.is_server_error() {
            tracing::error!(code = %self.code, error = %self.message, "server error");
            "internal server error"
        } else {
            &self.message
        };
        (
            self.status,
            Json(ErrorBody {
                error: self.code,
                message: public_message,
            }),
        )
            .into_response()
    }
}

// --- From impls for ergonomic `?` in handlers ---

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::internal(format!("db: {e}"))
    }
}

impl From<argon2::password_hash::Error> for ApiError {
    fn from(e: argon2::password_hash::Error) -> Self {
        ApiError::internal(format!("argon2: {e}"))
    }
}

impl From<jsonwebtoken::errors::Error> for ApiError {
    fn from(e: jsonwebtoken::errors::Error) -> Self {
        ApiError::internal(format!("jwt: {e}"))
    }
}

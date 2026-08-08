//! Error type for the bank-SMS module.
//!
//! Responses are RFC 7807 `application/problem+json`. The existing apex-rust
//! handlers return ad-hoc actix errors; new endpoints use this instead, so error
//! shape is consistent and SQLSTATE classification happens in one place.

use actix_web::{http::StatusCode, HttpResponse, ResponseError};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),

    #[error("not authenticated")]
    Unauthenticated,

    #[error("insufficient permission")]
    Forbidden,

    #[error("{0} not found")]
    NotFound(String),

    /// Optimistic-concurrency failure: the client's If-Match version is stale.
    #[error("version conflict: expected {expected}, found {actual}")]
    VersionConflict { expected: i32, actual: i32 },

    #[error("{0}")]
    Conflict(String),

    /// If-Match is mandatory on PUT/PATCH; a missing header is not a 400 but a
    /// 428, so a client can tell "you forgot the header" from "your body is bad".
    #[error("If-Match header is required")]
    PreconditionRequired,

    #[error("upstream WhatsApp API error: {0}")]
    Upstream(String),

    #[error("database error")]
    Database(#[from] sqlx::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

#[derive(Serialize)]
struct Problem {
    /// RFC 7807 `type`. A stable, machine-readable slug.
    #[serde(rename = "type")]
    kind: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expected_version: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actual_version: Option<i32>,
}

impl AppError {
    fn kind(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "about:blank#bad-request",
            AppError::Unauthenticated => "about:blank#unauthenticated",
            AppError::Forbidden => "about:blank#forbidden",
            AppError::NotFound(_) => "about:blank#not-found",
            AppError::VersionConflict { .. } => "about:blank#version-conflict",
            AppError::Conflict(_) => "about:blank#conflict",
            AppError::PreconditionRequired => "about:blank#precondition-required",
            AppError::Upstream(_) => "about:blank#upstream-error",
            AppError::Database(_) | AppError::Internal(_) => "about:blank#internal-error",
        }
    }

    fn title(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "Bad Request",
            AppError::Unauthenticated => "Unauthenticated",
            AppError::Forbidden => "Forbidden",
            AppError::NotFound(_) => "Not Found",
            AppError::VersionConflict { .. } | AppError::Conflict(_) => "Conflict",
            AppError::PreconditionRequired => "Precondition Required",
            AppError::Upstream(_) => "Bad Gateway",
            AppError::Database(_) | AppError::Internal(_) => "Internal Server Error",
        }
    }
}

impl ResponseError for AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::Unauthenticated => StatusCode::UNAUTHORIZED,
            AppError::Forbidden => StatusCode::FORBIDDEN,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::VersionConflict { .. } | AppError::Conflict(_) => StatusCode::CONFLICT,
            AppError::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,

            // sqlx errors are classified by SQLSTATE rather than lumped into 500:
            // a unique violation is the caller's problem, not ours.
            AppError::Database(e) => match sqlstate(e).as_deref() {
                Some("23505") => StatusCode::CONFLICT,            // unique_violation
                Some("23503") => StatusCode::CONFLICT,            // foreign_key_violation
                Some("23502") => StatusCode::BAD_REQUEST,         // not_null_violation
                Some("23514") => StatusCode::BAD_REQUEST,         // check_violation
                Some(code) if code.starts_with("22") => StatusCode::BAD_REQUEST, // data exception
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },

            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        let status = self.status_code();

        // Never leak database internals to a client; the operator gets the detail
        // in the log instead.
        let detail = match self {
            AppError::Database(e) if status == StatusCode::INTERNAL_SERVER_ERROR => {
                log::error!("unhandled database error: {e}");
                "an internal error occurred".to_string()
            }
            AppError::Internal(msg) => {
                log::error!("internal error: {msg}");
                "an internal error occurred".to_string()
            }
            other => other.to_string(),
        };

        let (expected_version, actual_version) = match self {
            AppError::VersionConflict { expected, actual } => (Some(*expected), Some(*actual)),
            _ => (None, None),
        };

        HttpResponse::build(status)
            .content_type("application/problem+json")
            .json(Problem {
                kind: self.kind(),
                title: self.title(),
                status: status.as_u16(),
                detail,
                expected_version,
                actual_version,
            })
    }
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(db) => db.code().map(|c| c.to_string()),
        _ => None,
    }
}

pub type AppResult<T> = Result<T, AppError>;

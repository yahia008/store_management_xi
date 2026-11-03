use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("authentication required")]
    Unauthorized,
    #[error("invalid email or password")]
    InvalidCredentials,
    #[error("you do not have permission to do this")]
    Forbidden,
    #[error("{0} not found")]
    NotFound(&'static str),
    #[error("{0}")]
    Conflict(String),
    #[error("multi-factor authentication is required: enrol an authenticator app and verify a code first")]
    MfaRequired,
    #[error("too many requests; try again in {0} seconds")]
    RateLimited(i64),
    #[error("internal error")]
    Internal(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> Self {
        ApiError::BadRequest(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        ApiError::Conflict(msg.into())
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        if let sqlx::Error::Database(db) = &e {
            // 23505 unique violation, 23514 check violation, 23503 foreign key violation.
            match db.code().as_deref() {
                Some("23505") => return ApiError::Conflict("that record already exists".into()),
                Some("23514") => return ApiError::BadRequest("a value is not allowed".into()),
                _ => {}
            }
        }
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unauthorized | ApiError::InvalidCredentials => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::MfaRequired => (StatusCode::FORBIDDEN, "mfa_required"),
            ApiError::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        };
        // Never leak internals to the client; log them instead.
        let message = match &self {
            ApiError::Internal(detail) => {
                tracing::error!("internal error: {detail}");
                "something went wrong".to_string()
            }
            other => other.to_string(),
        };
        let mut response = (status, Json(json!({ "error": { "code": code, "message": message } }))).into_response();
        if let ApiError::RateLimited(secs) = &self {
            if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                response.headers_mut().insert(axum::http::header::RETRY_AFTER, v);
            }
        }
        response
    }
}

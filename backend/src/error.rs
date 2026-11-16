//! Ошибки API. Клиент всегда получает `{ "error": { "code": "...", "fields": { ... } } }`.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

/// Ошибки валидации: имя поля → код ошибки.
pub type FieldErrors = BTreeMap<&'static str, &'static str>;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// Запрос не удалось разобрать (не JSON, нет обязательных полей, неверные типы).
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    #[error("payload too large")]
    PayloadTooLarge,
    #[error("method not allowed")]
    MethodNotAllowed,
    #[error("unauthorized: {0}")]
    Unauthorized(&'static str),
    #[error("forbidden: {0}")]
    Forbidden(&'static str),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(&'static str),
    #[error("validation failed")]
    Validation(FieldErrors),
    #[error("rate limited")]
    RateLimited { retry_after_secs: u64 },
    /// Сервер перегружен (очередь на проверку паролей) — повторите позже.
    #[error("busy")]
    Busy,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    pub fn field(name: &'static str, code: &'static str) -> Self {
        Self::Validation(BTreeMap::from([(name, code)]))
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, fields) = match &self {
            Self::BadRequest(code) => (StatusCode::BAD_REQUEST, *code, None),
            Self::PayloadTooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", None),
            Self::MethodNotAllowed => (StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed", None),
            Self::Unauthorized(code) => (StatusCode::UNAUTHORIZED, *code, None),
            Self::Forbidden(code) => (StatusCode::FORBIDDEN, *code, None),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", None),
            Self::Conflict(code) => (StatusCode::CONFLICT, *code, None),
            Self::Validation(fields) => (StatusCode::UNPROCESSABLE_ENTITY, "validation", Some(fields)),
            Self::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, "rate_limited", None),
            Self::Busy => (StatusCode::SERVICE_UNAVAILABLE, "busy", None),
            Self::Internal(error) => {
                // Подробности — только в журнал сервера, клиенту их не показываем.
                tracing::error!(error = ?error, "внутренняя ошибка");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal", None)
            }
        };

        let body = Json(json!({ "error": { "code": code, "fields": fields.cloned().unwrap_or_default() } }));
        let mut response = (status, body).into_response();

        let retry_after = match self {
            Self::RateLimited { retry_after_secs } => Some(retry_after_secs),
            Self::Busy => Some(5),
            _ => None,
        };
        if let Some(value) = retry_after.and_then(|secs| HeaderValue::from_str(&secs.to_string()).ok()) {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response
    }
}

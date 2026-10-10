//! Ошибки API: всегда JSON `{code, message}` с подходящим HTTP-статусом.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use feather_protocol::ApiError;

#[derive(Debug)]
pub enum AppError {
    BadRequest(&'static str, String),
    Unauthorized,
    Forbidden,
    NotFound(&'static str),
    Conflict(&'static str, String),
    Internal(String),
}

pub type ApiResult<T> = Result<T, AppError>;

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            AppError::BadRequest(code, msg) => (StatusCode::BAD_REQUEST, code, msg),
            AppError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", "нужна авторизация".into()),
            AppError::Forbidden => (StatusCode::FORBIDDEN, "forbidden", "недостаточно прав".into()),
            AppError::NotFound(what) => (StatusCode::NOT_FOUND, "not_found", format!("{what} не найден(о)")),
            AppError::Conflict(code, msg) => (StatusCode::CONFLICT, code, msg),
            AppError::Internal(detail) => {
                // Подробности только в лог: клиенту не показываем внутренности
                tracing::error!("внутренняя ошибка: {detail}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal", "внутренняя ошибка сервера".into())
            }
        };
        (status, Json(ApiError { code: code.into(), message })).into_response()
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        AppError::Internal(format!("БД: {e}"))
    }
}

/// Нарушение уникальности в Postgres (код 23505)
pub fn is_unique_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(db) if db.code().as_deref() == Some("23505"))
}

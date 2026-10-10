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
    /// Слишком часто: через сколько секунд можно повторить
    TooManyRequests(u64),
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
            AppError::TooManyRequests(secs) => {
                let body = Json(ApiError { code: "rate_limited".into(), message: format!("слишком часто, повторите через {secs} с") });
                return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", secs.to_string())], body).into_response();
            }
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

impl AppError {
    pub fn rate_limited(wait: std::time::Duration) -> Self {
        AppError::TooManyRequests(wait.as_secs().max(1))
    }
}

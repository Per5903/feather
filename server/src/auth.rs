//! Регистрация, вход, сессии (docs/07-security-privacy.md).
//!
//! Токен сессии непрозрачный: 32 случайных байта, в базе только SHA-256 от него.
//! Утечка базы не даёт рабочих токенов, а выход из сессии отзывает токен мгновенно.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};
use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use feather_protocol::{LoginRequest, LoginResponse, RegisterRequest, Snowflake, User};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::Shared;
use crate::error::{ApiResult, AppError, is_unique_violation};
use crate::ratelimit::{self, ClientIp};

const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 256;

pub type UserRow = (i64, String, Option<String>);

pub fn user_from_row((id, username, display_name): UserRow) -> User {
    User { id: Snowflake(id as u64), username, display_name }
}

fn validate_username(name: &str) -> ApiResult<()> {
    let len = name.chars().count();
    let ok_chars = name.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !(2..=32).contains(&len) || !ok_chars {
        return Err(AppError::BadRequest(
            "bad_username",
            "имя: 2–32 символа, буквы, цифры, _ . -".into(),
        ));
    }
    Ok(())
}

pub async fn register(State(st): State<Shared>, ClientIp(ip): ClientIp, Json(req): Json<RegisterRequest>) -> ApiResult<(StatusCode, Json<User>)> {
    st.limiter.check(&format!("auth:{ip}"), ratelimit::AUTH).map_err(AppError::rate_limited)?;
    let username = req.username.trim().to_owned();
    validate_username(&username)?;
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&req.password.chars().count()) {
        return Err(AppError::BadRequest("bad_password", format!("пароль: от {MIN_PASSWORD} символов")));
    }
    // Argon2 намеренно медленный — не блокируем им рантайм
    let password = req.password;
    let hash = tokio::task::spawn_blocking(move || {
        Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string())
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?
    .map_err(|e| AppError::Internal(format!("argon2: {e}")))?;

    let id = st.ids.next();
    let res = sqlx::query("INSERT INTO users (id, username, password_hash) VALUES ($1, $2, $3)")
        .bind(id.0 as i64)
        .bind(&username)
        .bind(&hash)
        .execute(&st.db)
        .await;
    match res {
        Err(e) if is_unique_violation(&e) => Err(AppError::Conflict("username_taken", "это имя уже занято".into())),
        Err(e) => Err(e.into()),
        Ok(_) => Ok((StatusCode::CREATED, Json(User { id, username, display_name: None }))),
    }
}

pub async fn login(State(st): State<Shared>, ClientIp(ip): ClientIp, Json(req): Json<LoginRequest>) -> ApiResult<Json<LoginResponse>> {
    st.limiter.check(&format!("auth:{ip}"), ratelimit::AUTH).map_err(AppError::rate_limited)?;
    let row: Option<(i64, String, Option<String>, String)> = sqlx::query_as(
        "SELECT id, username, display_name, password_hash FROM users WHERE lower(username) = lower($1)",
    )
    .bind(req.username.trim())
    .fetch_optional(&st.db)
    .await?;

    // Одинаковый ответ для «нет пользователя» и «неверный пароль»: не подсказываем, какие имена есть
    let bad = || AppError::BadRequest("bad_credentials", "неверное имя или пароль".into());
    let Some((id, username, display_name, hash)) = row else { return Err(bad()) };
    let password = req.password;
    let valid = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
    })
    .await
    .map_err(|e| AppError::Internal(e.to_string()))?;
    if !valid {
        return Err(bad());
    }

    let token = new_token();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, device_name, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(days => $4))",
    )
    .bind(token_hash(&token))
    .bind(id)
    .bind(req.device_name)
    .bind(st.cfg.session_ttl_days as i32)
    .execute(&st.db)
    .await?;

    Ok(Json(LoginResponse { token, user: user_from_row((id, username, display_name)) }))
}

pub async fn logout(State(st): State<Shared>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let token = bearer(&headers).ok_or(AppError::Unauthorized)?;
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1").bind(token_hash(token)).execute(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(user: AuthUser) -> Json<User> {
    Json(user.0)
}

fn new_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("нет источника случайности");
    URL_SAFE_NO_PAD.encode(buf)
}

fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")
}

/// Пользователь по токену сессии (REST и шлюз)
pub async fn user_by_token(db: &PgPool, token: &str) -> ApiResult<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name FROM sessions s JOIN users u ON u.id = s.user_id
         WHERE s.token_hash = $1 AND s.expires_at > now()",
    )
    .bind(token_hash(token))
    .fetch_optional(db)
    .await?;
    Ok(row.map(user_from_row))
}

/// Извлекатель для обработчиков: запрос без действующего токена получает 401
pub struct AuthUser(pub User);

impl AuthUser {
    pub fn id(&self) -> Snowflake {
        self.0.id
    }
}

impl FromRequestParts<Shared> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Result<Self, Self::Rejection> {
        let token = bearer(&parts.headers).ok_or(AppError::Unauthorized)?;
        user_by_token(&st.db, token).await?.map(AuthUser).ok_or(AppError::Unauthorized)
    }
}

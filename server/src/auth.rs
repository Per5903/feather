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
use feather_protocol::{InstanceInfo, LoginRequest, LoginResponse, RegisterRequest, RegistrationMode, Snowflake, User};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

use crate::Shared;
use crate::error::{ApiResult, AppError, is_unique_violation};
use crate::ratelimit::{self, ClientIp};

const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 256;

pub type UserRow = (i64, String, Option<String>, Option<i64>);

pub fn user_from_row((id, username, display_name, avatar): UserRow) -> User {
    User { id: Snowflake(id as u64), username, display_name, avatar: avatar.map(|a| Snowflake(a as u64)) }
}

pub async fn load_user(db: &PgPool, id: Snowflake) -> ApiResult<User> {
    let row: Option<UserRow> = sqlx::query_as("SELECT u.id, u.username, u.display_name, u.avatar_id FROM users u WHERE u.id = $1")
        .bind(id.0 as i64)
        .fetch_optional(db)
        .await?;
    row.map(user_from_row).ok_or(AppError::NotFound("пользователь"))
}

/// Режим регистрации: из панели администратора, иначе из FEATHER_REGISTRATION
pub async fn registration_mode(st: &Shared) -> ApiResult<RegistrationMode> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM instance_settings WHERE key = 'registration'").fetch_optional(&st.db).await?;
    Ok(match row.as_ref().map(|r| r.0.as_str()) {
        Some("open") => RegistrationMode::Open,
        Some("invite") => RegistrationMode::Invite,
        Some("closed") => RegistrationMode::Closed,
        _ => st.cfg.registration,
    })
}

pub async fn hash_password(password: String) -> ApiResult<String> {
    // Argon2 намеренно медленный — не блокируем им рантайм
    tokio::task::spawn_blocking(move || Argon2::default().hash_password(password.as_bytes()).map(|h| h.to_string()))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .map_err(|e| AppError::Internal(format!("argon2: {e}")))
}

pub async fn verify_password(password: String, hash: String) -> ApiResult<bool> {
    tokio::task::spawn_blocking(move || PasswordHash::new(&hash).is_ok_and(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok()))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))
}

pub fn validate_password(p: &str) -> ApiResult<()> {
    if !(MIN_PASSWORD..=MAX_PASSWORD).contains(&p.chars().count()) {
        return Err(AppError::BadRequest("bad_password", format!("пароль: от {MIN_PASSWORD} символов")));
    }
    Ok(())
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
    let invite = req.invite_code.as_deref().map(str::trim).filter(|c| !c.is_empty());
    match registration_mode(&st).await? {
        RegistrationMode::Closed => {
            return Err(AppError::BadRequest("registration_closed", "регистрация на этом сервере закрыта".into()));
        }
        RegistrationMode::Invite => match invite {
            Some(code) if crate::guilds::invite_is_valid(&st.db, code).await? => {}
            // Первый аккаунт на новом сервере — без кода, иначе приглашать было бы некому
            _ if sqlx::query_as::<_, (bool,)>("SELECT NOT EXISTS (SELECT 1 FROM users)").fetch_one(&st.db).await?.0 => {}
            Some(_) => return Err(AppError::BadRequest("bad_invite", "код приглашения недействителен или истёк".into())),
            None => return Err(AppError::BadRequest("invite_required", "регистрация только по приглашению: введите код".into())),
        },
        RegistrationMode::Open => {}
    }
    let username = req.username.trim().to_owned();
    validate_username(&username)?;
    validate_password(&req.password)?;
    let hash = hash_password(req.password).await?;

    let id = st.ids.next();
    // Первый аккаунт на сервере — его администратор
    let res = sqlx::query("INSERT INTO users (id, username, password_hash, is_admin) VALUES ($1, $2, $3, NOT EXISTS (SELECT 1 FROM users))")
        .bind(id.0 as i64)
        .bind(&username)
        .bind(&hash)
        .execute(&st.db)
        .await;
    match res {
        Err(e) if is_unique_violation(&e) => Err(AppError::Conflict("username_taken", "это имя уже занято".into())),
        Err(e) => Err(e.into()),
        Ok(_) => {
            let user = User { id, username, display_name: None, avatar: None };
            // С кодом приглашения — сразу в сообщество. Не вышло (код истёк в эту секунду) —
            // аккаунт всё равно создан, вступить можно позже
            if let Some(code) = invite
                && let Err(e) = crate::guilds::join_by_invite(&st, &user, code).await
            {
                tracing::info!("регистрация с приглашением: не вступил: {e:?}");
            }
            Ok((StatusCode::CREATED, Json(user)))
        }
    }
}

/// Сведения для экрана входа: какой режим регистрации
pub async fn instance(State(st): State<Shared>) -> ApiResult<Json<InstanceInfo>> {
    Ok(Json(InstanceInfo { registration: registration_mode(&st).await? }))
}

pub async fn login(State(st): State<Shared>, ClientIp(ip): ClientIp, Json(req): Json<LoginRequest>) -> ApiResult<Json<LoginResponse>> {
    st.limiter.check(&format!("auth:{ip}"), ratelimit::AUTH).map_err(AppError::rate_limited)?;
    let row: Option<(i64, String, Option<String>, Option<i64>, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT id, username, display_name, avatar_id, password_hash, totp_enabled, totp_secret
           FROM users WHERE lower(username) = lower($1) AND NOT deleted",
    )
    .bind(req.username.trim())
    .fetch_optional(&st.db)
    .await?;

    // Одинаковый ответ для «нет пользователя» и «неверный пароль»: не подсказываем, какие имена есть
    let bad = || AppError::BadRequest("bad_credentials", "неверное имя или пароль".into());
    let Some((id, username, display_name, avatar, hash, totp_enabled, totp_secret)) = row else { return Err(bad()) };
    if !verify_password(req.password, hash).await? {
        return Err(bad());
    }
    // Пароль верный — теперь второй фактор (код спрашиваем только после пароля)
    if totp_enabled {
        let secret = totp_secret.unwrap_or_default();
        match req.totp.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            None => return Err(AppError::BadRequest("totp_required", "введите код из приложения-аутентификатора".into())),
            Some(code) if !crate::totp::check(&secret, code) => {
                return Err(AppError::BadRequest("bad_totp", "неверный код подтверждения".into()));
            }
            Some(_) => {}
        }
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

    Ok(Json(LoginResponse { token, user: user_from_row((id, username, display_name, avatar)) }))
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

pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization")?.to_str().ok()?.strip_prefix("Bearer ")
}

/// Пользователь по токену сессии (REST и шлюз)
pub async fn user_by_token(db: &PgPool, token: &str) -> ApiResult<Option<User>> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name, u.avatar_id FROM sessions s JOIN users u ON u.id = s.user_id
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

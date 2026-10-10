//! Свой аккаунт: имя, пароль, аватар, двухфакторная защита, удаление.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use feather_protocol::{ChangePasswordRequest, ConfirmRequest, Event, Snowflake, TotpCode, TotpSetup, UpdateMeRequest, User};

use crate::Shared;
use crate::auth::{AuthUser, bearer, hash_password, load_user, token_hash, validate_password, verify_password};
use crate::bus::Target;
use crate::error::{ApiResult, AppError};
use crate::ratelimit;

/// Аватар: до 8 МБ на входе, хранится квадратом 128×128 PNG
pub const MAX_AVATAR: usize = 8 * 1024 * 1024;
const AVATAR_SIZE: u32 = 128;

fn avatar_path(st: &Shared, user: Snowflake) -> std::path::PathBuf {
    st.cfg.data_dir.join("avatars").join(format!("{user}.png"))
}

/// Разослать новое имя/аватар всем, кто видит пользователя: его серверам, собеседникам и ему самому
pub async fn broadcast_user(st: &Shared, user: &User) -> ApiResult<()> {
    let guilds: Vec<(i64,)> = sqlx::query_as("SELECT guild_id FROM guild_members WHERE user_id = $1").bind(user.id.0 as i64).fetch_all(&st.db).await?;
    for (g,) in guilds {
        st.bus.publish(Target::Guild(Snowflake(g as u64)), Event::UserUpdate(user.clone()));
    }
    let peers: Vec<(i64,)> = sqlx::query_as(
        "SELECT DISTINCT b.user_id FROM channel_recipients a JOIN channel_recipients b ON a.channel_id = b.channel_id
          WHERE a.user_id = $1 AND b.user_id <> $1",
    )
    .bind(user.id.0 as i64)
    .fetch_all(&st.db)
    .await?;
    for (p,) in peers {
        st.bus.publish(Target::User(Snowflake(p as u64)), Event::UserUpdate(user.clone()));
    }
    st.bus.publish(Target::User(user.id), Event::UserUpdate(user.clone()));
    Ok(())
}

pub async fn update_me(State(st): State<Shared>, user: AuthUser, Json(req): Json<UpdateMeRequest>) -> ApiResult<Json<User>> {
    if let Some(name) = req.display_name {
        let name = name.trim();
        if name.chars().count() > 32 || name.chars().any(char::is_control) {
            return Err(AppError::BadRequest("bad_display_name", "имя: до 32 символов".into()));
        }
        sqlx::query("UPDATE users SET display_name = $1 WHERE id = $2")
            .bind((!name.is_empty()).then_some(name))
            .bind(user.id().0 as i64)
            .execute(&st.db)
            .await?;
    }
    let me = load_user(&st.db, user.id()).await?;
    broadcast_user(&st, &me).await?;
    Ok(Json(me))
}

/// Проверка пароля (и кода 2FA, если включена) перед опасными действиями
async fn confirm(st: &Shared, user: Snowflake, password: &str, totp: Option<&str>) -> ApiResult<()> {
    st.limiter.check(&format!("confirm:{user}"), ratelimit::AUTH).map_err(AppError::rate_limited)?;
    let (hash, enabled, secret): (String, bool, Option<String>) =
        sqlx::query_as("SELECT password_hash, totp_enabled, totp_secret FROM users WHERE id = $1").bind(user.0 as i64).fetch_one(&st.db).await?;
    if !verify_password(password.to_owned(), hash).await? {
        return Err(AppError::BadRequest("bad_password", "неверный пароль".into()));
    }
    if enabled && !totp.is_some_and(|c| crate::totp::check(secret.as_deref().unwrap_or_default(), c)) {
        return Err(AppError::BadRequest("bad_totp", "неверный код подтверждения".into()));
    }
    Ok(())
}

/// Смена пароля: остальные устройства выходят из аккаунта
pub async fn change_password(State(st): State<Shared>, user: AuthUser, headers: HeaderMap, Json(req): Json<ChangePasswordRequest>) -> ApiResult<StatusCode> {
    confirm(&st, user.id(), &req.current, None).await?;
    validate_password(&req.new)?;
    let hash = hash_password(req.new).await?;
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2").bind(&hash).bind(user.id().0 as i64).execute(&st.db).await?;
    let current = bearer(&headers).map(token_hash).unwrap_or_default();
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND token_hash <> $2").bind(user.id().0 as i64).bind(current).execute(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Уменьшить картинку до квадрата 128×128 (обрезка по центру) и сохранить в PNG
fn make_avatar(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let side = img.width().min(img.height());
    let img = img.crop_imm((img.width() - side) / 2, (img.height() - side) / 2, side, side);
    let img = img.resize_exact(AVATAR_SIZE, AVATAR_SIZE, image::imageops::FilterType::Lanczos3).to_rgba8();
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).ok()?;
    Some(out)
}

pub async fn put_avatar(State(st): State<Shared>, user: AuthUser, body: Bytes) -> ApiResult<Json<User>> {
    st.limiter.check(&format!("upload:{}", user.id()), ratelimit::UPLOADS).map_err(AppError::rate_limited)?;
    if body.is_empty() || body.len() > MAX_AVATAR {
        return Err(AppError::BadRequest("bad_size", "аватар: картинка до 8 МБ".into()));
    }
    let png = tokio::task::spawn_blocking(move || make_avatar(&body))
        .await
        .map_err(|e| AppError::Internal(e.to_string()))?
        .ok_or(AppError::BadRequest("bad_image", "это не картинка (нужен PNG, JPEG, GIF или WebP)".into()))?;
    let path = avatar_path(&st, user.id());
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| AppError::Internal(format!("каталог аватаров: {e}")))?;
    }
    tokio::fs::write(&path, png).await.map_err(|e| AppError::Internal(format!("запись аватара: {e}")))?;
    // Новая версия — новый адрес: клиенты не возьмут старую картинку из кэша
    sqlx::query("UPDATE users SET avatar_id = $1 WHERE id = $2").bind(st.ids.next().0 as i64).bind(user.id().0 as i64).execute(&st.db).await?;
    let me = load_user(&st.db, user.id()).await?;
    broadcast_user(&st, &me).await?;
    Ok(Json(me))
}

pub async fn delete_avatar(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<User>> {
    sqlx::query("UPDATE users SET avatar_id = NULL WHERE id = $1").bind(user.id().0 as i64).execute(&st.db).await?;
    let _ = tokio::fs::remove_file(avatar_path(&st, user.id())).await;
    let me = load_user(&st.db, user.id()).await?;
    broadcast_user(&st, &me).await?;
    Ok(Json(me))
}

/// Аватар — любому вошедшему пользователю (как в Discord: аватары не секрет)
pub async fn get_avatar(State(st): State<Shared>, _user: AuthUser, Path((uid, version)): Path<(Snowflake, Snowflake)>) -> ApiResult<Response> {
    let (current,): (Option<i64>,) = sqlx::query_as("SELECT avatar_id FROM users WHERE id = $1")
        .bind(uid.0 as i64)
        .fetch_optional(&st.db)
        .await?
        .ok_or(AppError::NotFound("аватар"))?;
    if current != Some(version.0 as i64) {
        return Err(AppError::NotFound("аватар"));
    }
    let bytes = tokio::fs::read(avatar_path(&st, uid)).await.map_err(|_| AppError::NotFound("аватар"))?;
    Ok((
        [(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "private, max-age=31536000, immutable")],
        bytes,
    )
        .into_response())
}

/// Шаг 1 настройки 2FA: новый секрет (ещё не включён — включится после проверки кода)
pub async fn totp_setup(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<TotpSetup>> {
    let (enabled,): (bool,) = sqlx::query_as("SELECT totp_enabled FROM users WHERE id = $1").bind(user.id().0 as i64).fetch_one(&st.db).await?;
    if enabled {
        return Err(AppError::Conflict("totp_enabled", "двухфакторная защита уже включена".into()));
    }
    let secret = crate::totp::new_secret();
    sqlx::query("UPDATE users SET totp_secret = $1 WHERE id = $2").bind(&secret).bind(user.id().0 as i64).execute(&st.db).await?;
    let uri = crate::totp::uri(&secret, &user.0.username, "Feather");
    Ok(Json(TotpSetup { secret, uri }))
}

/// Шаг 2: код из приложения совпал — включаем
pub async fn totp_enable(State(st): State<Shared>, user: AuthUser, Json(req): Json<TotpCode>) -> ApiResult<StatusCode> {
    st.limiter.check(&format!("confirm:{}", user.id()), ratelimit::AUTH).map_err(AppError::rate_limited)?;
    let (secret,): (Option<String>,) = sqlx::query_as("SELECT totp_secret FROM users WHERE id = $1").bind(user.id().0 as i64).fetch_one(&st.db).await?;
    let secret = secret.ok_or(AppError::BadRequest("totp_not_setup", "сначала начните настройку".into()))?;
    if !crate::totp::check(&secret, &req.code) {
        return Err(AppError::BadRequest("bad_totp", "неверный код: проверьте время на телефоне".into()));
    }
    sqlx::query("UPDATE users SET totp_enabled = true WHERE id = $1").bind(user.id().0 as i64).execute(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn totp_disable(State(st): State<Shared>, user: AuthUser, Json(req): Json<ConfirmRequest>) -> ApiResult<StatusCode> {
    confirm(&st, user.id(), &req.password, req.totp.as_deref()).await?;
    sqlx::query("UPDATE users SET totp_enabled = false, totp_secret = NULL WHERE id = $1").bind(user.id().0 as i64).execute(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Удалить аккаунт. Сообщения остаются (подписаны «Удалённый аккаунт»), всё личное стирается.
/// `drop_guilds` — удалить и сообщества пользователя (так делает администратор);
/// сам пользователь должен сначала передать или удалить их
pub async fn delete_user(st: &Shared, uid: Snowflake, drop_guilds: bool) -> ApiResult<()> {
    let owned: Vec<(i64,)> = sqlx::query_as("SELECT id FROM guilds WHERE owner_id = $1").bind(uid.0 as i64).fetch_all(&st.db).await?;
    if !owned.is_empty() && !drop_guilds {
        return Err(AppError::BadRequest("owns_guilds", "сначала передайте или удалите свои сообщества".into()));
    }
    for (g,) in owned {
        crate::guilds::remove_guild(st, Snowflake(g as u64)).await?;
    }
    let guilds: Vec<(i64,)> = sqlx::query_as("SELECT guild_id FROM guild_members WHERE user_id = $1").bind(uid.0 as i64).fetch_all(&st.db).await?;
    for (g,) in guilds {
        crate::roles::remove_member(st, Snowflake(g as u64), uid).await?;
    }
    sqlx::query(
        "UPDATE users SET username = 'deleted-' || id, display_name = 'Удалённый аккаунт', password_hash = '!',
                avatar_id = NULL, totp_secret = NULL, totp_enabled = false, is_admin = false, deleted = true
          WHERE id = $1",
    )
    .bind(uid.0 as i64)
    .execute(&st.db)
    .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(uid.0 as i64).execute(&st.db).await?;
    sqlx::query("DELETE FROM reactions WHERE user_id = $1").bind(uid.0 as i64).execute(&st.db).await?;
    let _ = tokio::fs::remove_file(avatar_path(st, uid)).await;
    if let Ok(user) = load_user(&st.db, uid).await {
        broadcast_user(st, &user).await?;
    }
    Ok(())
}

pub async fn delete_me(State(st): State<Shared>, user: AuthUser, Json(req): Json<ConfirmRequest>) -> ApiResult<StatusCode> {
    confirm(&st, user.id(), &req.password, req.totp.as_deref()).await?;
    delete_user(&st, user.id(), false).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Включена ли у меня 2FA (для экрана настроек)
pub async fn totp_status(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<bool>> {
    let (enabled,): (bool,) = sqlx::query_as("SELECT totp_enabled FROM users WHERE id = $1").bind(user.id().0 as i64).fetch_one(&st.db).await?;
    Ok(Json(enabled))
}

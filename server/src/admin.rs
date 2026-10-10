//! Панель администратора сервера Feather: пользователи и режим регистрации.
//! Администратор — первый зарегистрированный (users.is_admin).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use feather_protocol::{AdminSettings, AdminUser, RegistrationMode, Snowflake, TempPassword};

use crate::Shared;
use crate::auth::{AuthUser, hash_password, user_from_row};
use crate::error::{ApiResult, AppError};

pub async fn is_admin(st: &Shared, user: Snowflake) -> ApiResult<bool> {
    let row: Option<(bool,)> = sqlx::query_as("SELECT is_admin FROM users WHERE id = $1").bind(user.0 as i64).fetch_optional(&st.db).await?;
    Ok(row.is_some_and(|r| r.0))
}

async fn require_admin(st: &Shared, user: &AuthUser) -> ApiResult<()> {
    if is_admin(st, user.id()).await? { Ok(()) } else { Err(AppError::Forbidden) }
}

pub async fn list_users(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<Vec<AdminUser>>> {
    require_admin(&st, &user).await?;
    let rows: Vec<(i64, String, Option<String>, Option<i64>, bool, i64, i64, bool)> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name, u.avatar_id, u.is_admin,
                (extract(epoch FROM u.created_at) * 1000)::bigint,
                (SELECT count(*) FROM guild_members m WHERE m.user_id = u.id), u.totp_enabled
           FROM users u WHERE NOT u.deleted ORDER BY u.created_at",
    )
    .fetch_all(&st.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, un, dn, av, is_admin, created_at, guilds, totp_enabled)| AdminUser {
                user: user_from_row((id, un, dn, av)),
                is_admin,
                created_at,
                guilds,
                totp_enabled,
            })
            .collect(),
    ))
}

/// Удалить пользователя вместе с его сообществами
pub async fn delete_user(State(st): State<Shared>, user: AuthUser, Path(target): Path<Snowflake>) -> ApiResult<StatusCode> {
    require_admin(&st, &user).await?;
    if target == user.id() {
        return Err(AppError::BadRequest("self_delete", "себя удаляйте в настройках аккаунта".into()));
    }
    crate::account::delete_user(&st, target, true).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Сбросить пароль (забыл): временный пароль, все сеансы и 2FA сбрасываются
pub async fn reset_password(State(st): State<Shared>, user: AuthUser, Path(target): Path<Snowflake>) -> ApiResult<Json<TempPassword>> {
    require_admin(&st, &user).await?;
    const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut buf = [0u8; 12];
    getrandom::fill(&mut buf).map_err(|e| AppError::Internal(e.to_string()))?;
    let password: String = buf.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
    let hash = hash_password(password.clone()).await?;
    let updated = sqlx::query("UPDATE users SET password_hash = $1, totp_enabled = false, totp_secret = NULL WHERE id = $2 AND NOT deleted")
        .bind(&hash)
        .bind(target.0 as i64)
        .execute(&st.db)
        .await?
        .rows_affected();
    if updated == 0 {
        return Err(AppError::NotFound("пользователь"));
    }
    sqlx::query("DELETE FROM sessions WHERE user_id = $1").bind(target.0 as i64).execute(&st.db).await?;
    Ok(Json(TempPassword { password }))
}

pub async fn get_settings(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<AdminSettings>> {
    require_admin(&st, &user).await?;
    Ok(Json(AdminSettings { registration: crate::auth::registration_mode(&st).await? }))
}

pub async fn put_settings(State(st): State<Shared>, user: AuthUser, Json(req): Json<AdminSettings>) -> ApiResult<Json<AdminSettings>> {
    require_admin(&st, &user).await?;
    let value = match req.registration {
        RegistrationMode::Open => "open",
        RegistrationMode::Invite => "invite",
        RegistrationMode::Closed => "closed",
    };
    sqlx::query("INSERT INTO instance_settings (key, value) VALUES ('registration', $1) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value")
        .bind(value)
        .execute(&st.db)
        .await?;
    Ok(Json(AdminSettings { registration: req.registration }))
}

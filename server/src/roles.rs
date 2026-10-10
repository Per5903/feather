//! Роли, права, кик и бан (docs/04-data-model.md).
//!
//! Правила иерархии, как в Discord:
//! - владелец сервера может всё и его нельзя выгнать;
//! - управлять можно только ролями и людьми НИЖЕ своей высшей роли;
//! - выдать роли права, которых нет у тебя самого, нельзя.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use feather_protocol::{Event, GuildMember, Role, RoleRequest, Snowflake, perms};
use sqlx::PgPool;

use crate::Shared;
use crate::auth::{AuthUser, user_from_row};
use crate::bus::Target;
use crate::error::{ApiResult, AppError};

/// Права пользователя на сервере
#[derive(Clone, Copy, Debug)]
pub struct Perms {
    pub bits: u64,
    /// Позиция высшей роли (владелец — выше всех)
    pub top: i32,
    pub owner: bool,
}

impl Perms {
    pub fn has(&self, p: u64) -> bool {
        self.bits & p == p
    }

    pub fn require(self, p: u64) -> ApiResult<Self> {
        if self.has(p) { Ok(self) } else { Err(AppError::Forbidden) }
    }

    /// Может управлять тем, чья высшая роль на позиции `other_top`
    pub fn outranks(&self, other_top: i32) -> bool {
        self.owner || self.top > other_top
    }
}

/// Права участника; не участник — «сервер не найден»
pub async fn member_perms(db: &PgPool, guild: Snowflake, user: Snowflake) -> ApiResult<Perms> {
    let row: Option<(i64, i64, i64, i32, bool)> = sqlx::query_as(
        "SELECT g.owner_id,
                COALESCE((SELECT permissions FROM roles WHERE id = g.id), 0),
                COALESCE((SELECT bit_or(r.permissions) FROM member_roles mr JOIN roles r ON r.id = mr.role_id
                           WHERE mr.guild_id = g.id AND mr.user_id = $2), 0),
                COALESCE((SELECT max(r.position) FROM member_roles mr JOIN roles r ON r.id = mr.role_id
                           WHERE mr.guild_id = g.id AND mr.user_id = $2), 0),
                EXISTS (SELECT 1 FROM guild_members m WHERE m.guild_id = g.id AND m.user_id = $2)
           FROM guilds g WHERE g.id = $1",
    )
    .bind(guild.0 as i64)
    .bind(user.0 as i64)
    .fetch_optional(db)
    .await?;
    let Some((owner, everyone, roles, top, _member)) = row.filter(|r| r.4) else {
        return Err(AppError::NotFound("сервер"));
    };
    let owner = owner as u64 == user.0;
    Ok(Perms {
        bits: perms::resolve(owner, [everyone as u64, roles as u64]),
        top: if owner { i32::MAX } else { top },
        owner,
    })
}

type RoleRow = (i64, i64, String, i32, i64, i32);

fn role_from_row((id, guild, name, color, permissions, position): RoleRow) -> Role {
    Role {
        id: Snowflake(id as u64),
        guild_id: Snowflake(guild as u64),
        name,
        color: color as u32,
        permissions: permissions as u64,
        position,
    }
}

async fn load_role(db: &PgPool, guild: Snowflake, role: Snowflake) -> ApiResult<Role> {
    let row: Option<RoleRow> =
        sqlx::query_as("SELECT id, guild_id, name, color, permissions, position FROM roles WHERE id = $1 AND guild_id = $2")
            .bind(role.0 as i64)
            .bind(guild.0 as i64)
            .fetch_optional(db)
            .await?;
    row.map(role_from_row).ok_or(AppError::NotFound("роль"))
}

/// Высшая роль участника (для иерархии)
async fn member_top(db: &PgPool, guild: Snowflake, user: Snowflake) -> ApiResult<(i32, bool)> {
    let row: Option<(i32, bool)> = sqlx::query_as(
        "SELECT COALESCE((SELECT max(r.position) FROM member_roles mr JOIN roles r ON r.id = mr.role_id
                           WHERE mr.guild_id = $1 AND mr.user_id = $2), 0),
                (SELECT owner_id FROM guilds WHERE id = $1) = $2
           FROM guild_members WHERE guild_id = $1 AND user_id = $2",
    )
    .bind(guild.0 as i64)
    .bind(user.0 as i64)
    .fetch_optional(db)
    .await?;
    row.ok_or(AppError::NotFound("участник"))
}

/// Роль @everyone для нового сервера (в той же транзакции, что и сервер)
pub async fn create_everyone(tx: &mut sqlx::PgConnection, guild: Snowflake) -> ApiResult<()> {
    sqlx::query("INSERT INTO roles (id, guild_id, name, permissions, position) VALUES ($1, $1, '@everyone', $2, 0)")
        .bind(guild.0 as i64)
        .bind(perms::DEFAULT_EVERYONE as i64)
        .execute(tx)
        .await?;
    Ok(())
}

pub async fn list_roles(State(st): State<Shared>, user: AuthUser, Path(guild): Path<Snowflake>) -> ApiResult<Json<Vec<Role>>> {
    member_perms(&st.db, guild, user.id()).await?;
    let rows: Vec<RoleRow> =
        sqlx::query_as("SELECT id, guild_id, name, color, permissions, position FROM roles WHERE guild_id = $1 ORDER BY position DESC, id")
            .bind(guild.0 as i64)
            .fetch_all(&st.db)
            .await?;
    Ok(Json(rows.into_iter().map(role_from_row).collect()))
}

fn check_grant(me: &Perms, requested: u64) -> ApiResult<()> {
    if !me.owner && requested & !me.bits != 0 {
        return Err(AppError::BadRequest("perms_escalation", "нельзя выдать права, которых нет у вас".into()));
    }
    if requested & !perms::ALL != 0 {
        return Err(AppError::BadRequest("bad_perms", "неизвестные биты прав".into()));
    }
    Ok(())
}

pub async fn create_role(
    State(st): State<Shared>,
    user: AuthUser,
    Path(guild): Path<Snowflake>,
    Json(req): Json<RoleRequest>,
) -> ApiResult<(StatusCode, Json<Role>)> {
    let me = member_perms(&st.db, guild, user.id()).await?.require(perms::MANAGE_ROLES)?;
    let permissions = req.permissions.unwrap_or(0);
    check_grant(&me, permissions)?;
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty() && n.chars().count() <= 64).unwrap_or("новая роль");
    // Новая роль — над всеми, кем я могу управлять (владелец — над всеми ролями)
    let (max_below,): (i32,) = sqlx::query_as("SELECT COALESCE(max(position), 0) FROM roles WHERE guild_id = $1 AND position < $2")
        .bind(guild.0 as i64)
        .bind(me.top)
        .fetch_one(&st.db)
        .await?;
    let position = if me.owner { max_below + 1 } else { (max_below + 1).min(me.top - 1).max(1) };
    let role = Role { id: st.ids.next(), guild_id: guild, name: name.into(), color: req.color.unwrap_or(0) & 0xFF_FFFF, permissions, position };
    sqlx::query("INSERT INTO roles (id, guild_id, name, color, permissions, position) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(role.id.0 as i64)
        .bind(guild.0 as i64)
        .bind(&role.name)
        .bind(role.color as i32)
        .bind(role.permissions as i64)
        .bind(role.position)
        .execute(&st.db)
        .await?;
    st.bus.publish(Target::Guild(guild), Event::RoleCreate(role.clone()));
    Ok((StatusCode::CREATED, Json(role)))
}

pub async fn update_role(
    State(st): State<Shared>,
    user: AuthUser,
    Path((guild, role_id)): Path<(Snowflake, Snowflake)>,
    Json(req): Json<RoleRequest>,
) -> ApiResult<Json<Role>> {
    let me = member_perms(&st.db, guild, user.id()).await?.require(perms::MANAGE_ROLES)?;
    let mut role = load_role(&st.db, guild, role_id).await?;
    let everyone = role.id == guild;
    if !everyone && !me.outranks(role.position) {
        return Err(AppError::Forbidden);
    }
    if let Some(p) = req.permissions {
        check_grant(&me, p)?;
        role.permissions = p;
    }
    if !everyone {
        if let Some(n) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            role.name = n.chars().take(64).collect();
        }
        if let Some(c) = req.color {
            role.color = c & 0xFF_FFFF;
        }
        if let Some(pos) = req.position {
            if !me.outranks(pos) || pos < 1 {
                return Err(AppError::Forbidden);
            }
            role.position = pos;
        }
    }
    sqlx::query("UPDATE roles SET name = $1, color = $2, permissions = $3, position = $4 WHERE id = $5")
        .bind(&role.name)
        .bind(role.color as i32)
        .bind(role.permissions as i64)
        .bind(role.position)
        .bind(role.id.0 as i64)
        .execute(&st.db)
        .await?;
    st.bus.publish(Target::Guild(guild), Event::RoleUpdate(role.clone()));
    Ok(Json(role))
}

pub async fn delete_role(State(st): State<Shared>, user: AuthUser, Path((guild, role_id)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    let me = member_perms(&st.db, guild, user.id()).await?.require(perms::MANAGE_ROLES)?;
    let role = load_role(&st.db, guild, role_id).await?;
    if role.id == guild {
        return Err(AppError::BadRequest("everyone", "@everyone удалить нельзя".into()));
    }
    if !me.outranks(role.position) {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM roles WHERE id = $1").bind(role.id.0 as i64).execute(&st.db).await?;
    st.bus.publish(Target::Guild(guild), Event::RoleDelete { guild_id: guild, role_id: role.id });
    Ok(StatusCode::NO_CONTENT)
}

async fn member_roles(db: &PgPool, guild: Snowflake, user: Snowflake) -> ApiResult<Vec<Snowflake>> {
    let rows: Vec<(i64,)> = sqlx::query_as("SELECT role_id FROM member_roles WHERE guild_id = $1 AND user_id = $2 ORDER BY role_id")
        .bind(guild.0 as i64)
        .bind(user.0 as i64)
        .fetch_all(db)
        .await?;
    Ok(rows.into_iter().map(|(r,)| Snowflake(r as u64)).collect())
}

/// Выдать (`add = true`) или снять роль
async fn set_member_role(st: &Shared, actor: Snowflake, guild: Snowflake, target: Snowflake, role_id: Snowflake, add: bool) -> ApiResult<StatusCode> {
    let me = member_perms(&st.db, guild, actor).await?.require(perms::MANAGE_ROLES)?;
    let role = load_role(&st.db, guild, role_id).await?;
    if role.id == guild || !me.outranks(role.position) {
        return Err(AppError::Forbidden);
    }
    member_top(&st.db, guild, target).await?; // цель — участник сервера
    let q = if add {
        "INSERT INTO member_roles (guild_id, user_id, role_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING"
    } else {
        "DELETE FROM member_roles WHERE guild_id = $1 AND user_id = $2 AND role_id = $3"
    };
    sqlx::query(q).bind(guild.0 as i64).bind(target.0 as i64).bind(role_id.0 as i64).execute(&st.db).await?;
    let roles = member_roles(&st.db, guild, target).await?;
    st.bus.publish(Target::Guild(guild), Event::GuildMemberUpdate { guild_id: guild, user_id: target, roles });
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_member_role(
    State(st): State<Shared>,
    user: AuthUser,
    Path((guild, target, role)): Path<(Snowflake, Snowflake, Snowflake)>,
) -> ApiResult<StatusCode> {
    set_member_role(&st, user.id(), guild, target, role, true).await
}

pub async fn remove_member_role(
    State(st): State<Shared>,
    user: AuthUser,
    Path((guild, target, role)): Path<(Snowflake, Snowflake, Snowflake)>,
) -> ApiResult<StatusCode> {
    set_member_role(&st, user.id(), guild, target, role, false).await
}

pub async fn list_members(State(st): State<Shared>, user: AuthUser, Path(guild): Path<Snowflake>) -> ApiResult<Json<Vec<GuildMember>>> {
    member_perms(&st.db, guild, user.id()).await?;
    let rows: Vec<(i64, String, Option<String>, Vec<i64>)> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name,
                COALESCE(array_agg(mr.role_id ORDER BY mr.role_id) FILTER (WHERE mr.role_id IS NOT NULL), '{}')
           FROM guild_members m JOIN users u ON u.id = m.user_id
           LEFT JOIN member_roles mr ON mr.guild_id = m.guild_id AND mr.user_id = m.user_id
          WHERE m.guild_id = $1
          GROUP BY u.id ORDER BY lower(u.username)",
    )
    .bind(guild.0 as i64)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, username, dn, roles)| GuildMember {
                user: user_from_row((id, username, dn)),
                roles: roles.into_iter().map(|r| Snowflake(r as u64)).collect(),
            })
            .collect(),
    ))
}

/// Убрать участника с сервера и разослать события (кик, бан, выход)
async fn remove_member(st: &Shared, guild: Snowflake, target: Snowflake) -> ApiResult<()> {
    sqlx::query("DELETE FROM guild_members WHERE guild_id = $1 AND user_id = $2")
        .bind(guild.0 as i64)
        .bind(target.0 as i64)
        .execute(&st.db)
        .await?;
    st.bus.publish(Target::Guild(guild), Event::GuildMemberRemove { guild_id: guild, user_id: target });
    st.bus.publish(Target::User(target), Event::GuildDelete { guild_id: guild });
    // Если сидел в голосовом канале этого сервера — убрать и оттуда
    if let Some(left) = st.voice.remove_in_guild(target, guild) {
        st.bus.publish(Target::Guild(guild), Event::VoiceStateUpdate(left));
    }
    Ok(())
}

/// Кик (или выход, если `target` — это я)
pub async fn kick(State(st): State<Shared>, user: AuthUser, Path((guild, target)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    let (target_top, target_owner) = member_top(&st.db, guild, target).await?;
    if target == user.id() {
        if target_owner {
            return Err(AppError::BadRequest("owner_leave", "владелец не может покинуть свой сервер".into()));
        }
    } else {
        let me = member_perms(&st.db, guild, user.id()).await?.require(perms::KICK_MEMBERS)?;
        if target_owner || !me.outranks(target_top) {
            return Err(AppError::Forbidden);
        }
    }
    remove_member(&st, guild, target).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn ban(State(st): State<Shared>, user: AuthUser, Path((guild, target)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    let me = member_perms(&st.db, guild, user.id()).await?.require(perms::BAN_MEMBERS)?;
    if target == user.id() {
        return Err(AppError::BadRequest("self_ban", "нельзя забанить себя".into()));
    }
    // Забанить можно и того, кто уже вышел; иерархию проверяем, только если он ещё участник
    if let Ok((top, owner)) = member_top(&st.db, guild, target).await {
        if owner || !me.outranks(top) {
            return Err(AppError::Forbidden);
        }
        remove_member(&st, guild, target).await?;
    }
    sqlx::query("INSERT INTO bans (guild_id, user_id, banned_by) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
        .bind(guild.0 as i64)
        .bind(target.0 as i64)
        .bind(user.id().0 as i64)
        .execute(&st.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unban(State(st): State<Shared>, user: AuthUser, Path((guild, target)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    member_perms(&st.db, guild, user.id()).await?.require(perms::BAN_MEMBERS)?;
    sqlx::query("DELETE FROM bans WHERE guild_id = $1 AND user_id = $2").bind(guild.0 as i64).bind(target.0 as i64).execute(&st.db).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn is_banned(db: &PgPool, guild: i64, user: Snowflake) -> ApiResult<bool> {
    let (b,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM bans WHERE guild_id = $1 AND user_id = $2)")
        .bind(guild)
        .bind(user.0 as i64)
        .fetch_one(db)
        .await?;
    Ok(b)
}

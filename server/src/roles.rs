//! Роли, права, кик и бан (docs/04-data-model.md).
//!
//! Правила иерархии, как в Discord:
//! - владелец сервера может всё и его нельзя выгнать;
//! - управлять можно только ролями и людьми НИЖЕ своей высшей роли;
//! - выдать роли права, которых нет у тебя самого, нельзя.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use feather_protocol::{Ban, Channel, ChannelKind, Event, GuildMember, Overwrite, OverwriteKind, OverwriteRequest, Role, RoleRequest, Snowflake, perms};
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

/// Права в конкретном канале: права на сервере с учётом настроек канала.
/// Ветка берёт настройки родительского канала; канал без своих настроек — настройки категории
pub async fn channel_perms(db: &PgPool, channel: &Channel, user: Snowflake) -> ApiResult<Perms> {
    let mut p = member_perms(db, channel.guild_id, user).await?;
    if p.owner || p.bits & perms::ADMINISTRATOR != 0 {
        return Ok(p);
    }
    // Цепочка: сам канал (или родитель ветки) → его категория; берём первый, у кого есть настройки
    let mut source = if channel.kind == ChannelKind::Thread { channel.parent_id.unwrap_or(channel.id) } else { channel.id };
    let rows: Vec<(i64, String, i64, i64)> = loop {
        let rows: Vec<(i64, String, i64, i64)> = sqlx::query_as(
            "SELECT o.target_id, o.kind, o.allow, o.deny FROM channel_overwrites o
              WHERE o.channel_id = $1 AND (
                    o.target_id = $2 OR o.target_id = $3
                 OR (o.kind = 'role' AND o.target_id IN (SELECT role_id FROM member_roles WHERE guild_id = $3 AND user_id = $2)))",
        )
        .bind(source.0 as i64)
        .bind(user.0 as i64)
        .bind(channel.guild_id.0 as i64)
        .fetch_all(db)
        .await?;
        if !rows.is_empty() {
            break rows;
        }
        // Есть ли у источника вообще настройки (может, не для меня) — тогда наследовать нечего
        let (any,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM channel_overwrites WHERE channel_id = $1)")
            .bind(source.0 as i64)
            .fetch_one(db)
            .await?;
        let parent: Option<(Option<i64>,)> = sqlx::query_as("SELECT parent_id FROM channels WHERE id = $1 AND kind <> 'category'")
            .bind(source.0 as i64)
            .fetch_optional(db)
            .await?;
        match parent.and_then(|p| p.0) {
            Some(cat) if !any => source = Snowflake(cat as u64),
            _ => return Ok(p),
        }
    };
    let guild = channel.guild_id.0 as i64;
    let mut everyone = None;
    let mut member = None;
    let mut roles = (0u64, 0u64);
    for (target, kind, allow, deny) in rows {
        let pair = (allow as u64, deny as u64);
        match kind.as_str() {
            "member" if target == user.0 as i64 => member = Some(pair),
            "role" if target == guild => everyone = Some(pair),
            "role" => {
                roles.0 |= pair.0;
                roles.1 |= pair.1;
            }
            _ => {}
        }
    }
    p.bits = perms::apply_overwrites(p.bits, everyone, roles, member);
    Ok(p)
}

/// Права всех каналов сервера на сервере, разом — для списка каналов и непрочитанного.
/// Возвращает id каналов, которые пользователь видит
pub async fn visible_channels(db: &PgPool, guild: Snowflake, user: Snowflake, channels: &[Channel]) -> ApiResult<Vec<Snowflake>> {
    let base = member_perms(db, guild, user).await?;
    if base.owner || base.bits & perms::ADMINISTRATOR != 0 {
        return Ok(channels.iter().map(|c| c.id).collect());
    }
    let my_roles: Vec<(i64,)> = sqlx::query_as("SELECT role_id FROM member_roles WHERE guild_id = $1 AND user_id = $2")
        .bind(guild.0 as i64)
        .bind(user.0 as i64)
        .fetch_all(db)
        .await?;
    let my_roles: std::collections::HashSet<u64> = my_roles.into_iter().map(|(r,)| r as u64).collect();
    let mut out = Vec::new();
    for c in channels {
        // Та же цепочка, что в channel_perms: ветка → её канал → категория без своих настроек
        let find = |id: Option<Snowflake>| id.and_then(|id| channels.iter().find(|p| p.id == id));
        let mut source = if c.kind == ChannelKind::Thread { find(c.parent_id).unwrap_or(c) } else { c };
        if source.overwrites.is_empty()
            && source.kind != ChannelKind::Category
            && let Some(cat) = find(source.parent_id)
        {
            source = cat;
        }
        let (mut everyone, mut member, mut roles) = (None, None, (0u64, 0u64));
        for o in &source.overwrites {
            match o.kind {
                OverwriteKind::Member if o.id == user => member = Some((o.allow, o.deny)),
                OverwriteKind::Role if o.id == guild => everyone = Some((o.allow, o.deny)),
                OverwriteKind::Role if my_roles.contains(&o.id.0) => {
                    roles.0 |= o.allow;
                    roles.1 |= o.deny;
                }
                _ => {}
            }
        }
        if perms::apply_overwrites(base.bits, everyone, roles, member) & perms::VIEW_CHANNEL != 0 {
            out.push(c.id);
        }
    }
    Ok(out)
}

/// Настройки прав канала (для выдачи вместе с каналом)
pub async fn load_overwrites(db: &PgPool, channel_ids: &[i64]) -> ApiResult<std::collections::HashMap<i64, Vec<Overwrite>>> {
    let rows: Vec<(i64, i64, String, i64, i64)> =
        sqlx::query_as("SELECT channel_id, target_id, kind, allow, deny FROM channel_overwrites WHERE channel_id = ANY($1) ORDER BY target_id")
            .bind(channel_ids)
            .fetch_all(db)
            .await?;
    let mut map: std::collections::HashMap<i64, Vec<Overwrite>> = Default::default();
    for (c, t, kind, allow, deny) in rows {
        map.entry(c).or_default().push(Overwrite {
            id: Snowflake(t as u64),
            kind: if kind == "member" { OverwriteKind::Member } else { OverwriteKind::Role },
            allow: allow as u64,
            deny: deny as u64,
        });
    }
    Ok(map)
}

pub async fn put_overwrite(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, target)): Path<(Snowflake, Snowflake)>,
    Json(req): Json<OverwriteRequest>,
) -> ApiResult<Json<Channel>> {
    let (channel, _) = crate::guilds::channel_for_member(&st.db, channel_id, user.id()).await?;
    if matches!(channel.kind, ChannelKind::Dm | ChannelKind::Thread) {
        return Err(AppError::BadRequest("bad_channel", "права задаются для каналов и категорий".into()));
    }
    let me = member_perms(&st.db, channel.guild_id, user.id()).await?.require(perms::MANAGE_CHANNELS | perms::MANAGE_ROLES)?;
    if (req.allow | req.deny) & !perms::CHANNEL_SCOPED != 0 {
        return Err(AppError::BadRequest("bad_perms", "в канале настраиваются только права канала".into()));
    }
    check_grant(&me, req.allow)?;
    match req.kind {
        OverwriteKind::Role => {
            let role = load_role(&st.db, channel.guild_id, target).await?;
            if role.id != channel.guild_id && !me.outranks(role.position) {
                return Err(AppError::Forbidden);
            }
        }
        OverwriteKind::Member => {
            member_top(&st.db, channel.guild_id, target).await?;
        }
    }
    sqlx::query(
        "INSERT INTO channel_overwrites (channel_id, target_id, kind, allow, deny) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (channel_id, target_id) DO UPDATE SET kind = EXCLUDED.kind, allow = EXCLUDED.allow, deny = EXCLUDED.deny",
    )
    .bind(channel_id.0 as i64)
    .bind(target.0 as i64)
    .bind(if req.kind == OverwriteKind::Member { "member" } else { "role" })
    .bind(req.allow as i64)
    .bind((req.deny & !req.allow) as i64)
    .execute(&st.db)
    .await?;
    crate::audit::log(&st, channel.guild_id, user.id(), "overwrite_update", Some(channel_id), format!("#{}", channel.name)).await;
    overwrites_changed(&st, channel_id, channel.guild_id).await
}

pub async fn delete_overwrite(State(st): State<Shared>, user: AuthUser, Path((channel_id, target)): Path<(Snowflake, Snowflake)>) -> ApiResult<Json<Channel>> {
    let (channel, _) = crate::guilds::channel_for_member(&st.db, channel_id, user.id()).await?;
    member_perms(&st.db, channel.guild_id, user.id()).await?.require(perms::MANAGE_CHANNELS | perms::MANAGE_ROLES)?;
    sqlx::query("DELETE FROM channel_overwrites WHERE channel_id = $1 AND target_id = $2")
        .bind(channel_id.0 as i64)
        .bind(target.0 as i64)
        .execute(&st.db)
        .await?;
    crate::audit::log(&st, channel.guild_id, user.id(), "overwrite_delete", Some(channel_id), format!("#{}", channel.name)).await;
    overwrites_changed(&st, channel_id, channel.guild_id).await
}

async fn overwrites_changed(st: &Shared, channel_id: Snowflake, guild: Snowflake) -> ApiResult<Json<Channel>> {
    let channel = crate::guilds::load_channel(&st.db, channel_id).await?;
    st.bus.publish(Target::Guild(guild), Event::ChannelsInvalidated { guild_id: guild });
    Ok(Json(channel))
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
    crate::audit::log(&st, guild, user.id(), "role_create", Some(role.id), &role.name).await;
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
    st.bus.publish(Target::Guild(guild), Event::ChannelsInvalidated { guild_id: guild });
    crate::audit::log(&st, guild, user.id(), "role_update", Some(role.id), &role.name).await;
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
    sqlx::query("DELETE FROM channel_overwrites WHERE target_id = $1").bind(role.id.0 as i64).execute(&st.db).await?;
    st.bus.publish(Target::Guild(guild), Event::RoleDelete { guild_id: guild, role_id: role.id });
    st.bus.publish(Target::Guild(guild), Event::ChannelsInvalidated { guild_id: guild });
    crate::audit::log(&st, guild, user.id(), "role_delete", Some(role.id), &role.name).await;
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
    st.bus.publish(Target::User(target), Event::ChannelsInvalidated { guild_id: guild });
    crate::audit::log(st, guild, actor, if add { "member_role_add" } else { "member_role_remove" }, Some(target), &role.name).await;
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
    let rows: Vec<(i64, String, Option<String>, Option<i64>, Vec<i64>)> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name, u.avatar_id,
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
            .map(|(id, username, dn, avatar, roles)| GuildMember {
                user: user_from_row((id, username, dn, avatar)),
                roles: roles.into_iter().map(|r| Snowflake(r as u64)).collect(),
            })
            .collect(),
    ))
}

/// Убрать участника с сервера и разослать события (кик, бан, выход)
pub async fn remove_member(st: &Shared, guild: Snowflake, target: Snowflake) -> ApiResult<()> {
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
    if target != user.id() {
        let name = crate::auth::load_user(&st.db, target).await.map(|u| u.username).unwrap_or_default();
        crate::audit::log(&st, guild, user.id(), "kick", Some(target), name).await;
    }
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
    let name = crate::auth::load_user(&st.db, target).await.map(|u| u.username).unwrap_or_default();
    crate::audit::log(&st, guild, user.id(), "ban", Some(target), name).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn unban(State(st): State<Shared>, user: AuthUser, Path((guild, target)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    member_perms(&st.db, guild, user.id()).await?.require(perms::BAN_MEMBERS)?;
    let removed = sqlx::query("DELETE FROM bans WHERE guild_id = $1 AND user_id = $2")
        .bind(guild.0 as i64)
        .bind(target.0 as i64)
        .execute(&st.db)
        .await?
        .rows_affected();
    if removed > 0 {
        let name = crate::auth::load_user(&st.db, target).await.map(|u| u.username).unwrap_or_default();
        crate::audit::log(&st, guild, user.id(), "unban", Some(target), name).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_bans(State(st): State<Shared>, user: AuthUser, Path(guild): Path<Snowflake>) -> ApiResult<Json<Vec<Ban>>> {
    member_perms(&st.db, guild, user.id()).await?.require(perms::BAN_MEMBERS)?;
    let rows: Vec<crate::auth::UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name, u.avatar_id FROM bans b JOIN users u ON u.id = b.user_id
          WHERE b.guild_id = $1 ORDER BY b.created_at DESC",
    )
    .bind(guild.0 as i64)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows.into_iter().map(|r| Ban { user: user_from_row(r) }).collect()))
}

pub async fn is_banned(db: &PgPool, guild: i64, user: Snowflake) -> ApiResult<bool> {
    let (b,): (bool,) = sqlx::query_as("SELECT EXISTS (SELECT 1 FROM bans WHERE guild_id = $1 AND user_id = $2)")
        .bind(guild)
        .bind(user.0 as i64)
        .fetch_one(db)
        .await?;
    Ok(b)
}

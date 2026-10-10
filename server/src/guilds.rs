//! Серверы (гильдии), каналы, участники, инвайты.
//!
//! Права пока простые: участник видит всё, владелец создаёт каналы.
//! Роли и битовые маски прав — следующий шаг этапа 1 (docs/04-data-model.md).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use feather_protocol::{
    Channel, ChannelKind, CreateChannelRequest, CreateGuildRequest, CreateInviteRequest, Event, Guild,
    GuildWithChannels, Invite, Snowflake, User,
};
use sqlx::PgPool;

use crate::Shared;
use crate::auth::{AuthUser, UserRow, user_from_row};
use crate::bus::Target;
use crate::error::{ApiResult, AppError};

type GuildRow = (i64, String, i64);
type ChannelRow = (i64, i64, String, String, Option<i64>, i32);
/// Канал + владелец его сервера
type ChannelOwnerRow = (i64, i64, String, String, Option<i64>, i32, i64);

fn guild_from_row((id, name, owner): GuildRow) -> Guild {
    Guild { id: Snowflake(id as u64), name, owner_id: Snowflake(owner as u64) }
}

fn kind_str(k: ChannelKind) -> &'static str {
    match k {
        ChannelKind::Text => "text",
        ChannelKind::Voice => "voice",
        ChannelKind::Category => "category",
        ChannelKind::Dm => "dm",
    }
}

pub fn channel_from_row((id, guild_id, kind, name, parent_id, position): ChannelRow) -> Channel {
    Channel {
        id: Snowflake(id as u64),
        guild_id: Snowflake(guild_id as u64),
        kind: match kind.as_str() {
            "voice" => ChannelKind::Voice,
            "category" => ChannelKind::Category,
            "dm" => ChannelKind::Dm,
            _ => ChannelKind::Text,
        },
        name,
        parent_id: parent_id.map(|p| Snowflake(p as u64)),
        position,
    }
}

fn validate_name(name: &str, what: &'static str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 {
        return Err(AppError::BadRequest("bad_name", format!("{what}: от 1 до 100 символов")));
    }
    Ok(name.to_owned())
}

/// Проверка членства; возвращает владельца сервера
pub async fn require_member(db: &PgPool, guild_id: Snowflake, user_id: Snowflake) -> ApiResult<Snowflake> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT g.owner_id FROM guilds g JOIN guild_members m ON m.guild_id = g.id
         WHERE g.id = $1 AND m.user_id = $2",
    )
    .bind(guild_id.0 as i64)
    .bind(user_id.0 as i64)
    .fetch_optional(db)
    .await?;
    // Чужой сервер выглядит как несуществующий: не раскрываем, что он есть
    row.map(|(o,)| Snowflake(o as u64)).ok_or(AppError::NotFound("сервер"))
}

/// Канал + проверка, что пользователь состоит в его сервере
pub async fn channel_for_member(db: &PgPool, channel_id: Snowflake, user_id: Snowflake) -> ApiResult<(Channel, Snowflake)> {
    let row: Option<ChannelOwnerRow> = sqlx::query_as(
        "SELECT c.id, c.guild_id, c.kind, c.name, c.parent_id, c.position, g.owner_id
         FROM channels c JOIN guilds g ON g.id = c.guild_id
         JOIN guild_members m ON m.guild_id = c.guild_id AND m.user_id = $2
         WHERE c.id = $1",
    )
    .bind(channel_id.0 as i64)
    .bind(user_id.0 as i64)
    .fetch_optional(db)
    .await?;
    let (id, gid, kind, name, parent, pos, owner) = row.ok_or(AppError::NotFound("канал"))?;
    Ok((channel_from_row((id, gid, kind, name, parent, pos)), Snowflake(owner as u64)))
}

pub async fn user_guilds(db: &PgPool, user_id: Snowflake) -> ApiResult<Vec<Guild>> {
    let rows: Vec<GuildRow> = sqlx::query_as(
        "SELECT g.id, g.name, g.owner_id FROM guilds g JOIN guild_members m ON m.guild_id = g.id
         WHERE m.user_id = $1 ORDER BY m.joined_at",
    )
    .bind(user_id.0 as i64)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().map(guild_from_row).collect())
}

pub async fn create_guild(
    State(st): State<Shared>,
    user: AuthUser,
    Json(req): Json<CreateGuildRequest>,
) -> ApiResult<(StatusCode, Json<GuildWithChannels>)> {
    let name = validate_name(&req.name, "название сервера")?;
    let guild = Guild { id: st.ids.next(), name, owner_id: user.id() };

    // Каналы по умолчанию, как у Discord: текстовый и голосовой в своих категориях
    let text_cat = st.ids.next();
    let voice_cat = st.ids.next();
    let defaults = [
        (text_cat, ChannelKind::Category, "Текстовые каналы", None, 0),
        (st.ids.next(), ChannelKind::Text, "общий", Some(text_cat), 0),
        (voice_cat, ChannelKind::Category, "Голосовые каналы", None, 1),
        (st.ids.next(), ChannelKind::Voice, "Лобби", Some(voice_cat), 0),
    ];

    let mut tx = st.db.begin().await?;
    sqlx::query("INSERT INTO guilds (id, name, owner_id) VALUES ($1, $2, $3)")
        .bind(guild.id.0 as i64)
        .bind(&guild.name)
        .bind(guild.owner_id.0 as i64)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO guild_members (guild_id, user_id) VALUES ($1, $2)")
        .bind(guild.id.0 as i64)
        .bind(user.id().0 as i64)
        .execute(&mut *tx)
        .await?;
    let mut channels = Vec::with_capacity(defaults.len());
    for (id, kind, name, parent, position) in defaults {
        sqlx::query("INSERT INTO channels (id, guild_id, kind, name, parent_id, position) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(id.0 as i64)
            .bind(guild.id.0 as i64)
            .bind(kind_str(kind))
            .bind(name)
            .bind(parent.map(|p: Snowflake| p.0 as i64))
            .bind(position)
            .execute(&mut *tx)
            .await?;
        channels.push(Channel { id, guild_id: guild.id, kind, name: name.into(), parent_id: parent, position });
    }
    tx.commit().await?;

    st.bus.publish(Target::User(user.id()), Event::GuildCreate(guild.clone()));
    Ok((StatusCode::CREATED, Json(GuildWithChannels { guild, channels })))
}

pub async fn my_guilds(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<Vec<Guild>>> {
    Ok(Json(user_guilds(&st.db, user.id()).await?))
}

pub async fn get_guild(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<Json<Guild>> {
    require_member(&st.db, id, user.id()).await?;
    let row: GuildRow = sqlx::query_as("SELECT id, name, owner_id FROM guilds WHERE id = $1")
        .bind(id.0 as i64)
        .fetch_one(&st.db)
        .await?;
    Ok(Json(guild_from_row(row)))
}

pub async fn list_channels(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<Json<Vec<Channel>>> {
    require_member(&st.db, id, user.id()).await?;
    let rows: Vec<ChannelRow> = sqlx::query_as(
        "SELECT id, guild_id, kind, name, parent_id, position FROM channels WHERE guild_id = $1 ORDER BY position, id",
    )
    .bind(id.0 as i64)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows.into_iter().map(channel_from_row).collect()))
}

pub async fn create_channel(
    State(st): State<Shared>,
    user: AuthUser,
    Path(guild_id): Path<Snowflake>,
    Json(req): Json<CreateChannelRequest>,
) -> ApiResult<(StatusCode, Json<Channel>)> {
    let owner = require_member(&st.db, guild_id, user.id()).await?;
    if owner != user.id() {
        return Err(AppError::Forbidden);
    }
    let name = validate_name(&req.name, "название канала")?;
    if req.kind == ChannelKind::Dm {
        return Err(AppError::BadRequest("bad_kind", "личный канал создаётся через /users/@me/channels".into()));
    }
    if let Some(parent) = req.parent_id {
        let ok: Option<(i64,)> = sqlx::query_as("SELECT id FROM channels WHERE id = $1 AND guild_id = $2 AND kind = 'category'")
            .bind(parent.0 as i64)
            .bind(guild_id.0 as i64)
            .fetch_optional(&st.db)
            .await?;
        if ok.is_none() || req.kind == ChannelKind::Category {
            return Err(AppError::BadRequest("bad_parent", "родитель должен быть категорией этого сервера".into()));
        }
    }
    let (position,): (i32,) = sqlx::query_as("SELECT COALESCE(MAX(position) + 1, 0) FROM channels WHERE guild_id = $1 AND parent_id IS NOT DISTINCT FROM $2")
        .bind(guild_id.0 as i64)
        .bind(req.parent_id.map(|p| p.0 as i64))
        .fetch_one(&st.db)
        .await?;
    let channel = Channel { id: st.ids.next(), guild_id, kind: req.kind, name, parent_id: req.parent_id, position };
    sqlx::query("INSERT INTO channels (id, guild_id, kind, name, parent_id, position) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(channel.id.0 as i64)
        .bind(guild_id.0 as i64)
        .bind(kind_str(channel.kind))
        .bind(&channel.name)
        .bind(channel.parent_id.map(|p| p.0 as i64))
        .bind(position)
        .execute(&st.db)
        .await?;
    st.bus.publish(Target::Guild(guild_id), Event::ChannelCreate(channel.clone()));
    Ok((StatusCode::CREATED, Json(channel)))
}

pub async fn list_members(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<Json<Vec<User>>> {
    require_member(&st.db, id, user.id()).await?;
    let rows: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name FROM guild_members m JOIN users u ON u.id = m.user_id
         WHERE m.guild_id = $1 ORDER BY lower(u.username)",
    )
    .bind(id.0 as i64)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows.into_iter().map(user_from_row).collect()))
}

/// Код приглашения: 8 символов без похожих (0/O, 1/l/I)
fn invite_code() -> String {
    const ALPHABET: &[u8] = b"23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("нет источника случайности");
    buf.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect()
}

pub async fn create_invite(
    State(st): State<Shared>,
    user: AuthUser,
    Path(guild_id): Path<Snowflake>,
    body: Option<Json<CreateInviteRequest>>,
) -> ApiResult<(StatusCode, Json<Invite>)> {
    require_member(&st.db, guild_id, user.id()).await?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    if req.max_uses.is_some_and(|m| m < 1) || req.ttl_secs.is_some_and(|t| t < 60) {
        return Err(AppError::BadRequest("bad_invite", "max_uses ≥ 1, ttl_secs ≥ 60".into()));
    }
    let code = invite_code();
    sqlx::query(
        "INSERT INTO invites (code, guild_id, creator_id, max_uses, expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))",
    )
    .bind(&code)
    .bind(guild_id.0 as i64)
    .bind(user.id().0 as i64)
    .bind(req.max_uses)
    .bind(req.ttl_secs.map(|t| t as f64))
    .execute(&st.db)
    .await?;
    Ok((StatusCode::CREATED, Json(Invite { code, guild_id, uses: 0, max_uses: req.max_uses })))
}

pub async fn accept_invite(State(st): State<Shared>, user: AuthUser, Path(code): Path<String>) -> ApiResult<Json<Guild>> {
    let mut tx = st.db.begin().await?;
    // FOR UPDATE: два одновременных входа не превысят max_uses
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT guild_id FROM invites WHERE code = $1
         AND (expires_at IS NULL OR expires_at > now()) AND (max_uses IS NULL OR uses < max_uses)
         FOR UPDATE",
    )
    .bind(&code)
    .fetch_optional(&mut *tx)
    .await?;
    let (guild_id,) = row.ok_or(AppError::NotFound("приглашение"))?;
    let inserted = sqlx::query("INSERT INTO guild_members (guild_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(guild_id)
        .bind(user.id().0 as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        == 1;
    if inserted {
        sqlx::query("UPDATE invites SET uses = uses + 1 WHERE code = $1").bind(&code).execute(&mut *tx).await?;
    }
    let guild: GuildRow = sqlx::query_as("SELECT id, name, owner_id FROM guilds WHERE id = $1")
        .bind(guild_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;

    let guild = guild_from_row(guild);
    if inserted {
        st.bus.publish(Target::Guild(guild.id), Event::GuildMemberAdd { guild_id: guild.id, user: user.0.clone() });
        st.bus.publish(Target::User(user.id()), Event::GuildCreate(guild.clone()));
    }
    Ok(Json(guild))
}

//! Серверы (гильдии), каналы, участники, инвайты.
//!
//! Права — роли (roles.rs) и настройки каналов: закрытые каналы видны не всем.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use feather_protocol::{
    Channel, ChannelKind, CreateChannelRequest, CreateGuildRequest, CreateInviteRequest, CreateThreadRequest, Event, Guild,
    GuildWithChannels, Invite, Snowflake, UpdateChannelRequest, UpdateGuildRequest, perms,
};
use sqlx::PgPool;

use crate::Shared;
use crate::auth::AuthUser;
use crate::roles::{create_everyone, is_banned, member_perms};
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
        ChannelKind::Thread => "thread",
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
            "thread" => ChannelKind::Thread,
            _ => ChannelKind::Text,
        },
        name,
        parent_id: parent_id.map(|p| Snowflake(p as u64)),
        position,
        overwrites: Vec::new(),
    }
}

/// Канал сервера с его настройками прав
pub async fn load_channel(db: &PgPool, id: Snowflake) -> ApiResult<Channel> {
    let row: Option<ChannelRow> = sqlx::query_as("SELECT id, guild_id, kind, name, parent_id, position FROM channels WHERE id = $1 AND guild_id IS NOT NULL")
        .bind(id.0 as i64)
        .fetch_optional(db)
        .await?;
    let mut channel = channel_from_row(row.ok_or(AppError::NotFound("канал"))?);
    channel.overwrites = crate::roles::load_overwrites(db, &[id.0 as i64]).await?.remove(&(id.0 as i64)).unwrap_or_default();
    Ok(channel)
}

/// Все каналы сервера с правами (без фильтра видимости)
pub async fn guild_channels(db: &PgPool, guild: Snowflake) -> ApiResult<Vec<Channel>> {
    let rows: Vec<ChannelRow> = sqlx::query_as("SELECT id, guild_id, kind, name, parent_id, position FROM channels WHERE guild_id = $1 ORDER BY position, id")
        .bind(guild.0 as i64)
        .fetch_all(db)
        .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.0).collect();
    let mut ow = crate::roles::load_overwrites(db, &ids).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let mut c = channel_from_row(r);
            c.overwrites = ow.remove(&(c.id.0 as i64)).unwrap_or_default();
            c
        })
        .collect())
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
    let mut channel = channel_from_row((id, gid, kind, name, parent, pos));
    channel.overwrites = crate::roles::load_overwrites(db, &[id]).await?.remove(&id).unwrap_or_default();
    // Закрытый канал выглядит как несуществующий (у ветки — права родительского канала)
    if !crate::roles::channel_perms(db, &channel, user_id).await?.has(perms::VIEW_CHANNEL) {
        return Err(AppError::NotFound("канал"));
    }
    Ok((channel, Snowflake(owner as u64)))
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
    create_everyone(&mut tx, guild.id).await?;
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
        channels.push(Channel { id, guild_id: guild.id, kind, name: name.into(), parent_id: parent, position, overwrites: Vec::new() });
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

/// Каналы, которые пользователь видит (закрытые — только тем, кому можно)
pub async fn list_channels(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<Json<Vec<Channel>>> {
    require_member(&st.db, id, user.id()).await?;
    let channels = guild_channels(&st.db, id).await?;
    let visible = crate::roles::visible_channels(&st.db, id, user.id(), &channels).await?;
    // Категория видна, если в ней есть видимый канал или она сама не закрыта
    Ok(Json(channels.into_iter().filter(|c| visible.contains(&c.id)).collect()))
}

pub async fn create_channel(
    State(st): State<Shared>,
    user: AuthUser,
    Path(guild_id): Path<Snowflake>,
    Json(req): Json<CreateChannelRequest>,
) -> ApiResult<(StatusCode, Json<Channel>)> {
    member_perms(&st.db, guild_id, user.id()).await?.require(perms::MANAGE_CHANNELS)?;
    let name = validate_name(&req.name, "название канала")?;
    if req.kind == ChannelKind::Dm || req.kind == ChannelKind::Thread {
        return Err(AppError::BadRequest("bad_kind", "этот вид канала создаётся отдельно".into()));
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
    let channel = Channel { id: st.ids.next(), guild_id, kind: req.kind, name, parent_id: req.parent_id, position, overwrites: Vec::new() };
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
    crate::audit::log(&st, guild_id, user.id(), "channel_create", Some(channel.id), &channel.name).await;
    Ok((StatusCode::CREATED, Json(channel)))
}

/// Переименовать сообщество или передать его другому участнику
pub async fn update_guild(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>, Json(req): Json<UpdateGuildRequest>) -> ApiResult<Json<Guild>> {
    let me = member_perms(&st.db, id, user.id()).await?;
    if let Some(name) = &req.name {
        me.require(perms::MANAGE_GUILD)?;
        let name = validate_name(name, "название сообщества")?;
        sqlx::query("UPDATE guilds SET name = $1 WHERE id = $2").bind(&name).bind(id.0 as i64).execute(&st.db).await?;
        crate::audit::log(&st, id, user.id(), "guild_update", None, &name).await;
    }
    if let Some(new_owner) = req.owner_id {
        if !me.owner {
            return Err(AppError::Forbidden);
        }
        require_member(&st.db, id, new_owner).await?;
        sqlx::query("UPDATE guilds SET owner_id = $1 WHERE id = $2").bind(new_owner.0 as i64).bind(id.0 as i64).execute(&st.db).await?;
        let name = crate::auth::load_user(&st.db, new_owner).await.map(|u| u.username).unwrap_or_default();
        crate::audit::log(&st, id, user.id(), "owner_transfer", Some(new_owner), name).await;
        st.bus.publish(Target::Guild(id), Event::ChannelsInvalidated { guild_id: id });
    }
    let row: GuildRow = sqlx::query_as("SELECT id, name, owner_id FROM guilds WHERE id = $1").bind(id.0 as i64).fetch_one(&st.db).await?;
    let guild = guild_from_row(row);
    st.bus.publish(Target::Guild(id), Event::GuildUpdate(guild.clone()));
    Ok(Json(guild))
}

/// Удалить сообщество целиком (только владелец)
pub async fn delete_guild(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<StatusCode> {
    if !member_perms(&st.db, id, user.id()).await?.owner {
        return Err(AppError::Forbidden);
    }
    remove_guild(&st, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Удаление сообщества: участники получают GUILD_DELETE, файлы вложений стираются
pub async fn remove_guild(st: &Shared, id: Snowflake) -> ApiResult<()> {
    let members: Vec<(i64,)> = sqlx::query_as("SELECT user_id FROM guild_members WHERE guild_id = $1").bind(id.0 as i64).fetch_all(&st.db).await?;
    let files: Vec<(i64,)> = sqlx::query_as("SELECT a.id FROM attachments a JOIN channels c ON c.id = a.channel_id WHERE c.guild_id = $1")
        .bind(id.0 as i64)
        .fetch_all(&st.db)
        .await?;
    sqlx::query("DELETE FROM guilds WHERE id = $1").bind(id.0 as i64).execute(&st.db).await?;
    crate::attachments::remove_files(st, &files.into_iter().map(|(f,)| f).collect::<Vec<_>>()).await;
    for (m,) in members {
        let uid = Snowflake(m as u64);
        if let Some(left) = st.voice.remove_in_guild(uid, id) {
            st.bus.publish(Target::Guild(id), Event::VoiceStateUpdate(left));
        }
        st.bus.publish(Target::User(uid), Event::GuildDelete { guild_id: id });
    }
    Ok(())
}

pub async fn update_channel(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>, Json(req): Json<UpdateChannelRequest>) -> ApiResult<Json<Channel>> {
    let (channel, _) = channel_for_member(&st.db, id, user.id()).await?;
    // Ветку может переименовать её автор? Проще и безопаснее: как каналы — по праву MANAGE_CHANNELS
    member_perms(&st.db, channel.guild_id, user.id()).await?.require(perms::MANAGE_CHANNELS)?;
    if let Some(name) = &req.name {
        let name = validate_name(name, "название канала")?;
        sqlx::query("UPDATE channels SET name = $1 WHERE id = $2").bind(&name).bind(id.0 as i64).execute(&st.db).await?;
    }
    if let Some(pos) = req.position {
        sqlx::query("UPDATE channels SET position = $1 WHERE id = $2").bind(pos.clamp(0, 10_000)).bind(id.0 as i64).execute(&st.db).await?;
    }
    let updated = load_channel(&st.db, id).await?;
    crate::audit::log(&st, channel.guild_id, user.id(), "channel_update", Some(id), format!("{} → {}", channel.name, updated.name)).await;
    st.bus.publish(Target::Guild(channel.guild_id), Event::ChannelUpdate(updated.clone()));
    Ok(Json(updated))
}

pub async fn delete_channel(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<StatusCode> {
    let (channel, _) = channel_for_member(&st.db, id, user.id()).await?;
    member_perms(&st.db, channel.guild_id, user.id()).await?.require(perms::MANAGE_CHANNELS)?;
    // Вместе с каналом уходят его ветки; каналы категории остаются без категории (ON DELETE SET NULL)
    let gone: Vec<(i64,)> = sqlx::query_as("SELECT id FROM channels WHERE id = $1 OR (kind = 'thread' AND parent_id = $1)")
        .bind(id.0 as i64)
        .fetch_all(&st.db)
        .await?;
    let gone: Vec<i64> = gone.into_iter().map(|(c,)| c).collect();
    let files: Vec<(i64,)> = sqlx::query_as("SELECT id FROM attachments WHERE channel_id = ANY($1)").bind(&gone).fetch_all(&st.db).await?;
    sqlx::query("DELETE FROM channels WHERE id = ANY($1)").bind(&gone).execute(&st.db).await?;
    crate::attachments::remove_files(&st, &files.into_iter().map(|(f,)| f).collect::<Vec<_>>()).await;
    crate::audit::log(&st, channel.guild_id, user.id(), "channel_delete", Some(id), &channel.name).await;
    for c in gone {
        st.bus.publish(Target::Guild(channel.guild_id), Event::ChannelDelete { guild_id: channel.guild_id, channel_id: Snowflake(c as u64) });
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Ветка из сообщения: текстовый канал внутри канала сообщения
pub async fn create_thread(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>,
    Json(req): Json<CreateThreadRequest>,
) -> ApiResult<(StatusCode, Json<Channel>)> {
    let (parent, _) = channel_for_member(&st.db, channel_id, user.id()).await?;
    if parent.kind != ChannelKind::Text {
        return Err(AppError::BadRequest("bad_channel", "ветку можно начать только в текстовом канале".into()));
    }
    crate::roles::channel_perms(&st.db, &parent, user.id()).await?.require(perms::SEND_MESSAGES)?;
    let name = validate_name(&req.name, "название ветки")?;
    let id = st.ids.next();
    let mut tx = st.db.begin().await?;
    sqlx::query("INSERT INTO channels (id, guild_id, kind, name, parent_id, position) VALUES ($1, $2, 'thread', $3, $4, 0)")
        .bind(id.0 as i64)
        .bind(parent.guild_id.0 as i64)
        .bind(&name)
        .bind(channel_id.0 as i64)
        .execute(&mut *tx)
        .await?;
    // Без commit транзакция откатится вместе с веткой
    let taken = sqlx::query("UPDATE messages SET thread_id = $1 WHERE id = $2 AND channel_id = $3 AND thread_id IS NULL")
        .bind(id.0 as i64)
        .bind(message_id.0 as i64)
        .bind(channel_id.0 as i64)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if taken == 0 {
        return Err(AppError::BadRequest("bad_message", "сообщение не найдено или у него уже есть ветка".into()));
    }
    tx.commit().await?;
    let thread = Channel { id, guild_id: parent.guild_id, kind: ChannelKind::Thread, name, parent_id: Some(channel_id), position: 0, overwrites: Vec::new() };
    st.bus.publish(Target::Guild(parent.guild_id), Event::ChannelCreate(thread.clone()));
    if let Ok(m) = crate::messages::load_public(&st.db, channel_id, message_id).await {
        st.bus.publish(Target::Guild(parent.guild_id), Event::MessageUpdate(m));
    }
    Ok((StatusCode::CREATED, Json(thread)))
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
    member_perms(&st.db, guild_id, user.id()).await?.require(perms::CREATE_INVITE)?;
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
    crate::audit::log(&st, guild_id, user.id(), "invite_create", None, &code).await;
    Ok((StatusCode::CREATED, Json(Invite { code, guild_id, uses: 0, max_uses: req.max_uses })))
}

pub async fn accept_invite(State(st): State<Shared>, user: AuthUser, Path(code): Path<String>) -> ApiResult<Json<Guild>> {
    Ok(Json(join_by_invite(&st, &user.0, &code).await?))
}

/// Действующее ли приглашение (для регистрации «только по приглашению»)
pub async fn invite_is_valid(db: &PgPool, code: &str) -> ApiResult<bool> {
    let (ok,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM invites WHERE code = $1
                         AND (expires_at IS NULL OR expires_at > now()) AND (max_uses IS NULL OR uses < max_uses))",
    )
    .bind(code.trim())
    .fetch_one(db)
    .await?;
    Ok(ok)
}

/// Вступить в сообщество по коду (кнопка «Войти по приглашению» и регистрация с кодом)
pub async fn join_by_invite(st: &Shared, user: &feather_protocol::User, code: &str) -> ApiResult<Guild> {
    let code = code.trim();
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
    if is_banned(&st.db, guild_id, user.id).await? {
        return Err(AppError::Forbidden);
    }
    let inserted = sqlx::query("INSERT INTO guild_members (guild_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(guild_id)
        .bind(user.id.0 as i64)
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
        st.bus.publish(Target::Guild(guild.id), Event::GuildMemberAdd { guild_id: guild.id, user: user.clone() });
        st.bus.publish(Target::User(user.id), Event::GuildCreate(guild.clone()));
    }
    Ok(guild)
}

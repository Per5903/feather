//! Сообщения: история с пагинацией, отправка, редактирование, удаление.
//! Действия идут через REST, а события о них рассылаются через шлюз (docs/05-protocol.md).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use feather_protocol::{AckRequest, EditMessageRequest, Embed, Event, Message, Reaction, SendMessageRequest, Snowflake, UnreadChannel, User};
use serde::Deserialize;
use sqlx::PgPool;

use crate::Shared;
use crate::auth::AuthUser;
use crate::error::{ApiResult, AppError};
use crate::access::channel_access;
use crate::ratelimit;

const MAX_CONTENT: usize = 4000;
const MAX_NONCE: usize = 64;

type MessageRow = (i64, i64, String, Option<i64>, Option<i64>, i64, String, Option<String>, Option<i64>, bool, Option<i64>, Option<String>);

const SELECT_MESSAGE: &str = "SELECT m.id, m.channel_id, m.content, m.reply_to_id,
       (extract(epoch FROM m.edited_at) * 1000)::bigint, u.id, u.username, u.display_name, u.avatar_id,
       m.pinned_at IS NOT NULL, m.thread_id, m.embeds
  FROM messages m JOIN users u ON u.id = m.author_id";

const MAX_EMOJI: usize = 32;

fn message_from_row((id, channel_id, content, reply_to, edited_at, uid, username, display_name, avatar, pinned, thread, embeds): MessageRow) -> Message {
    Message {
        id: Snowflake(id as u64),
        channel_id: Snowflake(channel_id as u64),
        author: User { id: Snowflake(uid as u64), username, display_name, avatar: avatar.map(|a| Snowflake(a as u64)) },
        content,
        reply_to_id: reply_to.map(|r| Snowflake(r as u64)),
        edited_at,
        nonce: None,
        attachments: Vec::new(),
        reactions: Vec::new(),
        pinned,
        thread_id: thread.map(|t| Snowflake(t as u64)),
        embeds: embeds.and_then(|e| serde_json::from_str::<Vec<Embed>>(&e).ok()).unwrap_or_default(),
    }
}

/// Подставить вложения и реакции одной пачкой (по запросу на всю страницу истории).
/// `viewer` — для отметки «моя реакция»; None — для рассылки событий всем
async fn enrich(db: &PgPool, mut msgs: Vec<Message>, viewer: Option<Snowflake>) -> ApiResult<Vec<Message>> {
    let ids: Vec<i64> = msgs.iter().map(|m| m.id.0 as i64).collect();
    let mut map = crate::attachments::for_messages(db, &ids).await?;
    let rows: Vec<(i64, String, i64, bool)> = sqlx::query_as(
        "SELECT message_id, emoji, count(*), bool_or(user_id = $2) FROM reactions
          WHERE message_id = ANY($1) GROUP BY message_id, emoji ORDER BY message_id, min(created_at)",
    )
    .bind(&ids)
    .bind(viewer.map(|v| v.0 as i64).unwrap_or(0))
    .fetch_all(db)
    .await?;
    let mut reactions: std::collections::HashMap<i64, Vec<Reaction>> = Default::default();
    for (mid, emoji, count, me) in rows {
        reactions.entry(mid).or_default().push(Reaction { emoji, count: count as u32, me });
    }
    for m in &mut msgs {
        if let Some(a) = map.remove(&(m.id.0 as i64)) {
            m.attachments = a;
        }
        if let Some(r) = reactions.remove(&(m.id.0 as i64)) {
            m.reactions = r;
        }
    }
    Ok(msgs)
}

fn validate_content(content: &str) -> ApiResult<String> {
    let content = content.trim();
    if content.is_empty() || content.chars().count() > MAX_CONTENT {
        return Err(AppError::BadRequest("bad_content", format!("сообщение: от 1 до {MAX_CONTENT} символов")));
    }
    Ok(content.to_owned())
}

async fn load(db: &PgPool, channel_id: Snowflake, id: Snowflake) -> ApiResult<Message> {
    // SELECT_MESSAGE — константа без пользовательских данных, поэтому AssertSqlSafe безопасен
    let sql = format!("{SELECT_MESSAGE} WHERE m.id = $1 AND m.channel_id = $2");
    let row: Option<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(id.0 as i64)
        .bind(channel_id.0 as i64)
        .fetch_optional(db)
        .await?;
    let m = row.map(message_from_row).ok_or(AppError::NotFound("сообщение"))?;
    Ok(enrich(db, vec![m], None).await?.remove(0))
}

/// Сообщение для рассылки событием (без «моих» отметок)
pub async fn load_public(db: &PgPool, channel_id: Snowflake, id: Snowflake) -> ApiResult<Message> {
    load(db, channel_id, id).await
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    before: Option<Snowflake>,
    limit: Option<i64>,
}

/// История канала: новые сверху, `before` — для подгрузки старых при прокрутке
pub async fn history(
    State(st): State<Shared>,
    user: AuthUser,
    Path(channel_id): Path<Snowflake>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<Vec<Message>>> {
    channel_access(&st.db, channel_id, user.id()).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let sql = format!("{SELECT_MESSAGE} WHERE m.channel_id = $1 AND ($2::bigint IS NULL OR m.id < $2) ORDER BY m.id DESC LIMIT $3");
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(channel_id.0 as i64)
        .bind(q.before.map(|b| b.0 as i64))
        .bind(limit)
        .fetch_all(&st.db)
        .await?;
    Ok(Json(enrich(&st.db, rows.into_iter().map(message_from_row).collect(), Some(user.id())).await?))
}

pub async fn send(
    State(st): State<Shared>,
    user: AuthUser,
    Path(channel_id): Path<Snowflake>,
    Json(req): Json<SendMessageRequest>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    st.limiter.check(&format!("msg:{}", user.id()), ratelimit::MESSAGES).map_err(AppError::rate_limited)?;
    let access = channel_access(&st.db, channel_id, user.id()).await?;
    if access.is_textual() && !access.can_write() {
        return Err(AppError::Forbidden);
    }
    if !access.can_write() {
        return Err(AppError::BadRequest("not_text_channel", "писать можно только в текстовый канал".into()));
    }
    // Подпись к картинке необязательна: можно отправить только вложения
    let content = if req.attachment_ids.is_empty() || !req.content.trim().is_empty() {
        validate_content(&req.content)?
    } else {
        String::new()
    };
    if req.nonce.as_ref().is_some_and(|n| n.len() > MAX_NONCE) {
        return Err(AppError::BadRequest("bad_nonce", format!("nonce: до {MAX_NONCE} байт")));
    }
    let id = st.ids.next();
    let mut tx = st.db.begin().await?;
    sqlx::query("INSERT INTO messages (id, channel_id, author_id, content, reply_to_id) VALUES ($1, $2, $3, $4, $5)")
        .bind(id.0 as i64)
        .bind(channel_id.0 as i64)
        .bind(user.id().0 as i64)
        .bind(&content)
        .bind(req.reply_to_id.map(|r| r.0 as i64))
        .execute(&mut *tx)
        .await?;
    // В той же транзакции: если вложение чужое или уже отправлено — сообщение не создаётся
    let attachments = crate::attachments::attach(&mut *tx, &req.attachment_ids, id, channel_id, user.id()).await?;
    tx.commit().await?;
    // Своё сообщение считается прочитанным: иначе канал «загорится» на других устройствах автора
    mark_read(&st.db, user.id(), channel_id, id).await?;
    let message = Message {
        id,
        channel_id,
        author: user.0,
        content,
        reply_to_id: req.reply_to_id,
        edited_at: None,
        nonce: req.nonce,
        attachments,
        reactions: Vec::new(),
        pinned: false,
        thread_id: None,
        embeds: Vec::new(),
    };
    access.publish(&st.bus, Event::MessageCreate(message.clone()));
    crate::embeds::spawn(st.clone(), access, message.clone());
    Ok((StatusCode::CREATED, Json(message)))
}

pub async fn edit(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>,
    Json(req): Json<EditMessageRequest>,
) -> ApiResult<Json<Message>> {
    let access = channel_access(&st.db, channel_id, user.id()).await?;
    let content = validate_content(&req.content)?;
    let updated = sqlx::query("UPDATE messages SET content = $1, edited_at = now() WHERE id = $2 AND channel_id = $3 AND author_id = $4")
        .bind(&content)
        .bind(message_id.0 as i64)
        .bind(channel_id.0 as i64)
        .bind(user.id().0 as i64)
        .execute(&st.db)
        .await?
        .rows_affected();
    if updated == 0 {
        // Либо нет такого сообщения, либо оно чужое: редактировать можно только своё
        load(&st.db, channel_id, message_id).await?;
        return Err(AppError::Forbidden);
    }
    // Текст сменился — превью ссылок пересчитываем
    sqlx::query("UPDATE messages SET embeds = NULL WHERE id = $1").bind(message_id.0 as i64).execute(&st.db).await?;
    let message = load(&st.db, channel_id, message_id).await?;
    access.publish(&st.bus, Event::MessageUpdate(message.clone()));
    crate::embeds::spawn(st.clone(), access, message.clone());
    Ok(Json(message))
}

pub async fn delete(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>,
) -> ApiResult<StatusCode> {
    let access = channel_access(&st.db, channel_id, user.id()).await?;
    let message = load(&st.db, channel_id, message_id).await?;
    // Удалить может автор или владелец сервера (модерация)
    if message.author.id != user.id() && !access.is_moderator() {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM messages WHERE id = $1").bind(message_id.0 as i64).execute(&st.db).await?;
    let files: Vec<i64> = message.attachments.iter().map(|a| a.id.0 as i64).collect();
    crate::attachments::remove_files(&st, &files).await;
    if message.author.id != user.id()
        && let Some(guild) = access.guild_id()
    {
        let preview: String = message.content.chars().take(80).collect();
        crate::audit::log(&st, guild, user.id(), "message_delete", Some(message.author.id), format!("{}: {preview}", message.author.username)).await;
    }
    access.publish(&st.bus, Event::MessageDelete { id: message_id, channel_id });
    Ok(StatusCode::NO_CONTENT)
}

/// Реакция: поставить (`add`) или убрать свою
async fn react(st: &Shared, user: Snowflake, channel_id: Snowflake, message_id: Snowflake, emoji: String, add: bool) -> ApiResult<StatusCode> {
    let emoji = emoji.trim().to_owned();
    if emoji.is_empty() || emoji.len() > MAX_EMOJI || emoji.chars().any(|c| c.is_ascii_alphanumeric() || c.is_whitespace() || c.is_control()) {
        return Err(AppError::BadRequest("bad_emoji", "реакция — это эмодзи".into()));
    }
    let access = channel_access(&st.db, channel_id, user).await?;
    if !access.is_textual() {
        return Err(AppError::BadRequest("not_text_channel", "реакции — только в текстовых каналах".into()));
    }
    load(&st.db, channel_id, message_id).await?;
    let changed = if add {
        st.limiter.check(&format!("react:{user}"), ratelimit::MESSAGES).map_err(AppError::rate_limited)?;
        let (kinds,): (i64,) = sqlx::query_as("SELECT count(DISTINCT emoji) FROM reactions WHERE message_id = $1").bind(message_id.0 as i64).fetch_one(&st.db).await?;
        if kinds >= 20 {
            return Err(AppError::BadRequest("too_many_reactions", "не больше 20 разных реакций".into()));
        }
        sqlx::query("INSERT INTO reactions (message_id, user_id, emoji) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
    } else {
        sqlx::query("DELETE FROM reactions WHERE message_id = $1 AND user_id = $2 AND emoji = $3")
    }
    .bind(message_id.0 as i64)
    .bind(user.0 as i64)
    .bind(&emoji)
    .execute(&st.db)
    .await?
    .rows_affected()
        > 0;
    if changed {
        let event = if add {
            Event::ReactionAdd { channel_id, message_id, user_id: user, emoji }
        } else {
            Event::ReactionRemove { channel_id, message_id, user_id: user, emoji }
        };
        access.publish(&st.bus, event);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_reaction(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id, emoji)): Path<(Snowflake, Snowflake, String)>,
) -> ApiResult<StatusCode> {
    react(&st, user.id(), channel_id, message_id, emoji, true).await
}

pub async fn remove_reaction(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id, emoji)): Path<(Snowflake, Snowflake, String)>,
) -> ApiResult<StatusCode> {
    react(&st, user.id(), channel_id, message_id, emoji, false).await
}

/// Закрепить / открепить: в сообществе — модератор, в личных — любой из собеседников
async fn pin(st: &Shared, user: Snowflake, channel_id: Snowflake, message_id: Snowflake, on: bool) -> ApiResult<StatusCode> {
    let access = channel_access(&st.db, channel_id, user).await?;
    if access.guild_id().is_some() && !access.is_moderator() {
        return Err(AppError::Forbidden);
    }
    if on {
        let (count,): (i64,) = sqlx::query_as("SELECT count(*) FROM messages WHERE channel_id = $1 AND pinned_at IS NOT NULL")
            .bind(channel_id.0 as i64)
            .fetch_one(&st.db)
            .await?;
        if count >= 50 {
            return Err(AppError::BadRequest("too_many_pins", "в канале не больше 50 закреплённых".into()));
        }
    }
    let q = if on {
        "UPDATE messages SET pinned_at = now() WHERE id = $1 AND channel_id = $2 AND pinned_at IS NULL"
    } else {
        "UPDATE messages SET pinned_at = NULL WHERE id = $1 AND channel_id = $2 AND pinned_at IS NOT NULL"
    };
    let changed = sqlx::query(q).bind(message_id.0 as i64).bind(channel_id.0 as i64).execute(&st.db).await?.rows_affected() > 0;
    let message = load(&st.db, channel_id, message_id).await?;
    if changed {
        if let Some(guild) = access.guild_id() {
            let preview: String = message.content.chars().take(80).collect();
            crate::audit::log(st, guild, user, if on { "message_pin" } else { "message_unpin" }, Some(message_id), preview).await;
        }
        access.publish(&st.bus, Event::MessageUpdate(message));
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn pin_message(State(st): State<Shared>, user: AuthUser, Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    pin(&st, user.id(), channel_id, message_id, true).await
}

pub async fn unpin_message(State(st): State<Shared>, user: AuthUser, Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>) -> ApiResult<StatusCode> {
    pin(&st, user.id(), channel_id, message_id, false).await
}

pub async fn list_pins(State(st): State<Shared>, user: AuthUser, Path(channel_id): Path<Snowflake>) -> ApiResult<Json<Vec<Message>>> {
    channel_access(&st.db, channel_id, user.id()).await?;
    let sql = format!("{SELECT_MESSAGE} WHERE m.channel_id = $1 AND m.pinned_at IS NOT NULL ORDER BY m.pinned_at DESC");
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(channel_id.0 as i64).fetch_all(&st.db).await?;
    Ok(Json(enrich(&st.db, rows.into_iter().map(message_from_row).collect(), Some(user.id())).await?))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: String,
    before: Option<Snowflake>,
}

/// Экранировать % и _ для ILIKE
fn like_pattern(q: &str) -> String {
    let escaped: String = q.chars().flat_map(|c| if matches!(c, '%' | '_' | '\\') { vec!['\\', c] } else { vec![c] }).collect();
    format!("%{escaped}%")
}

fn validate_query(q: &str) -> ApiResult<String> {
    let q = q.trim();
    if q.chars().count() < 2 || q.chars().count() > 100 {
        return Err(AppError::BadRequest("bad_query", "поиск: от 2 до 100 символов".into()));
    }
    Ok(q.to_owned())
}

/// Поиск по тексту в одном канале (личном или канале сообщества)
pub async fn search_channel(State(st): State<Shared>, user: AuthUser, Path(channel_id): Path<Snowflake>, Query(q): Query<SearchQuery>) -> ApiResult<Json<Vec<Message>>> {
    channel_access(&st.db, channel_id, user.id()).await?;
    let text = validate_query(&q.q)?;
    let sql = format!("{SELECT_MESSAGE} WHERE m.channel_id = $1 AND m.content ILIKE $2 AND ($3::bigint IS NULL OR m.id < $3) ORDER BY m.id DESC LIMIT 25");
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(channel_id.0 as i64)
        .bind(like_pattern(&text))
        .bind(q.before.map(|b| b.0 as i64))
        .fetch_all(&st.db)
        .await?;
    Ok(Json(enrich(&st.db, rows.into_iter().map(message_from_row).collect(), Some(user.id())).await?))
}

/// Поиск по всем видимым текстовым каналам сообщества
pub async fn search_guild(State(st): State<Shared>, user: AuthUser, Path(guild): Path<Snowflake>, Query(q): Query<SearchQuery>) -> ApiResult<Json<Vec<Message>>> {
    crate::guilds::require_member(&st.db, guild, user.id()).await?;
    let text = validate_query(&q.q)?;
    let channels = crate::guilds::guild_channels(&st.db, guild).await?;
    let visible: Vec<i64> = crate::roles::visible_channels(&st.db, guild, user.id(), &channels).await?.into_iter().map(|c| c.0 as i64).collect();
    let sql = format!("{SELECT_MESSAGE} WHERE m.channel_id = ANY($1) AND m.content ILIKE $2 AND ($3::bigint IS NULL OR m.id < $3) ORDER BY m.id DESC LIMIT 25");
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(&visible)
        .bind(like_pattern(&text))
        .bind(q.before.map(|b| b.0 as i64))
        .fetch_all(&st.db)
        .await?;
    Ok(Json(enrich(&st.db, rows.into_iter().map(message_from_row).collect(), Some(user.id())).await?))
}

/// Отметка «дочитал до message_id»; назад не откатывается (GREATEST)
async fn mark_read(db: &PgPool, user: Snowflake, channel: Snowflake, message: Snowflake) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO read_states (user_id, channel_id, last_read_message_id) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, channel_id)
         DO UPDATE SET last_read_message_id = GREATEST(read_states.last_read_message_id, EXCLUDED.last_read_message_id)",
    )
    .bind(user.0 as i64)
    .bind(channel.0 as i64)
    .bind(message.0 as i64)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn ack(
    State(st): State<Shared>,
    user: AuthUser,
    Path(channel_id): Path<Snowflake>,
    Json(req): Json<AckRequest>,
) -> ApiResult<StatusCode> {
    channel_access(&st.db, channel_id, user.id()).await?;
    mark_read(&st.db, user.id(), channel_id, req.message_id).await?;
    st.bus.publish(crate::bus::Target::User(user.id()), Event::ChannelAck { channel_id, message_id: req.message_id });
    Ok(StatusCode::NO_CONTENT)
}

/// Текстовые каналы пользователя, где есть сообщения новее его отметки о прочтении
pub async fn unread_channels(db: &PgPool, user: Snowflake) -> ApiResult<Vec<UnreadChannel>> {
    let rows: Vec<(Option<i64>, i64, i64)> = sqlx::query_as(
        "SELECT c.guild_id, c.id, lm.last_id
           FROM channels c
           LEFT JOIN guild_members gm ON gm.guild_id = c.guild_id AND gm.user_id = $1
           LEFT JOIN channel_recipients cr ON cr.channel_id = c.id AND cr.user_id = $1
           CROSS JOIN LATERAL (SELECT max(id) AS last_id FROM messages WHERE channel_id = c.id) lm
           LEFT JOIN read_states rs ON rs.channel_id = c.id AND rs.user_id = $1
          WHERE c.kind IN ('text', 'dm', 'thread') AND (gm.user_id IS NOT NULL OR cr.user_id IS NOT NULL)
            AND lm.last_id IS NOT NULL
            AND (rs.last_read_message_id IS NULL OR lm.last_id > rs.last_read_message_id)",
    )
    .bind(user.0 as i64)
    .fetch_all(db)
    .await?;
    // Закрытые каналы не показываем непрочитанными: проверяем видимость по серверам разом
    let mut visible: std::collections::HashMap<i64, Vec<Snowflake>> = Default::default();
    for g in rows.iter().filter_map(|r| r.0) {
        if !visible.contains_key(&g) {
            let gid = Snowflake(g as u64);
            let channels = crate::guilds::guild_channels(db, gid).await?;
            visible.insert(g, crate::roles::visible_channels(db, gid, user, &channels).await.unwrap_or_default());
        }
    }
    Ok(rows
        .into_iter()
        .filter(|(g, c, _)| g.is_none_or(|g| visible.get(&g).is_some_and(|v| v.contains(&Snowflake(*c as u64)))))
        .map(|(g, c, m)| UnreadChannel {
            guild_id: g.map(|g| Snowflake(g as u64)),
            channel_id: Snowflake(c as u64),
            last_message_id: Snowflake(m as u64),
        })
        .collect())
}

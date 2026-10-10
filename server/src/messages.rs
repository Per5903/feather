//! Сообщения: история с пагинацией, отправка, редактирование, удаление.
//! Действия идут через REST, а события о них рассылаются через шлюз (docs/05-protocol.md).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use feather_protocol::{AckRequest, ChannelKind, EditMessageRequest, Event, Message, SendMessageRequest, Snowflake, UnreadChannel, User};
use serde::Deserialize;
use sqlx::PgPool;

use crate::Shared;
use crate::auth::AuthUser;
use crate::bus::Target;
use crate::error::{ApiResult, AppError};
use crate::guilds::channel_for_member;
use crate::ratelimit;

const MAX_CONTENT: usize = 4000;
const MAX_NONCE: usize = 64;

type MessageRow = (i64, i64, String, Option<i64>, Option<i64>, i64, String, Option<String>);

const SELECT_MESSAGE: &str = "SELECT m.id, m.channel_id, m.content, m.reply_to_id,
       (extract(epoch FROM m.edited_at) * 1000)::bigint, u.id, u.username, u.display_name
  FROM messages m JOIN users u ON u.id = m.author_id";

fn message_from_row((id, channel_id, content, reply_to, edited_at, uid, username, display_name): MessageRow) -> Message {
    Message {
        id: Snowflake(id as u64),
        channel_id: Snowflake(channel_id as u64),
        author: User { id: Snowflake(uid as u64), username, display_name },
        content,
        reply_to_id: reply_to.map(|r| Snowflake(r as u64)),
        edited_at,
        nonce: None,
    }
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
    row.map(message_from_row).ok_or(AppError::NotFound("сообщение"))
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
    channel_for_member(&st.db, channel_id, user.id()).await?;
    let limit = q.limit.unwrap_or(50).clamp(1, 100);
    let sql = format!("{SELECT_MESSAGE} WHERE m.channel_id = $1 AND ($2::bigint IS NULL OR m.id < $2) ORDER BY m.id DESC LIMIT $3");
    let rows: Vec<MessageRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(channel_id.0 as i64)
        .bind(q.before.map(|b| b.0 as i64))
        .bind(limit)
        .fetch_all(&st.db)
        .await?;
    Ok(Json(rows.into_iter().map(message_from_row).collect()))
}

pub async fn send(
    State(st): State<Shared>,
    user: AuthUser,
    Path(channel_id): Path<Snowflake>,
    Json(req): Json<SendMessageRequest>,
) -> ApiResult<(StatusCode, Json<Message>)> {
    st.limiter.check(&format!("msg:{}", user.id()), ratelimit::MESSAGES).map_err(AppError::rate_limited)?;
    let (channel, _) = channel_for_member(&st.db, channel_id, user.id()).await?;
    if channel.kind != ChannelKind::Text {
        return Err(AppError::BadRequest("not_text_channel", "писать можно только в текстовый канал".into()));
    }
    let content = validate_content(&req.content)?;
    if req.nonce.as_ref().is_some_and(|n| n.len() > MAX_NONCE) {
        return Err(AppError::BadRequest("bad_nonce", format!("nonce: до {MAX_NONCE} байт")));
    }
    let id = st.ids.next();
    sqlx::query("INSERT INTO messages (id, channel_id, author_id, content, reply_to_id) VALUES ($1, $2, $3, $4, $5)")
        .bind(id.0 as i64)
        .bind(channel_id.0 as i64)
        .bind(user.id().0 as i64)
        .bind(&content)
        .bind(req.reply_to_id.map(|r| r.0 as i64))
        .execute(&st.db)
        .await?;
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
    };
    st.bus.publish(Target::Guild(channel.guild_id), Event::MessageCreate(message.clone()));
    Ok((StatusCode::CREATED, Json(message)))
}

pub async fn edit(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>,
    Json(req): Json<EditMessageRequest>,
) -> ApiResult<Json<Message>> {
    let (channel, _) = channel_for_member(&st.db, channel_id, user.id()).await?;
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
    let message = load(&st.db, channel_id, message_id).await?;
    st.bus.publish(Target::Guild(channel.guild_id), Event::MessageUpdate(message.clone()));
    Ok(Json(message))
}

pub async fn delete(
    State(st): State<Shared>,
    user: AuthUser,
    Path((channel_id, message_id)): Path<(Snowflake, Snowflake)>,
) -> ApiResult<StatusCode> {
    let (channel, owner) = channel_for_member(&st.db, channel_id, user.id()).await?;
    let message = load(&st.db, channel_id, message_id).await?;
    // Удалить может автор или владелец сервера (модерация)
    if message.author.id != user.id() && owner != user.id() {
        return Err(AppError::Forbidden);
    }
    sqlx::query("DELETE FROM messages WHERE id = $1").bind(message_id.0 as i64).execute(&st.db).await?;
    st.bus.publish(Target::Guild(channel.guild_id), Event::MessageDelete { id: message_id, channel_id });
    Ok(StatusCode::NO_CONTENT)
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
    channel_for_member(&st.db, channel_id, user.id()).await?;
    mark_read(&st.db, user.id(), channel_id, req.message_id).await?;
    st.bus.publish(Target::User(user.id()), Event::ChannelAck { channel_id, message_id: req.message_id });
    Ok(StatusCode::NO_CONTENT)
}

/// Текстовые каналы пользователя, где есть сообщения новее его отметки о прочтении
pub async fn unread_channels(db: &PgPool, user: Snowflake) -> ApiResult<Vec<UnreadChannel>> {
    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        "SELECT c.guild_id, c.id, lm.last_id
           FROM channels c
           JOIN guild_members gm ON gm.guild_id = c.guild_id AND gm.user_id = $1
           CROSS JOIN LATERAL (SELECT max(id) AS last_id FROM messages WHERE channel_id = c.id) lm
           LEFT JOIN read_states rs ON rs.channel_id = c.id AND rs.user_id = $1
          WHERE c.kind = 'text' AND lm.last_id IS NOT NULL
            AND (rs.last_read_message_id IS NULL OR lm.last_id > rs.last_read_message_id)",
    )
    .bind(user.0 as i64)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(g, c, m)| UnreadChannel {
            guild_id: Snowflake(g as u64),
            channel_id: Snowflake(c as u64),
            last_message_id: Snowflake(m as u64),
        })
        .collect())
}

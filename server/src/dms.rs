//! Личные сообщения: создание личного канала и список диалогов.
//! Писать можно только тем, с кем есть общий сервер — защита от спама незнакомцам.

use axum::extract::State;
use axum::Json;
use feather_protocol::{CreateDmRequest, DmChannel, Event, Snowflake, User};
use sqlx::PgPool;

use crate::Shared;
use crate::auth::{AuthUser, UserRow, user_from_row};
use crate::bus::Target;
use crate::error::{ApiResult, AppError};

/// Личный канал глазами пользователя `viewer`: собеседники без него самого
async fn dm_for(db: &PgPool, channel: i64, viewer: Snowflake) -> ApiResult<DmChannel> {
    let users: Vec<UserRow> = sqlx::query_as(
        "SELECT u.id, u.username, u.display_name FROM channel_recipients r JOIN users u ON u.id = r.user_id
          WHERE r.channel_id = $1 AND r.user_id <> $2",
    )
    .bind(channel)
    .bind(viewer.0 as i64)
    .fetch_all(db)
    .await?;
    let (last,): (Option<i64>,) = sqlx::query_as("SELECT max(id) FROM messages WHERE channel_id = $1").bind(channel).fetch_one(db).await?;
    Ok(DmChannel {
        id: Snowflake(channel as u64),
        recipients: users.into_iter().map(user_from_row).collect(),
        last_message_id: last.map(|l| Snowflake(l as u64)),
    })
}

/// Открыть (или создать) личный канал с пользователем
pub async fn open_dm(State(st): State<Shared>, user: AuthUser, Json(req): Json<CreateDmRequest>) -> ApiResult<Json<DmChannel>> {
    let (me, other) = (user.id(), req.recipient_id);
    if me == other {
        return Err(AppError::BadRequest("self_dm", "нельзя написать самому себе".into()));
    }
    let (shared,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM guild_members a JOIN guild_members b ON a.guild_id = b.guild_id
                         WHERE a.user_id = $1 AND b.user_id = $2)",
    )
    .bind(me.0 as i64)
    .bind(other.0 as i64)
    .fetch_one(&st.db)
    .await?;
    if !shared {
        // Не раскрываем, существует ли пользователь
        return Err(AppError::NotFound("пользователь"));
    }

    let key = format!("{}:{}", me.0.min(other.0), me.0.max(other.0));
    let mut tx = st.db.begin().await?;
    let created: Option<(i64,)> = sqlx::query_as(
        "INSERT INTO channels (id, guild_id, kind, name, dm_key) VALUES ($1, NULL, 'dm', '', $2)
         ON CONFLICT (dm_key) DO NOTHING RETURNING id",
    )
    .bind(st.ids.next().0 as i64)
    .bind(&key)
    .fetch_optional(&mut *tx)
    .await?;
    let channel = match created {
        Some((id,)) => {
            for u in [me, other] {
                sqlx::query("INSERT INTO channel_recipients (channel_id, user_id) VALUES ($1, $2)")
                    .bind(id)
                    .bind(u.0 as i64)
                    .execute(&mut *tx)
                    .await?;
            }
            id
        }
        None => sqlx::query_as::<_, (i64,)>("SELECT id FROM channels WHERE dm_key = $1").bind(&key).fetch_one(&mut *tx).await?.0,
    };
    tx.commit().await?;

    if created.is_some() {
        // Оба увидят новый диалог в списке (каждый — со своей стороны)
        for u in [me, other] {
            st.bus.publish(Target::User(u), Event::DmCreate(dm_for(&st.db, channel, u).await?));
        }
    }
    Ok(Json(dm_for(&st.db, channel, me).await?))
}

pub async fn list_dms(State(st): State<Shared>, user: AuthUser) -> ApiResult<Json<Vec<DmChannel>>> {
    let ids: Vec<(i64,)> = sqlx::query_as(
        "SELECT c.id FROM channels c JOIN channel_recipients r ON r.channel_id = c.id AND r.user_id = $1
          WHERE c.kind = 'dm'
          ORDER BY (SELECT max(id) FROM messages WHERE channel_id = c.id) DESC NULLS LAST",
    )
    .bind(user.id().0 as i64)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(ids.len());
    for (id,) in ids {
        out.push(dm_for(&st.db, id, user.id()).await?);
    }
    Ok(Json(out))
}

/// Для подписи и проверок: собеседник в личном канале
#[allow(dead_code)]
pub fn other(dm: &DmChannel) -> Option<&User> {
    dm.recipients.first()
}

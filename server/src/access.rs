//! Доступ к каналу: гильдейскому (участник сервера) или личному (собеседник).
//! Сообщения, отметки о прочтении и «печатает» работают одинаково для обоих видов.

use feather_protocol::{Channel, ChannelKind, Event, Snowflake, perms};
use sqlx::PgPool;

use crate::bus::{Bus, Target};
use crate::error::{ApiResult, AppError};
use crate::guilds::channel_from_row;

pub enum Access {
    Guild { channel: Channel, perms: crate::roles::Perms },
    Dm { recipients: Vec<Snowflake> },
}

impl Access {
    /// В канал вообще пишут сообщения: текстовый канал сервера или личный
    pub fn is_textual(&self) -> bool {
        match self {
            Access::Guild { channel, .. } => matches!(channel.kind, ChannelKind::Text | ChannelKind::Thread),
            Access::Dm { .. } => true,
        }
    }

    /// Мне можно писать: канал текстовый и есть право SEND_MESSAGES
    pub fn can_write(&self) -> bool {
        self.is_textual() && !matches!(self, Access::Guild { perms, .. } if !perms.has(perms::SEND_MESSAGES))
    }

    /// Может удалять чужие сообщения: право MANAGE_MESSAGES. В личных — никто
    pub fn is_moderator(&self) -> bool {
        matches!(self, Access::Guild { perms, .. } if perms.has(perms::MANAGE_MESSAGES))
    }

    pub fn guild_id(&self) -> Option<Snowflake> {
        match self {
            Access::Guild { channel, .. } => Some(channel.guild_id),
            Access::Dm { .. } => None,
        }
    }

    /// Разослать событие тем, кто видит канал
    pub fn publish(&self, bus: &Bus, event: Event) {
        match self {
            Access::Guild { channel, .. } => bus.publish(Target::Guild(channel.guild_id), event),
            Access::Dm { recipients } => {
                for r in recipients {
                    bus.publish(Target::User(*r), event.clone());
                }
            }
        }
    }
}

type Row = (i64, Option<i64>, String, String, Option<i64>, i32, Option<i64>);

/// Канал, если пользователь имеет к нему доступ; чужой канал выглядит как несуществующий
pub async fn channel_access(db: &PgPool, channel_id: Snowflake, user: Snowflake) -> ApiResult<Access> {
    let row: Option<Row> = sqlx::query_as(
        "SELECT c.id, c.guild_id, c.kind, c.name, c.parent_id, c.position, g.owner_id
           FROM channels c LEFT JOIN guilds g ON g.id = c.guild_id
          WHERE c.id = $1 AND (
                (c.guild_id IS NOT NULL AND EXISTS
                    (SELECT 1 FROM guild_members m WHERE m.guild_id = c.guild_id AND m.user_id = $2))
             OR (c.guild_id IS NULL AND EXISTS
                    (SELECT 1 FROM channel_recipients r WHERE r.channel_id = c.id AND r.user_id = $2)))",
    )
    .bind(channel_id.0 as i64)
    .bind(user.0 as i64)
    .fetch_optional(db)
    .await?;
    let (id, guild_id, kind, name, parent, position, owner) = row.ok_or(AppError::NotFound("канал"))?;
    match (guild_id, owner) {
        (Some(g), Some(_)) => {
            let mut channel = channel_from_row((id, g, kind, name, parent, position));
            channel.overwrites = crate::roles::load_overwrites(db, &[id]).await?.remove(&id).unwrap_or_default();
            let perms = crate::roles::channel_perms(db, &channel, user).await?;
            // Закрытый канал выглядит как несуществующий
            if !perms.has(perms::VIEW_CHANNEL) {
                return Err(AppError::NotFound("канал"));
            }
            Ok(Access::Guild { perms, channel })
        }
        _ => {
            let recipients: Vec<(i64,)> = sqlx::query_as("SELECT user_id FROM channel_recipients WHERE channel_id = $1")
                .bind(id)
                .fetch_all(db)
                .await?;
            Ok(Access::Dm { recipients: recipients.into_iter().map(|(u,)| Snowflake(u as u64)).collect() })
        }
    }
}

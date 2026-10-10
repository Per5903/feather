//! Протокол Feather: модели, тела REST-запросов и сообщения шлюза (docs/05-protocol.md).
//!
//! REST ходит в JSON, шлюз (WebSocket) — в MessagePack. ID в JSON передаются строкой
//! (JavaScript теряет точность на u64), в MessagePack — числом.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 64-битный ID: время (мс от эпохи Feather) | воркер | счётчик. Сортируется по времени.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Snowflake(pub u64);

/// Эпоха Feather: 2026-01-01T00:00:00Z, мс Unix
pub const FEATHER_EPOCH_MS: u64 = 1_767_225_600_000;
/// Биты узла и счётчика под временем (10 + 12)
pub const SNOWFLAKE_TIME_SHIFT: u32 = 22;

impl Snowflake {
    /// Время создания объекта, мс Unix: отдельное поле created_at не нужно
    pub fn timestamp_ms(self) -> u64 {
        (self.0 >> SNOWFLAKE_TIME_SHIFT) + FEATHER_EPOCH_MS
    }
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for Snowflake {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Snowflake)
    }
}

impl Serialize for Snowflake {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() { s.collect_str(&self.0) } else { s.serialize_u64(self.0) }
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Num(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Num(n) => Ok(Snowflake(n)),
            Raw::Str(s) => s.parse().map(Snowflake).map_err(serde::de::Error::custom),
        }
    }
}

// ---------------------------------------------------------------- модели

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct User {
    pub id: Snowflake,
    pub username: String,
    pub display_name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Guild {
    pub id: Snowflake,
    pub name: String,
    pub owner_id: Snowflake,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
    Category,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub id: Snowflake,
    pub guild_id: Snowflake,
    pub kind: ChannelKind,
    pub name: String,
    pub parent_id: Option<Snowflake>,
    pub position: i32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub id: Snowflake,
    pub channel_id: Snowflake,
    pub author: User,
    pub content: String,
    pub reply_to_id: Option<Snowflake>,
    /// Время последнего редактирования, мс Unix
    pub edited_at: Option<i64>,
    /// Эхо `nonce` из запроса отправки: клиент сопоставляет своё «отправляется…» с ответом
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Invite {
    pub code: String,
    pub guild_id: Snowflake,
    pub uses: i32,
    pub max_uses: Option<i32>,
}

// ---------------------------------------------------------------- REST

#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Название устройства для списка активных сессий («Ноутбук», «Домашний ПК»)
    #[serde(default)]
    pub device_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LoginResponse {
    /// Непрозрачный токен сессии: заголовок `Authorization: Bearer <token>`
    pub token: String,
    pub user: User,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateGuildRequest {
    pub name: String,
}

/// Сервер создаётся вместе с каналами по умолчанию
#[derive(Debug, Serialize, Deserialize)]
pub struct GuildWithChannels {
    pub guild: Guild,
    pub channels: Vec<Channel>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub name: String,
    pub kind: ChannelKind,
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CreateInviteRequest {
    #[serde(default)]
    pub max_uses: Option<i32>,
    /// Срок жизни в секундах; None — бессрочно
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub reply_to_id: Option<Snowflake>,
    #[serde(default)]
    pub nonce: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EditMessageRequest {
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VoiceJoinResponse {
    /// Адрес LiveKit для клиента
    pub url: String,
    pub token: String,
    pub room: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

// ---------------------------------------------------------------- шлюз

/// Клиент → сервер
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", content = "d", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientOp {
    Identify { token: String },
    Heartbeat { seq: u64 },
    /// Полные события только по открытым серверам (экономия трафика и CPU клиента)
    SubscribeGuild { guild_id: Snowflake },
    UnsubscribeGuild { guild_id: Snowflake },
    /// Игровой режим: не присылать typing/presence, пока окно скрыто
    ClientState { background: bool },
}

/// Сервер → клиент
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", content = "d", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServerOp {
    Hello { heartbeat_interval_ms: u64 },
    Ready { session_id: String, user: User, guilds: Vec<Guild> },
    HeartbeatAck,
    Dispatch { seq: u64, event: Event },
    /// Сессия недействительна (неверный токен и т.п.); соединение будет закрыто
    InvalidSession { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", content = "data", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Event {
    MessageCreate(Message),
    MessageUpdate(Message),
    MessageDelete { id: Snowflake, channel_id: Snowflake },
    ChannelCreate(Channel),
    GuildMemberAdd { guild_id: Snowflake, user: User },
    /// Новый сервер, в котором пользователь теперь состоит (создал или вступил)
    GuildCreate(Guild),
    /// По неподписанным серверам приходят только счётчики
    /// `author_id` — чтобы клиент не помечал непрочитанными свои же сообщения (с другого устройства)
    UnreadUpdate { guild_id: Snowflake, channel_id: Snowflake, last_message_id: Snowflake, author_id: Snowflake },
}

impl Event {
    /// Полное событие или «лёгкое» (для серверов без подписки)
    pub fn is_heavy(&self) -> bool {
        !matches!(self, Event::GuildCreate(_) | Event::UnreadUpdate { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflake_is_string_in_json_and_number_in_msgpack() {
        let id = Snowflake(1234567890123456789);
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"1234567890123456789\"");
        let packed = rmp_serde::to_vec(&id).unwrap();
        assert_eq!(rmp_serde::from_slice::<Snowflake>(&packed).unwrap(), id);
        assert!(packed.len() <= 9);
    }

    #[test]
    fn gateway_ops_roundtrip_msgpack() {
        let op = ClientOp::SubscribeGuild { guild_id: Snowflake(42) };
        let bytes = rmp_serde::to_vec_named(&op).unwrap();
        assert_eq!(rmp_serde::from_slice::<ClientOp>(&bytes).unwrap(), op);
    }
}

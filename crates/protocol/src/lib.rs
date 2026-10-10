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
    /// Версия аватара (`/avatars/{user_id}/{avatar}`); None — буква вместо картинки
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<Snowflake>,
}

impl User {
    /// Как показывать: отображаемое имя или логин
    pub fn shown_name(&self) -> &str {
        self.display_name.as_deref().filter(|n| !n.is_empty()).unwrap_or(&self.username)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Guild {
    pub id: Snowflake,
    pub name: String,
    pub owner_id: Snowflake,
}

/// Права ролей (битовая маска). Владелец сервера и ADMINISTRATOR могут всё.
pub mod perms {
    pub const ADMINISTRATOR: u64 = 1 << 0;
    pub const MANAGE_GUILD: u64 = 1 << 1;
    pub const MANAGE_CHANNELS: u64 = 1 << 2;
    pub const MANAGE_ROLES: u64 = 1 << 3;
    pub const KICK_MEMBERS: u64 = 1 << 4;
    pub const BAN_MEMBERS: u64 = 1 << 5;
    pub const CREATE_INVITE: u64 = 1 << 6;
    pub const SEND_MESSAGES: u64 = 1 << 7;
    pub const MANAGE_MESSAGES: u64 = 1 << 8;
    pub const VOICE_CONNECT: u64 = 1 << 9;
    pub const VOICE_SPEAK: u64 = 1 << 10;
    /// Видеть канал (снимается в настройках канала — получается закрытый канал)
    pub const VIEW_CHANNEL: u64 = 1 << 11;
    pub const ALL: u64 = (1 << 12) - 1;
    /// Права @everyone в новом сервере
    pub const DEFAULT_EVERYONE: u64 = VIEW_CHANNEL | CREATE_INVITE | SEND_MESSAGES | VOICE_CONNECT | VOICE_SPEAK;
    /// Права, которые имеет смысл менять на уровне канала
    pub const CHANNEL_SCOPED: u64 = VIEW_CHANNEL | SEND_MESSAGES | MANAGE_MESSAGES | VOICE_CONNECT | VOICE_SPEAK;

    /// Права в канале: сначала @everyone, затем роли (запреты, потом разрешения), затем сам участник.
    /// Владелец и администратор не ограничиваются (как в Discord)
    pub fn apply_overwrites(base: u64, everyone: Option<(u64, u64)>, roles: (u64, u64), member: Option<(u64, u64)>) -> u64 {
        if base & ADMINISTRATOR != 0 {
            return ALL;
        }
        let mut p = base;
        if let Some((allow, deny)) = everyone {
            p = (p & !deny) | allow;
        }
        p = (p & !roles.1) | roles.0;
        if let Some((allow, deny)) = member {
            p = (p & !deny) | allow;
        }
        p
    }

    /// Итоговые права: объединение ролей; администратор — всё
    pub fn resolve(owner: bool, role_perms: impl IntoIterator<Item = u64>) -> u64 {
        if owner {
            return ALL;
        }
        let p = role_perms.into_iter().fold(0, |a, b| a | b);
        if p & ADMINISTRATOR != 0 { ALL } else { p }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub id: Snowflake,
    pub guild_id: Snowflake,
    pub name: String,
    /// 0xRRGGBB, 0 — без цвета
    pub color: u32,
    pub permissions: u64,
    /// Выше — главнее; @everyone всегда 0
    pub position: i32,
}

/// Участник сервера с ролями (@everyone не перечисляется — она есть у всех)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct GuildMember {
    pub user: User,
    pub roles: Vec<Snowflake>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RoleRequest {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub permissions: Option<u64>,
    pub position: Option<i32>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Text,
    Voice,
    Category,
    /// Личные сообщения (вне серверов)
    Dm,
    /// Ветка обсуждения: текстовый канал внутри текстового (`parent_id` — родительский канал)
    Thread,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverwriteKind {
    Role,
    Member,
}

/// Права канала для роли или участника: `allow` добавляет, `deny` убирает
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Overwrite {
    /// id роли (@everyone — id сервера) или пользователя
    pub id: Snowflake,
    pub kind: OverwriteKind,
    pub allow: u64,
    pub deny: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub id: Snowflake,
    pub guild_id: Snowflake,
    pub kind: ChannelKind,
    pub name: String,
    pub parent_id: Option<Snowflake>,
    pub position: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overwrites: Vec<Overwrite>,
}

/// Личный канал: собеседники (без меня) и последнее сообщение (для сортировки и непрочитанного)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DmChannel {
    pub id: Snowflake,
    pub recipients: Vec<User>,
    pub last_message_id: Option<Snowflake>,
}

/// Вложение. Картинки: `has_thumb` — есть уменьшенная копия (`/attachments/{id}/thumb`)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub id: Snowflake,
    pub filename: String,
    pub size: u64,
    pub content_type: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub has_thumb: bool,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pinned: bool,
    /// Ветка, начатая с этого сообщения
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<Snowflake>,
    /// Превью ссылок (сервер дописывает через MESSAGE_UPDATE через пару секунд после отправки)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embeds: Vec<Embed>,
}

/// Реакция: эмодзи, сколько человек поставили, есть ли среди них я
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Reaction {
    pub emoji: String,
    pub count: u32,
    #[serde(default)]
    pub me: bool,
}

/// Превью ссылки (Open Graph)
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Embed {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub site_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Забаненный на сервере
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Ban {
    pub user: User,
}

/// Запись журнала модерации
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AuditEntry {
    pub id: Snowflake,
    pub actor: User,
    /// channel_create, channel_update, channel_delete, guild_update, role_create, role_update,
    /// role_delete, member_role_add, member_role_remove, kick, ban, unban, message_delete,
    /// overwrite_update, overwrite_delete, invite_create, owner_transfer, message_pin, message_unpin
    pub action: String,
    pub target_id: Option<Snowflake>,
    /// Человекочитаемые подробности: название, имя роли и т.п.
    pub details: String,
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
    /// Код приглашения: сразу вступить в сообщество. В режиме «только по приглашению» обязателен
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<String>,
}

/// Кто может регистрироваться на этом сервере Feather
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationMode {
    /// Любой, кто знает адрес
    Open,
    /// Только с действующим кодом приглашения
    Invite,
    /// Регистрация выключена
    Closed,
}

/// Сведения о сервере Feather для экрана входа (GET /api/v1/instance)
#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub registration: RegistrationMode,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Название устройства для списка активных сессий («Ноутбук», «Домашний ПК»)
    #[serde(default)]
    pub device_name: Option<String>,
    /// Код из приложения-аутентификатора, если включена двухфакторная защита.
    /// Без него сервер ответит ошибкой `totp_required`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<String>,
}

/// Смена отображаемого имени: пустая строка — сбросить на логин
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UpdateMeRequest {
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub current: String,
    pub new: String,
}

/// Подтверждение опасного действия паролем (и кодом 2FA, если она включена)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub password: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<String>,
}

/// Начало настройки 2FA: секрет и ссылка otpauth:// для QR-кода
#[derive(Debug, Serialize, Deserialize)]
pub struct TotpSetup {
    pub secret: String,
    pub uri: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TotpCode {
    pub code: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UpdateGuildRequest {
    #[serde(default)]
    pub name: Option<String>,
    /// Передать сообщество другому участнику (только владелец)
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct UpdateChannelRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OverwriteRequest {
    pub kind: OverwriteKind,
    pub allow: u64,
    pub deny: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateThreadRequest {
    pub name: String,
}

/// Пользователь в панели администратора
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AdminUser {
    pub user: User,
    pub is_admin: bool,
    /// Время регистрации, мс Unix
    pub created_at: i64,
    pub guilds: i64,
    pub totp_enabled: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AdminSettings {
    pub registration: RegistrationMode,
}

/// Временный пароль после сброса администратором
#[derive(Debug, Serialize, Deserialize)]
pub struct TempPassword {
    pub password: String,
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

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub reply_to_id: Option<Snowflake>,
    #[serde(default)]
    pub nonce: Option<String>,
    /// Загруженные заранее вложения (POST /channels/{id}/attachments)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<Snowflake>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EditMessageRequest {
    pub content: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateDmRequest {
    pub recipient_id: Snowflake,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AckRequest {
    pub message_id: Snowflake,
}

/// Канал, в котором есть непрочитанное (приходит в READY)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct UnreadChannel {
    /// None — личный канал
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    pub last_message_id: Snowflake,
}

/// Кто в каком голосовом канале (`channel_id: None` — вышел)
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VoiceState {
    pub guild_id: Snowflake,
    pub channel_id: Option<Snowflake>,
    pub user: User,
    pub muted: bool,
    pub deafened: bool,
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
    /// Я печатаю в канале (клиент шлёт не чаще раза в несколько секунд)
    Typing { channel_id: Snowflake },
    /// Я зашёл в голосовой канал / вышел (None) / переключил микрофон или звук
    VoiceState { channel_id: Option<Snowflake>, muted: bool, deafened: bool },
}

/// Сервер → клиент
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", content = "d", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ServerOp {
    Hello { heartbeat_interval_ms: u64 },
    Ready {
        session_id: String,
        user: User,
        guilds: Vec<Guild>,
        /// Каналы с непрочитанным на момент подключения
        #[serde(default)]
        unread: Vec<UnreadChannel>,
        /// Кто сейчас в голосовых каналах серверов пользователя
        #[serde(default)]
        voice_states: Vec<VoiceState>,
        /// Я администратор этого сервера Feather (панель администратора)
        #[serde(default)]
        admin: bool,
    },
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
    ChannelUpdate(Channel),
    ChannelDelete { guild_id: Snowflake, channel_id: Snowflake },
    /// Сменились права (роли, права каналов): видимые каналы могли измениться — перезапросить список
    ChannelsInvalidated { guild_id: Snowflake },
    GuildUpdate(Guild),
    /// Пользователь сменил имя или аватар
    UserUpdate(User),
    ReactionAdd { channel_id: Snowflake, message_id: Snowflake, user_id: Snowflake, emoji: String },
    ReactionRemove { channel_id: Snowflake, message_id: Snowflake, user_id: Snowflake, emoji: String },
    GuildMemberAdd { guild_id: Snowflake, user: User },
    /// Участника выгнали/забанили или он ушёл
    GuildMemberRemove { guild_id: Snowflake, user_id: Snowflake },
    /// Сменились роли участника
    GuildMemberUpdate { guild_id: Snowflake, user_id: Snowflake, roles: Vec<Snowflake> },
    /// Меня выгнали (или сервер удалён) — убрать его из списка
    GuildDelete { guild_id: Snowflake },
    RoleCreate(Role),
    RoleUpdate(Role),
    RoleDelete { guild_id: Snowflake, role_id: Snowflake },
    /// Новый сервер, в котором пользователь теперь состоит (создал или вступил)
    GuildCreate(Guild),
    /// Новый личный канал (со мной написали впервые)
    DmCreate(DmChannel),
    VoiceStateUpdate(VoiceState),
    /// Кто-то начал печатать (показывать ~10 секунд или до его сообщения)
    TypingStart { channel_id: Snowflake, user: User },
    /// Пользователь дочитал канал (на этом или другом своём устройстве)
    ChannelAck { channel_id: Snowflake, message_id: Snowflake },
    /// По неподписанным серверам приходят только счётчики
    /// `author_id` — чтобы клиент не помечал непрочитанными свои же сообщения (с другого устройства)
    UnreadUpdate { guild_id: Snowflake, channel_id: Snowflake, last_message_id: Snowflake, author_id: Snowflake },
}

impl Event {
    /// Полное событие или «лёгкое» (для серверов без подписки)
    pub fn is_heavy(&self) -> bool {
        !matches!(
            self,
            Event::GuildCreate(_)
                | Event::GuildDelete { .. }
                | Event::GuildUpdate(_)
                | Event::DmCreate(_)
                | Event::UnreadUpdate { .. }
                | Event::ChannelAck { .. }
                | Event::VoiceStateUpdate(_)
                | Event::UserUpdate(_)
                | Event::ChannelsInvalidated { .. }
        )
    }

    /// Канал, к которому относится событие: сервер проверяет право видеть его перед отправкой
    pub fn channel_id(&self) -> Option<Snowflake> {
        match self {
            Event::MessageCreate(m) | Event::MessageUpdate(m) => Some(m.channel_id),
            Event::MessageDelete { channel_id, .. }
            | Event::TypingStart { channel_id, .. }
            | Event::ReactionAdd { channel_id, .. }
            | Event::ReactionRemove { channel_id, .. }
            | Event::ChannelDelete { channel_id, .. } => Some(*channel_id),
            Event::ChannelCreate(c) | Event::ChannelUpdate(c) => Some(c.id),
            _ => None,
        }
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
    fn permissions_resolve() {
        use perms::*;
        assert_eq!(resolve(true, []), ALL, "владелец может всё");
        assert_eq!(resolve(false, [SEND_MESSAGES, KICK_MEMBERS]), SEND_MESSAGES | KICK_MEMBERS);
        assert_eq!(resolve(false, [ADMINISTRATOR]), ALL);
    }

    #[test]
    fn channel_overwrites() {
        use perms::*;
        let base = DEFAULT_EVERYONE;
        // Закрытый канал: @everyone без VIEW, роле «Модер» — можно
        let hidden = apply_overwrites(base, Some((0, VIEW_CHANNEL)), (0, 0), None);
        assert_eq!(hidden & VIEW_CHANNEL, 0);
        let moder = apply_overwrites(base, Some((0, VIEW_CHANNEL)), (VIEW_CHANNEL, 0), None);
        assert_ne!(moder & VIEW_CHANNEL, 0);
        // Запрет конкретному участнику сильнее разрешения роли
        let muted = apply_overwrites(base, None, (SEND_MESSAGES, 0), Some((0, SEND_MESSAGES)));
        assert_eq!(muted & SEND_MESSAGES, 0);
        assert_eq!(apply_overwrites(ADMINISTRATOR, Some((0, ALL)), (0, ALL), Some((0, ALL))), ALL);
    }

    #[test]
    fn gateway_ops_roundtrip_msgpack() {
        let op = ClientOp::SubscribeGuild { guild_id: Snowflake(42) };
        let bytes = rmp_serde::to_vec_named(&op).unwrap();
        assert_eq!(rmp_serde::from_slice::<ClientOp>(&bytes).unwrap(), op);
    }
}

//! REST-клиент сервера Feather (`/api/v1`).

use std::fmt;

use feather_protocol::*;
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

#[derive(Debug, Clone)]
pub enum Error {
    /// Нет связи, таймаут, неверный адрес
    Network(String),
    /// Сервер ответил ошибкой
    Api { status: u16, code: String, message: String },
}

impl Error {
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Error::Api { status: 401, .. })
    }

    /// Код ошибки сервера (`totp_required`, `owns_guilds`, …)
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Api { code, .. } => Some(code),
            Error::Network(_) => None,
        }
    }
}

/// Эмодзи и прочее в пути адреса
fn path_escape(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(e) => write!(f, "нет связи с сервером ({e})"),
            Error::Api { message, .. } => f.write_str(message),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

/// Адрес без схемы: для локальной разработки — `http://`, для остальных — `https://`
pub fn normalize_base(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.contains("://") {
        return base.to_owned();
    }
    let host = base.split(['/', ':']).next().unwrap_or_default();
    let local = host == "localhost" || host.starts_with("127.") || host.starts_with("192.168.") || host.starts_with("10.");
    format!("{}://{base}", if local { "http" } else { "https" })
}

impl Api {
    /// `base` — адрес сервера, например `https://chat.example.com` или `http://127.0.0.1:8080`
    pub fn new(base: &str) -> Self {
        let base = normalize_base(base);
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("HTTP-клиент");
        Self { http, base, token: None }
    }

    pub fn with_token(mut self, token: String) -> Self {
        self.token = Some(token);
        self
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// Адрес шлюза: http → ws, https → wss
    pub fn gateway_url(&self) -> String {
        let ws = self.base.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
        format!("{ws}/gateway")
    }

    async fn call<B: Serialize, R: DeserializeOwned>(&self, method: Method, path: &str, body: Option<&B>) -> Result<R> {
        let mut req = self.http.request(method, format!("{}/api/v1{path}", self.base));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| Error::Network(e.to_string()))?;
        if status.is_success() {
            // Пустой ответ (204) читаем как JSON null — подходит для R = ()
            let bytes = if status == StatusCode::NO_CONTENT || bytes.is_empty() { &b"null"[..] } else { &bytes[..] };
            return serde_json::from_slice(bytes).map_err(|e| Error::Network(format!("непонятный ответ: {e}")));
        }
        let err: ApiError = serde_json::from_slice(&bytes)
            .unwrap_or(ApiError { code: "http".into(), message: format!("ошибка сервера {status}") });
        Err(Error::Api { status: status.as_u16(), code: err.code, message: err.message })
    }

    /// Режим регистрации сервера Feather (для экрана входа)
    pub async fn instance(&self) -> Result<InstanceInfo> {
        self.call::<(), _>(Method::GET, "/instance", None).await
    }

    pub async fn register(&self, username: &str, password: &str, invite_code: Option<&str>) -> Result<User> {
        let req = RegisterRequest { username: username.into(), password: password.into(), invite_code: invite_code.map(Into::into) };
        self.call(Method::POST, "/auth/register", Some(&req)).await
    }

    pub async fn login(&self, username: &str, password: &str, device: &str, totp: Option<&str>) -> Result<LoginResponse> {
        let req = LoginRequest { username: username.into(), password: password.into(), device_name: Some(device.into()), totp: totp.map(Into::into) };
        self.call(Method::POST, "/auth/login", Some(&req)).await
    }

    pub async fn logout(&self) -> Result<()> {
        self.call::<(), ()>(Method::POST, "/auth/logout", None).await
    }

    pub async fn me(&self) -> Result<User> {
        self.call::<(), _>(Method::GET, "/users/@me", None).await
    }

    pub async fn guilds(&self) -> Result<Vec<Guild>> {
        self.call::<(), _>(Method::GET, "/users/@me/guilds", None).await
    }

    pub async fn create_guild(&self, name: &str) -> Result<GuildWithChannels> {
        self.call(Method::POST, "/guilds", Some(&CreateGuildRequest { name: name.into() })).await
    }

    pub async fn channels(&self, guild: Snowflake) -> Result<Vec<Channel>> {
        self.call::<(), _>(Method::GET, &format!("/guilds/{guild}/channels"), None).await
    }

    /// Открыть (или создать) личный канал с пользователем
    pub async fn open_dm(&self, user: Snowflake) -> Result<DmChannel> {
        self.call(Method::POST, "/users/@me/channels", Some(&CreateDmRequest { recipient_id: user })).await
    }

    pub async fn dms(&self) -> Result<Vec<DmChannel>> {
        self.call::<(), _>(Method::GET, "/users/@me/channels", None).await
    }

    pub async fn create_channel(&self, guild: Snowflake, name: &str, kind: ChannelKind, parent: Option<Snowflake>) -> Result<Channel> {
        let req = CreateChannelRequest { name: name.into(), kind, parent_id: parent };
        self.call(Method::POST, &format!("/guilds/{guild}/channels"), Some(&req)).await
    }

    pub async fn members(&self, guild: Snowflake) -> Result<Vec<GuildMember>> {
        self.call::<(), _>(Method::GET, &format!("/guilds/{guild}/members"), None).await
    }

    pub async fn roles(&self, guild: Snowflake) -> Result<Vec<Role>> {
        self.call::<(), _>(Method::GET, &format!("/guilds/{guild}/roles"), None).await
    }

    pub async fn create_role(&self, guild: Snowflake, req: &RoleRequest) -> Result<Role> {
        self.call(Method::POST, &format!("/guilds/{guild}/roles"), Some(req)).await
    }

    pub async fn update_role(&self, guild: Snowflake, role: Snowflake, req: &RoleRequest) -> Result<Role> {
        self.call(Method::PATCH, &format!("/guilds/{guild}/roles/{role}"), Some(req)).await
    }

    pub async fn delete_role(&self, guild: Snowflake, role: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/guilds/{guild}/roles/{role}"), None).await
    }

    /// Выдать (`add`) или снять роль у участника
    pub async fn set_member_role(&self, guild: Snowflake, user: Snowflake, role: Snowflake, add: bool) -> Result<()> {
        let method = if add { Method::PUT } else { Method::DELETE };
        self.call::<(), ()>(method, &format!("/guilds/{guild}/members/{user}/roles/{role}"), None).await
    }

    /// Выгнать участника (или выйти самому, если это я)
    pub async fn kick(&self, guild: Snowflake, user: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/guilds/{guild}/members/{user}"), None).await
    }

    pub async fn create_invite(&self, guild: Snowflake) -> Result<Invite> {
        self.call(Method::POST, &format!("/guilds/{guild}/invites"), Some(&CreateInviteRequest::default())).await
    }

    pub async fn accept_invite(&self, code: &str) -> Result<Guild> {
        self.call::<(), _>(Method::POST, &format!("/invites/{}", code.trim()), None).await
    }

    /// История: новые сверху (как отдаёт сервер)
    pub async fn history(&self, channel: Snowflake, before: Option<Snowflake>, limit: u32) -> Result<Vec<Message>> {
        let mut path = format!("/channels/{channel}/messages?limit={limit}");
        if let Some(b) = before {
            path.push_str(&format!("&before={b}"));
        }
        self.call::<(), _>(Method::GET, &path, None).await
    }

    /// Токен LiveKit для входа в голосовой канал
    pub async fn voice_join(&self, channel: Snowflake) -> Result<VoiceJoinResponse> {
        self.call::<(), _>(Method::POST, &format!("/channels/{channel}/voice/join"), None).await
    }

    /// «Дочитал канал до этого сообщения»
    pub async fn ack(&self, channel: Snowflake, message: Snowflake) -> Result<()> {
        self.call(Method::POST, &format!("/channels/{channel}/ack"), Some(&AckRequest { message_id: message })).await
    }

    pub async fn edit_message(&self, channel: Snowflake, id: Snowflake, content: &str) -> Result<Message> {
        self.call(Method::PATCH, &format!("/channels/{channel}/messages/{id}"), Some(&EditMessageRequest { content: content.into() })).await
    }

    pub async fn delete_message(&self, channel: Snowflake, id: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/channels/{channel}/messages/{id}"), None).await
    }

    pub async fn send_message(&self, channel: Snowflake, content: &str, nonce: String) -> Result<Message> {
        self.send_with_attachments(channel, content, nonce, Vec::new()).await
    }

    pub async fn send_with_attachments(&self, channel: Snowflake, content: &str, nonce: String, attachments: Vec<Snowflake>) -> Result<Message> {
        let req = SendMessageRequest { content: content.into(), nonce: Some(nonce), attachment_ids: attachments, ..Default::default() };
        self.call(Method::POST, &format!("/channels/{channel}/messages"), Some(&req)).await
    }

    pub async fn send_full(&self, channel: Snowflake, req: &SendMessageRequest) -> Result<Message> {
        self.call(Method::POST, &format!("/channels/{channel}/messages"), Some(req)).await
    }

    // ---------------------------------------------------------------- аккаунт

    pub async fn update_me(&self, display_name: &str) -> Result<User> {
        self.call(Method::PATCH, "/users/@me", Some(&UpdateMeRequest { display_name: Some(display_name.into()) })).await
    }

    pub async fn change_password(&self, current: &str, new: &str) -> Result<()> {
        self.call(Method::POST, "/users/@me/password", Some(&ChangePasswordRequest { current: current.into(), new: new.into() })).await
    }

    pub async fn set_avatar(&self, bytes: Vec<u8>) -> Result<User> {
        let mut req = self.http.put(format!("{}/api/v1/users/@me/avatar", self.base)).body(bytes).timeout(std::time::Duration::from_secs(60));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| Error::Network(e.to_string()))?;
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|e| Error::Network(e.to_string()));
        }
        let err: ApiError = serde_json::from_slice(&body).unwrap_or(ApiError { code: "http".into(), message: format!("ошибка сервера {status}") });
        Err(Error::Api { status: status.as_u16(), code: err.code, message: err.message })
    }

    pub async fn remove_avatar(&self) -> Result<User> {
        self.call::<(), _>(Method::DELETE, "/users/@me/avatar", None).await
    }

    /// Аватар пользователя (PNG 128×128)
    pub async fn avatar(&self, user: Snowflake, version: Snowflake) -> Result<Vec<u8>> {
        self.get_bytes(&format!("/avatars/{user}/{version}")).await
    }

    pub async fn totp_status(&self) -> Result<bool> {
        self.call::<(), _>(Method::GET, "/users/@me/2fa", None).await
    }

    pub async fn totp_setup(&self) -> Result<TotpSetup> {
        self.call::<(), _>(Method::POST, "/users/@me/2fa/setup", None).await
    }

    pub async fn totp_enable(&self, code: &str) -> Result<()> {
        self.call(Method::POST, "/users/@me/2fa/enable", Some(&TotpCode { code: code.into() })).await
    }

    pub async fn totp_disable(&self, password: &str, code: &str) -> Result<()> {
        let req = ConfirmRequest { password: password.into(), totp: Some(code.into()) };
        self.call(Method::DELETE, "/users/@me/2fa", Some(&req)).await
    }

    pub async fn delete_account(&self, password: &str, code: Option<&str>) -> Result<()> {
        let req = ConfirmRequest { password: password.into(), totp: code.map(Into::into) };
        self.call(Method::DELETE, "/users/@me", Some(&req)).await
    }

    // ---------------------------------------------------------------- сообщества и каналы

    pub async fn update_guild(&self, guild: Snowflake, req: &UpdateGuildRequest) -> Result<Guild> {
        self.call(Method::PATCH, &format!("/guilds/{guild}"), Some(req)).await
    }

    pub async fn delete_guild(&self, guild: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/guilds/{guild}"), None).await
    }

    pub async fn update_channel(&self, channel: Snowflake, req: &UpdateChannelRequest) -> Result<Channel> {
        self.call(Method::PATCH, &format!("/channels/{channel}"), Some(req)).await
    }

    pub async fn delete_channel(&self, channel: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/channels/{channel}"), None).await
    }

    pub async fn put_overwrite(&self, channel: Snowflake, target: Snowflake, req: &OverwriteRequest) -> Result<Channel> {
        self.call(Method::PUT, &format!("/channels/{channel}/overwrites/{target}"), Some(req)).await
    }

    pub async fn delete_overwrite(&self, channel: Snowflake, target: Snowflake) -> Result<Channel> {
        self.call::<(), _>(Method::DELETE, &format!("/channels/{channel}/overwrites/{target}"), None).await
    }

    pub async fn bans(&self, guild: Snowflake) -> Result<Vec<Ban>> {
        self.call::<(), _>(Method::GET, &format!("/guilds/{guild}/bans"), None).await
    }

    pub async fn ban(&self, guild: Snowflake, user: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::PUT, &format!("/guilds/{guild}/bans/{user}"), None).await
    }

    pub async fn unban(&self, guild: Snowflake, user: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/guilds/{guild}/bans/{user}"), None).await
    }

    pub async fn audit_log(&self, guild: Snowflake) -> Result<Vec<AuditEntry>> {
        self.call::<(), _>(Method::GET, &format!("/guilds/{guild}/audit-log"), None).await
    }

    // ---------------------------------------------------------------- сообщения

    pub async fn react(&self, channel: Snowflake, message: Snowflake, emoji: &str, add: bool) -> Result<()> {
        let method = if add { Method::PUT } else { Method::DELETE };
        self.call::<(), ()>(method, &format!("/channels/{channel}/messages/{message}/reactions/{}", path_escape(emoji)), None).await
    }

    pub async fn pin(&self, channel: Snowflake, message: Snowflake, on: bool) -> Result<()> {
        let method = if on { Method::PUT } else { Method::DELETE };
        self.call::<(), ()>(method, &format!("/channels/{channel}/pins/{message}"), None).await
    }

    pub async fn pins(&self, channel: Snowflake) -> Result<Vec<Message>> {
        self.call::<(), _>(Method::GET, &format!("/channels/{channel}/pins"), None).await
    }

    /// Поиск: по сообществу (`guild`) или по одному каналу
    pub async fn search(&self, guild: Option<Snowflake>, channel: Snowflake, query: &str) -> Result<Vec<Message>> {
        let base = match guild {
            Some(g) => format!("/guilds/{g}/search"),
            None => format!("/channels/{channel}/search"),
        };
        self.call::<(), _>(Method::GET, &format!("{base}?q={}", path_escape(query)), None).await
    }

    pub async fn create_thread(&self, channel: Snowflake, message: Snowflake, name: &str) -> Result<Channel> {
        self.call(Method::POST, &format!("/channels/{channel}/messages/{message}/threads"), Some(&CreateThreadRequest { name: name.into() })).await
    }

    // ---------------------------------------------------------------- администратор

    pub async fn admin_users(&self) -> Result<Vec<AdminUser>> {
        self.call::<(), _>(Method::GET, "/admin/users", None).await
    }

    pub async fn admin_delete_user(&self, user: Snowflake) -> Result<()> {
        self.call::<(), ()>(Method::DELETE, &format!("/admin/users/{user}"), None).await
    }

    pub async fn admin_reset_password(&self, user: Snowflake) -> Result<TempPassword> {
        self.call::<(), _>(Method::POST, &format!("/admin/users/{user}/password-reset"), None).await
    }

    pub async fn admin_settings(&self) -> Result<AdminSettings> {
        self.call::<(), _>(Method::GET, "/admin/settings", None).await
    }

    pub async fn admin_set_registration(&self, mode: RegistrationMode) -> Result<AdminSettings> {
        self.call(Method::PUT, "/admin/settings", Some(&AdminSettings { registration: mode })).await
    }

    async fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let mut req = self.http.get(format!("{}/api/v1{path}", self.base));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api { status: status.as_u16(), code: "http".into(), message: format!("недоступно ({status})") });
        }
        Ok(resp.bytes().await.map_err(|e| Error::Network(e.to_string()))?.to_vec())
    }

    /// Загрузить файл в канал (до отправки сообщения)
    pub async fn upload(&self, channel: Snowflake, filename: &str, content_type: &str, bytes: Vec<u8>) -> Result<Attachment> {
        let mut url = reqwest::Url::parse(&format!("{}/api/v1/channels/{channel}/attachments", self.base)).map_err(|e| Error::Network(e.to_string()))?;
        url.query_pairs_mut().append_pair("filename", filename);
        let mut req = self.http.post(url).header("content-type", content_type).body(bytes).timeout(std::time::Duration::from_secs(120));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        let body = resp.bytes().await.map_err(|e| Error::Network(e.to_string()))?;
        if status.is_success() {
            return serde_json::from_slice(&body).map_err(|e| Error::Network(e.to_string()));
        }
        let err: ApiError = serde_json::from_slice(&body).unwrap_or(ApiError { code: "http".into(), message: format!("ошибка сервера {status}") });
        Err(Error::Api { status: status.as_u16(), code: err.code, message: err.message })
    }

    /// Скачать вложение (`thumb` — уменьшенную копию картинки)
    pub async fn download(&self, id: Snowflake, thumb: bool) -> Result<Vec<u8>> {
        let path = if thumb { format!("/attachments/{id}/thumb") } else { format!("/attachments/{id}") };
        let mut req = self.http.get(format!("{}/api/v1{path}", self.base)).timeout(std::time::Duration::from_secs(120));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| Error::Network(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Api { status: status.as_u16(), code: "http".into(), message: format!("файл недоступен ({status})") });
        }
        Ok(resp.bytes().await.map_err(|e| Error::Network(e.to_string()))?.to_vec())
    }
}

#[cfg(test)]
mod base_tests {
    use super::normalize_base;

    #[test]
    fn scheme_by_default() {
        assert_eq!(normalize_base(" chat.example.com/ "), "https://chat.example.com");
        assert_eq!(normalize_base("127.0.0.1:8080"), "http://127.0.0.1:8080");
        assert_eq!(normalize_base("localhost:8080"), "http://localhost:8080");
        assert_eq!(normalize_base("http://x.test"), "http://x.test");
    }
}

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

impl Api {
    /// `base` — адрес сервера, например `http://127.0.0.1:8080`
    pub fn new(base: &str) -> Self {
        let mut base = base.trim().trim_end_matches('/').to_owned();
        if !base.contains("://") {
            base = format!("http://{base}");
        }
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

    pub async fn register(&self, username: &str, password: &str) -> Result<User> {
        let req = RegisterRequest { username: username.into(), password: password.into() };
        self.call(Method::POST, "/auth/register", Some(&req)).await
    }

    pub async fn login(&self, username: &str, password: &str, device: &str) -> Result<LoginResponse> {
        let req = LoginRequest { username: username.into(), password: password.into(), device_name: Some(device.into()) };
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

    pub async fn members(&self, guild: Snowflake) -> Result<Vec<User>> {
        self.call::<(), _>(Method::GET, &format!("/guilds/{guild}/members"), None).await
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

    pub async fn send_message(&self, channel: Snowflake, content: &str, nonce: String) -> Result<Message> {
        let req = SendMessageRequest { content: content.into(), reply_to_id: None, nonce: Some(nonce) };
        self.call(Method::POST, &format!("/channels/{channel}/messages"), Some(&req)).await
    }
}

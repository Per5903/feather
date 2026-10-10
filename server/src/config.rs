//! Настройки сервера из переменных окружения (удобно для docker-compose).

use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,
    /// Номер узла для Snowflake (0..1023). В режиме одного сервера — 0
    pub worker_id: u16,
    /// Адрес LiveKit, который сервер отдаёт клиентам
    pub livekit_url: String,
    pub livekit_api_key: String,
    pub livekit_api_secret: String,
    pub session_ttl_days: i64,
    /// Сервер стоит за обратным прокси (Caddy): IP клиента брать из X-Forwarded-For
    pub trust_proxy: bool,
    /// Каталог для файлов (вложения)
    pub data_dir: std::path::PathBuf,
    /// Регистрация: open | invite | closed (FEATHER_REGISTRATION)
    pub registration: feather_protocol::RegistrationMode,
}

impl Config {
    pub fn from_env() -> Self {
        let var = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.to_owned());
        Self {
            database_url: var("DATABASE_URL", "postgres://feather:feather_dev_password@127.0.0.1:5432/feather"),
            bind: var("FEATHER_BIND", "127.0.0.1:8080").parse().expect("FEATHER_BIND: неверный адрес"),
            worker_id: var("FEATHER_WORKER_ID", "0").parse().expect("FEATHER_WORKER_ID: нужно число"),
            livekit_url: var("LIVEKIT_URL", "ws://127.0.0.1:7880"),
            livekit_api_key: var("LIVEKIT_API_KEY", "devkey"),
            livekit_api_secret: var("LIVEKIT_API_SECRET", "dev_secret_change_me_at_least_32_chars_long"),
            session_ttl_days: var("FEATHER_SESSION_TTL_DAYS", "30").parse().expect("FEATHER_SESSION_TTL_DAYS: нужно число"),
            trust_proxy: var("FEATHER_TRUST_PROXY", "0") == "1",
            data_dir: var("FEATHER_DATA_DIR", "data").into(),
            registration: match var("FEATHER_REGISTRATION", "open").as_str() {
                "invite" => feather_protocol::RegistrationMode::Invite,
                "closed" => feather_protocol::RegistrationMode::Closed,
                "open" => feather_protocol::RegistrationMode::Open,
                other => panic!("FEATHER_REGISTRATION: open | invite | closed, а не {other}"),
            },
        }
    }
}

//! Сохранённая сессия: адрес сервера, имя и токен, чтобы не вводить пароль при каждом запуске.
//!
//! Файл: ~/.config/feather/session.json (права 600). FEATHER_PROFILE=имя — отдельный файл,
//! чтобы запускать несколько клиентов на одной машине (тесты, второй аккаунт).
//! TODO: хранить токен в системном хранилище ключей (Secret Service / Windows Credential Manager).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Session {
    pub server: String,
    pub username: String,
    pub token: Option<String>,
    /// Последний открытый канал на каждом сервере: id сервера → id канала
    #[serde(default)]
    pub last_channels: HashMap<String, String>,
    /// Последний открытый сервер
    #[serde(default)]
    pub last_guild: Option<String>,
}

fn path() -> Option<PathBuf> {
    let name = match std::env::var("FEATHER_PROFILE") {
        Ok(p) if !p.is_empty() => format!("session-{p}.json"),
        _ => "session.json".into(),
    };
    Some(feather_core::files::config_dir()?.join(name))
}

pub fn load() -> Option<Session> {
    serde_json::from_slice(&std::fs::read(path()?).ok()?).ok()
}

pub fn save(s: &Session) {
    let Some(p) = path() else { return };
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, serde_json::to_vec_pretty(s)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    };
    if let Err(e) = write() {
        eprintln!("не удалось сохранить сессию: {e}");
    }
}

/// Выход: забываем токен, но помним сервер и имя для следующего входа
pub fn forget_token() {
    if let Some(mut s) = load() {
        s.token = None;
        save(&s);
    }
}

/// Запомнить, где пользователь был, чтобы открыть там же при следующем запуске
pub fn remember(guild: &str, channel: &str) {
    if let Some(mut s) = load() {
        s.last_guild = Some(guild.to_owned());
        s.last_channels.insert(guild.to_owned(), channel.to_owned());
        save(&s);
    }
}

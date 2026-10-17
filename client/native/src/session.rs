//! Сохранённая сессия: адрес сервера, имя и токен, чтобы не вводить пароль при каждом запуске.
//!
//! Файл: ~/.config/feather/session.json (права 600). FEATHER_PROFILE=имя — отдельный файл,
//! чтобы запускать несколько клиентов на одной машине (тесты, второй аккаунт).
//! Сам токен — в системном хранилище ключей (Secret Service / диспетчер учётных данных Windows);
//! если хранилища нет, он остаётся в файле.

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
    /// Токен лежит в системном хранилище ключей, а не в этом файле
    #[serde(default)]
    pub token_in_keyring: bool,
}

/// Имя записи в хранилище ключей: адрес + имя (+ профиль для тестов)
fn keyring_account(server: &str, username: &str) -> String {
    let profile = std::env::var("FEATHER_PROFILE").unwrap_or_default();
    format!("{server}|{username}|{profile}")
}

fn path() -> Option<PathBuf> {
    let name = match std::env::var("FEATHER_PROFILE") {
        Ok(p) if !p.is_empty() => format!("session-{p}.json"),
        _ => "session.json".into(),
    };
    Some(feather_core::files::config_dir()?.join(name))
}

/// Сессия из файла, токен — из хранилища ключей, если он там
pub fn load() -> Option<Session> {
    let mut s: Session = serde_json::from_slice(&std::fs::read(path()?).ok()?).ok()?;
    if s.token.is_none() && s.token_in_keyring {
        s.token = feather_core::secrets::load(&keyring_account(&s.server, &s.username));
    }
    Some(s)
}

/// Сохранить; токен по возможности уходит в хранилище ключей, в файл — только если его нет
pub fn save(s: &Session) {
    let mut s = s.clone();
    if let Some(token) = s.token.as_deref() {
        // FEATHER_NO_KEYRING — для автотестов (несколько клиентов, нет сеанса D-Bus)
        if std::env::var_os("FEATHER_NO_KEYRING").is_none() && feather_core::secrets::store(&keyring_account(&s.server, &s.username), token) {
            s.token = None;
            s.token_in_keyring = true;
        } else {
            s.token_in_keyring = false;
        }
    }
    write(&s);
}

fn write(s: &Session) {
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
        if s.token_in_keyring {
            feather_core::secrets::forget(&keyring_account(&s.server, &s.username));
        }
        s.token = None;
        s.token_in_keyring = false;
        write(&s);
    }
}

/// Запомнить, где пользователь был, чтобы открыть там же при следующем запуске
pub fn remember(guild: &str, channel: &str) {
    if let Some(mut s) = load() {
        s.last_guild = Some(guild.to_owned());
        s.last_channels.insert(guild.to_owned(), channel.to_owned());
        if s.token_in_keyring {
            s.token = None; // уже в хранилище
        }
        write(&s);
    }
}

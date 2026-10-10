//! Сохранённая сессия: адрес сервера, имя и токен, чтобы не вводить пароль при каждом запуске.
//!
//! Файл: ~/.config/feather/session.json (права 600). FEATHER_PROFILE=имя — отдельный файл,
//! чтобы запускать несколько клиентов на одной машине (тесты, второй аккаунт).
//! TODO: хранить токен в системном хранилище ключей (Secret Service / Windows Credential Manager).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Session {
    pub server: String,
    pub username: String,
    pub token: Option<String>,
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let name = match std::env::var("FEATHER_PROFILE") {
        Ok(p) if !p.is_empty() => format!("session-{p}.json"),
        _ => "session.json".into(),
    };
    Some(base.join("feather").join(name))
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

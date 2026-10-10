//! Настройки клиента: ~/.config/feather/settings.json (с FEATHER_PROFILE — settings-<профиль>.json).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub push_to_talk: bool,
    /// Имя устройства; None — системное по умолчанию
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    /// Программная отрисовка: 55 МБ вместо 118 МБ (bench/README.md, замер 11)
    pub economy_mode: bool,
    /// Уведомления о личных сообщениях и упоминаниях
    pub notifications: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            push_to_talk: false,
            input_device: None,
            output_device: None,
            noise_suppression: true,
            echo_cancellation: true,
            economy_mode: false,
            notifications: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    let name = match std::env::var("FEATHER_PROFILE") {
        Ok(p) if !p.is_empty() => format!("settings-{p}.json"),
        _ => "settings.json".into(),
    };
    Some(base.join("feather").join(name))
}

pub fn load() -> Settings {
    path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save(s: &Settings) {
    let Some(p) = path() else { return };
    let res = std::fs::create_dir_all(p.parent().unwrap()).and_then(|_| std::fs::write(&p, serde_json::to_vec_pretty(s).unwrap_or_default()));
    if let Err(e) = res {
        eprintln!("не удалось сохранить настройки: {e}");
    }
}

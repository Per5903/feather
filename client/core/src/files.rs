//! Файлы: системный выбор файлов (Linux: XDG-портал), тип по расширению, папка загрузок.

use std::path::PathBuf;

/// Системное окно выбора файлов. Пусто — пользователь передумал.
#[cfg(target_os = "linux")]
pub async fn pick_files() -> Vec<PathBuf> {
    use ashpd::desktop::file_chooser::SelectedFiles;
    let Ok(req) = SelectedFiles::open_file().title("Прикрепить файлы").multiple(true).send().await else { return Vec::new() };
    let Ok(files) = req.response() else { return Vec::new() };
    files.uris().iter().filter_map(|u| file_uri_to_path(&u.to_string())).collect()
}

#[cfg(not(target_os = "linux"))]
pub async fn pick_files() -> Vec<PathBuf> {
    // Системный диалог Windows/macOS; блокирующий — в отдельном потоке
    tokio::task::spawn_blocking(|| rfd::FileDialog::new().set_title("Прикрепить файлы").pick_files().unwrap_or_default())
        .await
        .unwrap_or_default()
}

/// Каталог настроек: Linux ~/.config/feather, Windows %APPDATA%\Feather
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Feather"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("feather"))
    }
}

/// Открыть файл программой по умолчанию
pub fn open_path(path: &std::path::Path) {
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(path).spawn();
}

/// file:///home/user/%D0%A1%D0%BA%D1%80%D0%B8%D0%BD.png → путь
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let raw = uri.strip_prefix("file://")?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8(out).ok()?))
}

/// MIME-тип по расширению (тип картинок сервер всё равно проверяет по содержимому)
pub fn content_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("txt" | "log") => "text/plain",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Куда сохранять скачанное: ~/Загрузки/Feather (XDG_DOWNLOAD_DIR), иначе ~/Downloads/Feather
pub fn downloads_dir() -> PathBuf {
    #[cfg(windows)]
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile).join("Downloads").join("Feather");
    }
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let xdg = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).ok().and_then(|s| {
        s.lines().find_map(|l| l.strip_prefix("XDG_DOWNLOAD_DIR=").map(|v| v.trim_matches('"').replace("$HOME", &home.to_string_lossy())))
    });
    xdg.map(PathBuf::from).unwrap_or_else(|| home.join("Downloads")).join("Feather")
}

/// Человеческий размер: «1,2 МБ»
pub fn human_size(bytes: u64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} Б"),
        b if b < 1024 * 1024 => format!("{:.0} КБ", b as f64 / 1024.0),
        b => format!("{:.1} МБ", b as f64 / 1024.0 / 1024.0).replace('.', ","),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_decoding() {
        assert_eq!(
            file_uri_to_path("file:///home/user/%D0%A1%D0%BA%D1%80%D0%B8%D0%BD%201.png"),
            Some(PathBuf::from("/home/user/Скрин 1.png"))
        );
        assert_eq!(file_uri_to_path("https://x"), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(500), "500 Б");
        assert_eq!(human_size(1_300_000), "1,2 МБ");
    }
}

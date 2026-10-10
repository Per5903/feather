// Не открывать консольное окно на Windows в релизе
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use tauri::Manager;

fn main() {
    #[cfg(target_os = "linux")]
    linux::tune_webkit_env();

    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "linux")]
            if let Some(window) = app.get_webview_window("main") {
                window.with_webview(linux::tune_webview)?;
            }
            #[cfg(not(target_os = "linux"))]
            let _ = app;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("ошибка запуска Feather");
}

#[cfg(target_os = "linux")]
mod linux {
    use webkit2gtk::{CacheModel, WebContextExt, WebViewExt};

    /// Переменные окружения WebKitGTK, снижающие память и CPU (замеры в bench/README.md).
    /// Пользователь может переопределить любую из них, задав её сам.
    pub fn tune_webkit_env() {
        const DEFAULTS: &[(&str, &str)] = &[
            // Без DMA-BUF рендерера и композитинга: ~-35 МБ и меньше CPU для простого UI
            ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
            ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
        ];
        for (key, value) in DEFAULTS {
            if std::env::var_os(key).is_none() {
                // SAFETY: вызывается в самом начале main, до запуска других потоков
                unsafe { std::env::set_var(key, value) };
            }
        }
    }

    /// Чат — не браузер: кэш страниц и большие кэши ресурсов не нужны.
    pub fn tune_webview(webview: tauri::webview::PlatformWebview) {
        if let Some(ctx) = webview.inner().context() {
            ctx.set_cache_model(CacheModel::DocumentViewer);
        }
    }
}

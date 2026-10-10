// Не открывать консольное окно на Windows в релизе
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod client;
mod demo;
mod mock;
mod session;
mod settings;
mod util;

use std::time::Duration;

use slint::{ComponentHandle, Timer, TimerMode};

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    // WebRTC при подключении проверяет видеодекодер NVIDIA и поднимает контекст CUDA:
    // +80 МБ памяти ради видео, которого в голосе нет (bench/README.md, замер 11).
    // Аппаратное КОДИРОВАНИЕ для демонстрации экрана это не отключает.
    if std::env::var_os("LK_DISABLE_NVDEC").is_none() {
        // SAFETY: в самом начале main, до запуска других потоков
        unsafe { std::env::set_var("LK_DISABLE_NVDEC", "1") };
    }
    // Экономный режим: программная отрисовка (выбирается до создания окна)
    if settings::load().economy_mode && std::env::var_os("SLINT_BACKEND").is_none() {
        // SAFETY: в самом начале main, до запуска других потоков
        unsafe { std::env::set_var("SLINT_BACKEND", "winit-software") };
    }
    let ui = AppWindow::new()?;

    // Сеть и шлюз живут в tokio; двух потоков чату хватает с запасом (лёгкость — D5)
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("feather-net")
        .enable_all()
        .build()
        .expect("не удалось запустить tokio");

    let _demo_timers = if std::env::var_os("FEATHER_DEMO").is_some() {
        demo::run(&ui)
    } else {
        client::run(&ui, rt.handle().clone());
        Vec::new()
    };
    let _bench = bench_modes(&ui);

    ui.run()
}

/// Режимы для замеров и проверок (bench/README.md). Таймеры должны жить до конца работы.
fn bench_modes(ui: &AppWindow) -> Vec<Timer> {
    let mut timers = Vec::new();

    // FEATHER_WINDOW=2560x1440 — размер окна в пикселях
    if let Some((w, h)) = std::env::var("FEATHER_WINDOW").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }) {
        ui.window().set_size(slint::PhysicalSize::new(w, h));
    }

    // FEATHER_AUTOSCROLL=1 — непрерывная прокрутка ленты (60 шагов в секунду)
    if std::env::var_os("FEATHER_AUTOSCROLL").is_some() {
        let weak = ui.as_weak();
        let dir = std::cell::Cell::new(1.0f32);
        let t = Timer::default();
        t.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            if let Some(ui) = weak.upgrade()
                && ui.invoke_scroll_by(40.0 * dir.get())
            {
                dir.set(-dir.get());
            }
        });
        timers.push(t);
    }

    // FEATHER_SNAPSHOT=путь.png — снимок окна через FEATHER_SNAPSHOT_DELAY секунд (3) и выход
    if let Some(path) = std::env::var_os("FEATHER_SNAPSHOT") {
        let delay = std::env::var("FEATHER_SNAPSHOT_DELAY").ok().and_then(|s| s.parse().ok()).unwrap_or(3.0);
        let weak = ui.as_weak();
        Timer::single_shot(Duration::from_secs_f64(delay), move || {
            if let Some(ui) = weak.upgrade() {
                if let Err(e) = save_snapshot(&ui, path.as_ref()) {
                    eprintln!("снимок не сохранён: {e}");
                }
                let _ = slint::quit_event_loop();
            }
        });
    }
    timers
}

fn save_snapshot(ui: &AppWindow, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let shot = ui.window().take_snapshot()?;
    let mut enc = png::Encoder::new(std::fs::File::create(path)?, shot.width(), shot.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(shot.as_bytes())?;
    Ok(())
}

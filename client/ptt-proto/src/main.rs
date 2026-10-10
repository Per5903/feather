//! Прототип глобального push-to-talk через XDG-портал GlobalShortcuts (Wayland, этап 0).
//!
//! На Wayland приложение не может само перехватывать клавиши в чужих окнах (в игре).
//! Правильный путь — попросить портал: рабочий стол показывает пользователю диалог
//! назначения клавиши и потом присылает события «нажата»/«отпущена» даже поверх игры.
//!
//! Запуск: cargo run -p feather-ptt-proto
//! Первый запуск: KDE покажет окно подтверждения сочетания клавиш.

use std::time::{Duration, Instant};

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::CreateSessionOptions;
use futures_util::StreamExt;

const APP_ID: &str = "app.feather.Feather";
const PTT_ID: &str = "push-to-talk";
// Формат по спецификации XDG shortcuts; пользователь может переназначить в диалоге
const PREFERRED: &str = "CTRL+ALT+V";

/// Текущее монотонное время в мс — в той же шкале, что timestamp портала (CLOCK_MONOTONIC)
fn monotonic_ms() -> u64 {
    let mut ts = std::mem::MaybeUninit::<[i64; 2]>::uninit();
    // SAFETY: clock_gettime пишет struct timespec { tv_sec: i64, tv_nsec: i64 } на 64-битном Linux
    unsafe extern "C" {
        fn clock_gettime(clk: i32, ts: *mut [i64; 2]) -> i32;
    }
    const CLOCK_MONOTONIC: i32 = 1;
    unsafe {
        clock_gettime(CLOCK_MONOTONIC, ts.as_mut_ptr());
        let [s, ns] = ts.assume_init();
        (s as u64) * 1000 + (ns as u64) / 1_000_000
    }
}

#[tokio::main]
async fn main() -> ashpd::Result<()> {
    // Хост-приложению (не flatpak) нужно назвать себя, иначе портал не знает, кому выдавать права
    if let Err(e) = ashpd::register_host_app(APP_ID.try_into().expect("неверный app id")).await {
        eprintln!("регистрация app id не удалась (продолжаем): {e}");
    }

    let portal = GlobalShortcuts::new().await?;
    println!("портал GlobalShortcuts, версия {}", portal.version());
    let session = portal.create_session(CreateSessionOptions::default()).await?;

    let shortcut = NewShortcut::new(PTT_ID, "Feather: говорить (push-to-talk)").preferred_trigger(PREFERRED);
    println!("прошу назначить {PREFERRED}; подтвердите в окне KDE…");
    let bound = portal
        .bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default())
        .await?
        .response()?;
    for s in bound.shortcuts() {
        println!("назначено: {} → {}", s.id(), s.trigger_description());
    }

    let mut pressed = portal.receive_activated().await?;
    let mut released = portal.receive_deactivated().await?;
    let mut down_at: Option<Instant> = None;
    println!("жду нажатий (Ctrl+C — выход)");
    loop {
        tokio::select! {
            Some(ev) = pressed.next() => {
                if ev.shortcut_id() != PTT_ID { continue; }
                let lag = monotonic_ms().saturating_sub(ev.timestamp().as_millis() as u64);
                println!("▶ говорю      (событие пришло через {lag} мс)");
                down_at = Some(Instant::now());
            }
            Some(ev) = released.next() => {
                if ev.shortcut_id() != PTT_ID { continue; }
                let lag = monotonic_ms().saturating_sub(ev.timestamp().as_millis() as u64);
                let held = down_at.take().map_or(Duration::ZERO, |t| t.elapsed());
                println!("■ замолчал    (держал {:.2} с, событие через {lag} мс)", held.as_secs_f64());
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

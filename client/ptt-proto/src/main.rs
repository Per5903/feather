//! Прототип глобального push-to-talk через XDG-портал GlobalShortcuts (Wayland, этап 0).
//!
//! На Wayland приложение не может само перехватывать клавиши в чужих окнах (в игре).
//! Правильный путь — попросить портал: рабочий стол показывает пользователю диалог
//! назначения клавиши и потом присылает события «нажата»/«отпущена» даже поверх игры.
//!
//! Запуск: cargo run -p feather-ptt-proto
//! Первый запуск: KDE покажет окно подтверждения; клавишу нужно выбрать в нём или в
//! открывшихся после настройках (KDE не назначает предложенную приложением клавишу сам).

use std::time::{Duration, Instant};

use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut};
use ashpd::desktop::CreateSessionOptions;
use futures_util::StreamExt;

const APP_ID: &str = "app.feather.Feather";
const PTT_ID: &str = "push-to-talk";
// Формат по спецификации XDG shortcuts; пользователь может переназначить в диалоге
const PREFERRED: &str = "CTRL+ALT+V";

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
    let mut unassigned = false;
    for s in bound.shortcuts() {
        let trigger = s.trigger_description();
        println!("назначено: {} → {}", s.id(), if trigger.is_empty() { "клавиша не выбрана" } else { trigger });
        unassigned |= trigger.is_empty();
    }
    // KDE не применяет предложенную клавишу сам: её выбирает пользователь.
    // Если клавиши нет — открываем системное окно настройки (портал v2).
    if unassigned && portal.version() >= 2 {
        println!("открываю окно настройки клавиши: назначьте её и закройте окно");
        portal.configure_shortcuts(&session, None, ConfigureShortcutsOptions::default()).await?;
    }

    let mut pressed = portal.receive_activated().await?;
    let mut released = portal.receive_deactivated().await?;
    // Метка времени портала в KDE не в системной шкале (почти ноль), поэтому абсолютную
    // задержку по ней не посчитать. Сверяем только длительность удержания: по меткам KDE
    // и по нашим часам — расхождение показывает разброс доставки событий.
    let mut down_at: Option<(Instant, Duration)> = None;
    println!("жду нажатий (Ctrl+C — выход)");
    loop {
        tokio::select! {
            Some(ev) = pressed.next() => {
                if ev.shortcut_id() != PTT_ID { continue; }
                println!("▶ говорю");
                down_at = Some((Instant::now(), ev.timestamp()));
            }
            Some(ev) = released.next() => {
                if ev.shortcut_id() != PTT_ID { continue; }
                if let Some((at, ts)) = down_at.take() {
                    let ours = at.elapsed().as_millis() as i64;
                    let kde = ev.timestamp().saturating_sub(ts).as_millis() as i64;
                    println!("■ замолчал    (держал {ours} мс по нашим часам, {kde} мс по KDE, расхождение {} мс)", ours - kde);
                } else {
                    println!("■ замолчал");
                }
            }
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

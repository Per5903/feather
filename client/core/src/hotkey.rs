//! Глобальная клавиша push-to-talk (работает поверх игры).
//!
//! Linux/Wayland: XDG-портал GlobalShortcuts (D15, проверено в Dota 2). Особенности KDE:
//! предложенную клавишу он сам не назначает — открываем окно настройки; метка времени
//! событий нулевая — задержку по ней не измерить. Клавишу выбирает пользователь в KDE.
//!
//! Windows: опрос состояния клавиш (`GetAsyncKeyState`) раз в 15 мс в отдельном потоке.
//! Работает поверх полноэкранных игр, понимает боковые кнопки мыши («Mouse4»), только читает
//! состояние (ничего не перехватывает). Клавиша хранится в настройках клиента строкой
//! вида «Ctrl+Alt+V» или «Mouse4»; «назначить» = следующее нажатие.
//!
//! macOS: TODO.

#[derive(Debug, Clone)]
pub enum HotkeyEvent {
    /// Клавиша назначена (описание для интерфейса, например «Ctrl+Alt+V»), пусто — не назначена.
    /// На Windows клиент сохраняет эту строку в настройки.
    Bound(String),
    Pressed,
    Released,
    /// Глобальные клавиши недоступны (нет портала, отказ пользователя)
    Unavailable(String),
}

/// Пока жив — клавиша зарегистрирована. Уничтожение отключает push-to-talk.
pub struct HotkeyHandle {
    #[cfg(target_os = "linux")]
    configure: tokio::sync::mpsc::UnboundedSender<()>,
    #[cfg(target_os = "linux")]
    task: tokio::task::JoinHandle<()>,
    #[cfg(windows)]
    shared: std::sync::Arc<win::Flags>,
}

impl HotkeyHandle {
    /// Назначить клавишу: Linux — системное окно KDE, Windows — следующее нажатие
    pub fn configure(&self) {
        #[cfg(target_os = "linux")]
        let _ = self.configure.send(());
        #[cfg(windows)]
        self.shared.capture.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        self.task.abort();
        #[cfg(windows)]
        self.shared.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// `binding` — сохранённая клавиша (используется на Windows; на Linux клавишей управляет KDE)
#[cfg(target_os = "linux")]
pub fn register(app_id: &'static str, _binding: Option<String>, on_event: impl Fn(HotkeyEvent) + Send + Sync + 'static) -> HotkeyHandle {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        if let Err(e) = linux::run(app_id, rx, &on_event).await {
            on_event(HotkeyEvent::Unavailable(e.to_string()));
        }
    });
    HotkeyHandle { configure: tx, task }
}

#[cfg(windows)]
pub fn register(_app_id: &'static str, binding: Option<String>, on_event: impl Fn(HotkeyEvent) + Send + Sync + 'static) -> HotkeyHandle {
    let shared = std::sync::Arc::new(win::Flags::default());
    let flags = shared.clone();
    let binding = binding.unwrap_or_else(|| win::DEFAULT.to_owned());
    std::thread::Builder::new()
        .name("feather-ptt".into())
        .spawn(move || win::run(flags, binding, on_event))
        .expect("поток push-to-talk");
    HotkeyHandle { shared }
}

#[cfg(not(any(target_os = "linux", windows)))]
pub fn register(_app_id: &'static str, _binding: Option<String>, on_event: impl Fn(HotkeyEvent) + Send + Sync + 'static) -> HotkeyHandle {
    on_event(HotkeyEvent::Unavailable("глобальные клавиши на этой ОС пока не поддерживаются".into()));
    HotkeyHandle {}
}

/// Разбор и описание сочетаний — общий код (тестируется на любой ОС)
pub mod keys {
    /// Виртуальные коды клавиш Windows и их имена в настройках
    pub const NAMES: &[(i32, &str)] = &[
        (0x11, "Ctrl"),
        (0x12, "Alt"),
        (0x10, "Shift"),
        (0x04, "Mouse3"),
        (0x05, "Mouse4"),
        (0x06, "Mouse5"),
        (0x14, "CapsLock"),
        (0x20, "Space"),
        (0x09, "Tab"),
        (0xC0, "`"),
        (0x2D, "Insert"),
        (0x2E, "Delete"),
        (0x24, "Home"),
        (0x23, "End"),
        (0x21, "PageUp"),
        (0x22, "PageDown"),
    ];

    pub fn is_modifier(vk: i32) -> bool {
        matches!(vk, 0x10 | 0x11 | 0x12)
    }

    pub fn name(vk: i32) -> Option<String> {
        if let Some((_, n)) = NAMES.iter().find(|(v, _)| *v == vk) {
            return Some((*n).to_owned());
        }
        match vk {
            0x30..=0x39 | 0x41..=0x5A => Some((vk as u8 as char).to_string()),
            0x60..=0x69 => Some(format!("Num{}", vk - 0x60)),
            0x70..=0x87 => Some(format!("F{}", vk - 0x6F)),
            _ => None,
        }
    }

    pub fn code(name: &str) -> Option<i32> {
        let n = name.trim();
        if let Some((v, _)) = NAMES.iter().find(|(_, x)| x.eq_ignore_ascii_case(n)) {
            return Some(*v);
        }
        let up = n.to_ascii_uppercase();
        if up.len() == 1 {
            let c = up.as_bytes()[0];
            if c.is_ascii_alphanumeric() {
                return Some(c as i32);
            }
        }
        if let Some(d) = up.strip_prefix("NUM").and_then(|d| d.parse::<i32>().ok()).filter(|d| (0..=9).contains(d)) {
            return Some(0x60 + d);
        }
        up.strip_prefix('F').and_then(|f| f.parse::<i32>().ok()).filter(|f| (1..=24).contains(f)).map(|f| 0x6F + f)
    }

    /// «Ctrl+Alt+V» → [Ctrl, Alt, V]; непонятное сочетание — None
    pub fn parse(binding: &str) -> Option<Vec<i32>> {
        let codes: Option<Vec<i32>> = binding.split('+').filter(|s| !s.trim().is_empty()).map(code).collect();
        codes.filter(|c| !c.is_empty())
    }

    pub fn describe(codes: &[i32]) -> String {
        codes.iter().filter_map(|c| name(*c)).collect::<Vec<_>>().join("+")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn roundtrip() {
            for b in ["Ctrl+Alt+V", "Mouse4", "CapsLock", "F13", "Num5", "Shift+`"] {
                assert_eq!(describe(&parse(b).unwrap()), b);
            }
            assert_eq!(parse("ctrl+alt+v"), parse("Ctrl+Alt+V"), "регистр не важен");
            assert_eq!(parse("Ctrl+Банан"), None);
        }
    }
}

#[cfg(windows)]
mod win {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    use super::{HotkeyEvent, keys};

    pub const DEFAULT: &str = "Ctrl+Alt+V";
    const POLL: Duration = Duration::from_millis(15);

    #[derive(Default)]
    pub struct Flags {
        pub stop: AtomicBool,
        pub capture: AtomicBool,
    }

    fn down(vk: i32) -> bool {
        // SAFETY: просто чтение состояния клавиши, без побочных эффектов
        (unsafe { GetAsyncKeyState(vk) } as u16) & 0x8000 != 0
    }

    /// Все коды, которые умеем назвать (кроме левой и правой кнопок мыши — ими кликают)
    fn all_codes() -> impl Iterator<Item = i32> {
        (0x03..=0xFE).filter(|vk| keys::name(*vk).is_some())
    }

    pub fn run(flags: Arc<Flags>, binding: String, on_event: impl Fn(HotkeyEvent)) {
        let mut combo = keys::parse(&binding).or_else(|| keys::parse(DEFAULT)).unwrap_or_default();
        on_event(HotkeyEvent::Bound(keys::describe(&combo)));
        let mut pressed = false;
        while !flags.stop.load(Ordering::Relaxed) {
            if flags.capture.swap(false, Ordering::Relaxed) {
                if pressed {
                    pressed = false;
                    on_event(HotkeyEvent::Released);
                }
                if let Some(c) = capture(&flags) {
                    combo = c;
                    on_event(HotkeyEvent::Bound(keys::describe(&combo)));
                }
                continue;
            }
            let now = !combo.is_empty() && combo.iter().all(|vk| down(*vk));
            if now != pressed {
                pressed = now;
                on_event(if now { HotkeyEvent::Pressed } else { HotkeyEvent::Released });
            }
            std::thread::sleep(POLL);
        }
    }

    /// Ждём следующее нажатие (до 10 с): модификаторы, зажатые в этот момент, входят в сочетание
    fn capture(flags: &Flags) -> Option<Vec<i32>> {
        // Сначала дождаться, пока отпустят всё (например, кнопку мыши, которой нажали «Назначить»)
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while all_codes().any(down) {
            if std::time::Instant::now() > deadline || flags.stop.load(Ordering::Relaxed) {
                return None;
            }
            std::thread::sleep(POLL);
        }
        while std::time::Instant::now() < deadline && !flags.stop.load(Ordering::Relaxed) {
            if let Some(key) = all_codes().filter(|vk| !keys::is_modifier(*vk)).find(|vk| down(*vk)) {
                let mut combo: Vec<i32> = [0x11, 0x12, 0x10].into_iter().filter(|m| down(*m)).collect();
                combo.push(key);
                return Some(combo);
            }
            std::thread::sleep(POLL);
        }
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use ashpd::desktop::CreateSessionOptions;
    use ashpd::desktop::global_shortcuts::{BindShortcutsOptions, ConfigureShortcutsOptions, GlobalShortcuts, NewShortcut};
    use futures_util::StreamExt;

    use super::HotkeyEvent;

    const PTT_ID: &str = "push-to-talk";
    const PREFERRED: &str = "CTRL+ALT+V";

    pub async fn run(
        app_id: &str,
        mut configure: tokio::sync::mpsc::UnboundedReceiver<()>,
        on_event: &(impl Fn(HotkeyEvent) + Send + Sync),
    ) -> ashpd::Result<()> {
        // Хост-приложению нужен app id (и .desktop-файл с ним), иначе портал не выдаст права
        if let Ok(id) = app_id.try_into() {
            let _ = ashpd::register_host_app(id).await;
        }
        let portal = GlobalShortcuts::new().await?;
        let session = portal.create_session(CreateSessionOptions::default()).await?;
        let shortcut = NewShortcut::new(PTT_ID, "Feather: говорить (push-to-talk)").preferred_trigger(PREFERRED);
        let bound = portal.bind_shortcuts(&session, &[shortcut], None, BindShortcutsOptions::default()).await?.response()?;
        let trigger = bound.shortcuts().iter().find(|s| s.id() == PTT_ID).map(|s| s.trigger_description().to_owned()).unwrap_or_default();
        on_event(HotkeyEvent::Bound(trigger.clone()));
        if trigger.is_empty() && portal.version() >= 2 {
            portal.configure_shortcuts(&session, None, ConfigureShortcutsOptions::default()).await?;
        }

        let mut pressed = portal.receive_activated().await?;
        let mut released = portal.receive_deactivated().await?;
        let mut changed = portal.receive_shortcuts_changed().await?;
        loop {
            tokio::select! {
                Some(ev) = pressed.next() => if ev.shortcut_id() == PTT_ID { on_event(HotkeyEvent::Pressed) },
                Some(ev) = released.next() => if ev.shortcut_id() == PTT_ID { on_event(HotkeyEvent::Released) },
                Some(ev) = changed.next() => {
                    if let Some(s) = ev.shortcuts().iter().find(|s| s.id() == PTT_ID) {
                        on_event(HotkeyEvent::Bound(s.trigger_description().to_owned()));
                    }
                }
                Some(()) = configure.recv() => {
                    if portal.version() >= 2 {
                        portal.configure_shortcuts(&session, None, ConfigureShortcutsOptions::default()).await?;
                    }
                }
                else => return Ok(()),
            }
        }
    }
}

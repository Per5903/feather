//! Глобальная клавиша push-to-talk (работает поверх игры).
//!
//! Linux/Wayland: XDG-портал GlobalShortcuts (D15, проверено в Dota 2). Особенности KDE:
//! предложенную клавишу он сам не назначает — открываем окно настройки; метка времени
//! событий нулевая — задержку по ней не измерить.
//! Windows/macOS: TODO (RegisterHotKey / CGEventTap).

#[derive(Debug, Clone)]
pub enum HotkeyEvent {
    /// Клавиша назначена (описание для интерфейса, например «Ctrl+Alt+V»), пусто — не назначена
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
    task: tokio::task::JoinHandle<()>,
}

impl HotkeyHandle {
    /// Открыть системное окно назначения клавиши
    pub fn configure(&self) {
        #[cfg(target_os = "linux")]
        let _ = self.configure.send(());
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(target_os = "linux")]
pub fn register(app_id: &'static str, on_event: impl Fn(HotkeyEvent) + Send + Sync + 'static) -> HotkeyHandle {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        if let Err(e) = linux::run(app_id, rx, &on_event).await {
            on_event(HotkeyEvent::Unavailable(e.to_string()));
        }
    });
    HotkeyHandle { configure: tx, task }
}

#[cfg(not(target_os = "linux"))]
pub fn register(_app_id: &'static str, on_event: impl Fn(HotkeyEvent) + Send + 'static) -> HotkeyHandle {
    on_event(HotkeyEvent::Unavailable("глобальные клавиши на этой ОС пока не поддерживаются".into()));
    HotkeyHandle { task: tokio::spawn(async {}) }
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

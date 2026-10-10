//! Системные уведомления (личные сообщения, упоминания).
//! Linux: XDG-портал Notification (тот же механизм, что у глобальных клавиш).
//! Windows/macOS: TODO (WinRT Toast / UNUserNotificationCenter).

use tokio::sync::mpsc;

pub struct Notifier {
    tx: mpsc::UnboundedSender<(String, String, String)>,
}

impl Notifier {
    /// Запускает отправку уведомлений в текущем tokio-рантайме
    pub fn spawn(app_id: &'static str) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(run(app_id, rx));
        Self { tx }
    }

    /// `key` — повторное уведомление с тем же ключом заменяет прежнее (например, один канал)
    pub fn show(&self, key: &str, title: &str, body: &str) {
        let body: String = body.chars().take(200).collect();
        let _ = self.tx.send((key.to_owned(), title.to_owned(), body));
    }
}

#[cfg(target_os = "linux")]
async fn run(app_id: &'static str, mut rx: mpsc::UnboundedReceiver<(String, String, String)>) {
    use ashpd::desktop::notification::{Notification, NotificationProxy, Priority};
    if let Ok(id) = app_id.try_into() {
        let _ = ashpd::register_host_app(id).await;
    }
    let Ok(proxy) = NotificationProxy::new().await else { return };
    while let Some((key, title, body)) = rx.recv().await {
        let n = Notification::new(&title).body(body.as_str()).priority(Priority::Normal);
        let _ = proxy.add_notification(&key, n).await;
    }
}

#[cfg(not(target_os = "linux"))]
async fn run(_app_id: &'static str, mut rx: mpsc::UnboundedReceiver<(String, String, String)>) {
    while rx.recv().await.is_some() {}
}

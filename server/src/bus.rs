//! Шина событий внутри процесса: REST-обработчики публикуют, соединения шлюза читают.
//! Для одного сервера Redis не нужен (docs/03-architecture.md). В кластере её заменит NATS.

use std::sync::Arc;

use feather_protocol::{Event, Snowflake};
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Всем участникам сервера
    Guild(Snowflake),
    /// Всем сессиям одного пользователя
    User(Snowflake),
}

pub type Envelope = Arc<(Target, Event)>;

pub struct Bus {
    tx: broadcast::Sender<Envelope>,
}

impl Bus {
    pub fn new() -> Self {
        Self { tx: broadcast::channel(4096).0 }
    }

    pub fn publish(&self, target: Target, event: Event) {
        // Ошибка означает только «сейчас никто не слушает» — это нормально
        let _ = self.tx.send(Arc::new((target, event)));
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.tx.subscribe()
    }
}

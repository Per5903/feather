//! Кто в каком голосовом канале. Хранится в памяти процесса: это живое состояние,
//! оно не переживает перезапуск и не должно (после перезапуска клиенты заходят заново).
//! Состояние принадлежит сессии шлюза: закрылась сессия — человек вышел из канала.

use std::collections::HashMap;
use std::sync::Mutex;

use feather_protocol::{Snowflake, User, VoiceState};

struct Entry {
    state: VoiceState,
    session: String,
}

#[derive(Default)]
pub struct VoiceStates {
    by_user: Mutex<HashMap<Snowflake, Entry>>,
}

impl VoiceStates {
    /// Записать состояние от сессии; возвращает событие «вышел» из прежнего сервера, если человек
    /// перешёл в канал другого сервера (участники того сервера должны увидеть уход)
    pub fn set(&self, session: &str, state: VoiceState) -> Option<VoiceState> {
        let mut map = self.by_user.lock().unwrap();
        let left_other = map
            .get(&state.user.id)
            .filter(|old| old.state.guild_id != state.guild_id && old.state.channel_id.is_some())
            .map(|old| VoiceState { channel_id: None, ..old.state.clone() });
        if state.channel_id.is_some() {
            map.insert(state.user.id, Entry { state, session: session.to_owned() });
        } else {
            map.remove(&state.user.id);
        }
        left_other
    }

    /// Сессия закрылась: если человек был в голосе именно через неё — убрать
    pub fn session_closed(&self, session: &str, user: Snowflake) -> Option<VoiceState> {
        let mut map = self.by_user.lock().unwrap();
        if map.get(&user).is_some_and(|e| e.session == session) {
            map.remove(&user).map(|e| VoiceState { channel_id: None, ..e.state })
        } else {
            None
        }
    }

    pub fn current_guild(&self, user: Snowflake) -> Option<Snowflake> {
        self.by_user.lock().unwrap().get(&user).map(|e| e.state.guild_id)
    }

    pub fn for_guilds(&self, guilds: &[Snowflake]) -> Vec<VoiceState> {
        self.by_user
            .lock()
            .unwrap()
            .values()
            .filter(|e| guilds.contains(&e.state.guild_id))
            .map(|e| e.state.clone())
            .collect()
    }

    /// Для теста и отладки
    #[allow(dead_code)]
    pub fn user(&self, user: &User) -> Option<VoiceState> {
        self.by_user.lock().unwrap().get(&user.id).map(|e| e.state.clone())
    }
}

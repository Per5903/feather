//! Realtime-шлюз (WebSocket), docs/05-protocol.md.
//!
//! HELLO → клиент шлёт IDENTIFY → READY → DISPATCH событий. Полные события приходят
//! только по серверам, на которые клиент подписан (SUBSCRIBE_GUILD); по остальным —
//! только UNREAD_UPDATE. Формат: MessagePack по умолчанию, `?encoding=json` для отладки.

use std::collections::HashSet;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use feather_protocol::{ChannelKind, ClientOp, Event, ServerOp, Snowflake, User, VoiceState};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, interval_at, timeout};

use crate::Shared;
use crate::auth::user_by_token;
use crate::bus::{Envelope, Target};
use crate::guilds::{channel_for_member, user_guilds};
use crate::messages::unread_channels;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const IDENTIFY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Encoding {
    #[default]
    Msgpack,
    Json,
}

#[derive(Deserialize)]
pub struct GatewayQuery {
    #[serde(default)]
    encoding: Encoding,
}

pub async fn upgrade(State(st): State<Shared>, Query(q): Query<GatewayQuery>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        let mut who = None;
        if let Err(e) = Session::run(st.clone(), socket, q.encoding, &mut who).await {
            tracing::debug!("шлюз: соединение закрыто: {e}");
        }
        // Сессия закрылась (выход, обрыв, падение клиента): убираем человека из голосового канала
        if let Some((session_id, user)) = who
            && let Some(left) = st.voice.session_closed(&session_id, user)
        {
            st.bus.publish(Target::Guild(left.guild_id), Event::VoiceStateUpdate(left));
        }
    })
}

/// Голосовое состояние от клиента: проверяем канал и рассылаем участникам сервера
async fn voice_state(st: &Shared, session_id: &str, user: &User, channel_id: Option<Snowflake>, muted: bool, deafened: bool) -> Result<(), String> {
    let guild_id = match channel_id {
        Some(cid) => {
            let (channel, _) = channel_for_member(&st.db, cid, user.id).await.map_err(|e| format!("{e:?}"))?;
            if channel.kind != ChannelKind::Voice {
                return Ok(()); // не голосовой канал — молча игнорируем
            }
            channel.guild_id
        }
        // Выход: сервер, в котором человек был
        None => match st.voice.current_guild(user.id) {
            Some(g) => g,
            None => return Ok(()),
        },
    };
    let state = VoiceState { guild_id, channel_id, user: user.clone(), muted, deafened };
    if let Some(left) = st.voice.set(session_id, state.clone()) {
        st.bus.publish(Target::Guild(left.guild_id), Event::VoiceStateUpdate(left));
    }
    st.bus.publish(Target::Guild(guild_id), Event::VoiceStateUpdate(state));
    Ok(())
}

struct Session {
    ws: WebSocket,
    enc: Encoding,
    seq: u64,
}

type SessionResult = Result<(), String>;

impl Session {
    async fn send(&mut self, op: &ServerOp) -> SessionResult {
        let msg = match self.enc {
            Encoding::Msgpack => WsMessage::Binary(rmp_serde::to_vec_named(op).map_err(|e| e.to_string())?.into()),
            Encoding::Json => WsMessage::Text(serde_json::to_string(op).map_err(|e| e.to_string())?.into()),
        };
        self.ws.send(msg).await.map_err(|e| e.to_string())
    }

    async fn dispatch(&mut self, event: Event) -> SessionResult {
        self.seq += 1;
        let seq = self.seq;
        self.send(&ServerOp::Dispatch { seq, event }).await
    }

    fn decode(&self, msg: WsMessage) -> Option<Result<ClientOp, String>> {
        let op = match msg {
            WsMessage::Binary(b) => rmp_serde::from_slice(&b).map_err(|e| e.to_string()),
            WsMessage::Text(t) => serde_json::from_str(&t).map_err(|e| e.to_string()),
            WsMessage::Close(_) => return Some(Err("клиент закрыл соединение".into())),
            _ => return None, // ping/pong обрабатывает axum
        };
        match op {
            Ok(op) => Some(Ok(op)),
            // Неизвестная команда (клиент новее сервера) — пропускаем, а не рвём соединение:
            // иначе клиент переподключается и присылает её снова, бесконечно
            Err(e) => {
                tracing::debug!("шлюз: пропущена непонятная команда: {e}");
                None
            }
        }
    }

    async fn recv(&mut self) -> Option<Result<ClientOp, String>> {
        loop {
            match self.ws.recv().await? {
                Ok(msg) => match self.decode(msg) {
                    Some(op) => return Some(op),
                    None => continue,
                },
                Err(e) => return Some(Err(e.to_string())),
            }
        }
    }

    async fn run(st: Shared, ws: WebSocket, enc: Encoding, who: &mut Option<(String, Snowflake)>) -> SessionResult {
        let mut s = Session { ws, enc, seq: 0 };
        // Подписываемся на шину до READY, чтобы не потерять события между ними
        let mut bus = st.bus.subscribe();
        s.send(&ServerOp::Hello { heartbeat_interval_ms: HEARTBEAT_INTERVAL.as_millis() as u64 }).await?;

        let token = match timeout(IDENTIFY_TIMEOUT, s.recv()).await {
            Ok(Some(Ok(ClientOp::Identify { token }))) => token,
            _ => {
                s.send(&ServerOp::InvalidSession { reason: "ожидался IDENTIFY".into() }).await.ok();
                return Err("нет IDENTIFY".into());
            }
        };
        let Some(user) = user_by_token(&st.db, &token).await.map_err(|e| format!("{e:?}"))? else {
            s.send(&ServerOp::InvalidSession { reason: "неверный токен".into() }).await.ok();
            return Err("неверный токен".into());
        };
        let guilds = user_guilds(&st.db, user.id).await.map_err(|e| format!("{e:?}"))?;
        let unread = unread_channels(&st.db, user.id).await.map_err(|e| format!("{e:?}"))?;
        let mut member_of: HashSet<Snowflake> = guilds.iter().map(|g| g.id).collect();
        let mut subscribed: HashSet<Snowflake> = HashSet::new();
        let mut background = false;
        let me = user.id;

        let mut session_id = [0u8; 12];
        getrandom::fill(&mut session_id).map_err(|e| e.to_string())?;
        let session_id: String = session_id.iter().map(|b| format!("{b:02x}")).collect();
        *who = Some((session_id.clone(), user.id));
        let guild_ids: Vec<Snowflake> = guilds.iter().map(|g| g.id).collect();
        let voice_states = st.voice.for_guilds(&guild_ids);
        let me_user = user.clone();
        s.send(&ServerOp::Ready { session_id: session_id.clone(), user, guilds, unread, voice_states }).await?;
        tracing::debug!("шлюз: пользователь {me} подключён");

        // Клиент шлёт HEARTBEAT раз в интервал; два пропуска подряд — соединение мёртвое
        let mut last_beat = Instant::now();
        let mut watchdog = interval_at(Instant::now() + HEARTBEAT_INTERVAL, HEARTBEAT_INTERVAL);

        loop {
            tokio::select! {
                op = s.recv() => match op {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(op)) => match op {
                        ClientOp::Heartbeat { .. } => {
                            last_beat = Instant::now();
                            s.send(&ServerOp::HeartbeatAck).await?;
                        }
                        ClientOp::SubscribeGuild { guild_id } if member_of.contains(&guild_id) => {
                            subscribed.insert(guild_id);
                        }
                        ClientOp::SubscribeGuild { .. } => {} // чужой сервер — молча игнорируем
                        ClientOp::UnsubscribeGuild { guild_id } => {
                            subscribed.remove(&guild_id);
                        }
                        ClientOp::ClientState { background: b } => background = b,
                        ClientOp::VoiceState { channel_id, muted, deafened } => {
                            voice_state(&st, &session_id, &me_user, channel_id, muted, deafened).await?;
                        }
                        ClientOp::Identify { .. } => return Err("повторный IDENTIFY".into()),
                    },
                },
                env = bus.recv() => match env {
                    Ok(env) => {
                        if let Some(event) = route(&env, me, &mut member_of, &subscribed, background) {
                            s.dispatch(event).await?;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Клиент не успевает читать. TODO(этап 1): RESUME с буфером событий
                        tracing::warn!("шлюз: пользователь {me} пропустил {n} событий");
                    }
                    Err(RecvError::Closed) => return Ok(()),
                },
                _ = watchdog.tick() => {
                    if last_beat.elapsed() > HEARTBEAT_INTERVAL * 2 {
                        return Err("нет HEARTBEAT".into());
                    }
                }
            }
        }
    }
}

/// Что из события шины получит этот клиент (если вообще что-то)
fn route(
    env: &Envelope,
    me: Snowflake,
    member_of: &mut HashSet<Snowflake>,
    subscribed: &HashSet<Snowflake>,
    _background: bool,
) -> Option<Event> {
    let (target, event) = env.as_ref();
    match *target {
        Target::User(uid) if uid == me => {
            if let Event::GuildCreate(g) = event {
                member_of.insert(g.id);
            }
            Some(event.clone())
        }
        Target::User(_) => None,
        Target::Guild(gid) if !member_of.contains(&gid) => None,
        Target::Guild(_) if !event.is_heavy() => Some(event.clone()),
        Target::Guild(gid) if subscribed.contains(&gid) => Some(event.clone()),
        // Сервер не открыт: только счётчик непрочитанного
        Target::Guild(gid) => match event {
            Event::MessageCreate(m) => Some(Event::UnreadUpdate {
                guild_id: gid,
                channel_id: m.channel_id,
                last_message_id: m.id,
                author_id: m.author.id,
            }),
            _ => None,
        },
    }
}

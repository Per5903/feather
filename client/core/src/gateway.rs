//! Подключение к realtime-шлюзу с автоматическим переподключением.

use std::time::Duration;

use feather_protocol::{ClientOp, Event, Guild, ServerOp, UnreadChannel, User, VoiceState};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;

#[derive(Debug)]
pub enum GatewayEvent {
    /// Подключились (или переподключились): клиенту нужно заново подписаться на открытый сервер
    Ready { user: User, guilds: Vec<Guild>, unread: Vec<UnreadChannel>, voice_states: Vec<VoiceState>, admin: bool },
    Event(Event),
    /// Связь потеряна, идёт переподключение
    Reconnecting(String),
    /// Токен недействителен — переподключаться бессмысленно, нужен повторный вход
    SessionInvalid(String),
}

/// Отправка команд в шлюз. Когда все копии уничтожены, соединение закрывается.
#[derive(Clone)]
pub struct GatewayHandle(mpsc::UnboundedSender<ClientOp>);

impl GatewayHandle {
    pub fn send(&self, op: ClientOp) {
        let _ = self.0.send(op);
    }
}

/// Запускает шлюз в текущем tokio-рантайме. `on_event` вызывается из фоновой задачи.
pub fn spawn(url: String, token: String, mut on_event: impl FnMut(GatewayEvent) + Send + 'static) -> GatewayHandle {
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match session(&url, &token, &mut rx, &mut on_event).await {
                End::Closed => return,
                End::Invalid(reason) => {
                    on_event(GatewayEvent::SessionInvalid(reason));
                    return;
                }
                End::Lost(reason, was_ready) => {
                    if was_ready {
                        backoff = Duration::from_secs(1);
                    }
                    on_event(GatewayEvent::Reconnecting(reason));
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    });
    GatewayHandle(tx)
}

enum End {
    /// Клиент сам закрыл (выход)
    Closed,
    Invalid(String),
    /// Обрыв; флаг — успели ли получить READY (тогда сбрасываем паузу переподключения)
    Lost(String, bool),
}

async fn session(
    url: &str,
    token: &str,
    rx: &mut mpsc::UnboundedReceiver<ClientOp>,
    on_event: &mut (impl FnMut(GatewayEvent) + Send),
) -> End {
    let lost = |e: &dyn std::fmt::Display, ready| End::Lost(e.to_string(), ready);
    let (mut ws, _) = match tokio_tungstenite::connect_async(url).await {
        Ok(c) => c,
        Err(e) => return lost(&e, false),
    };
    let encode = |op: &ClientOp| Ws::Binary(rmp_serde::to_vec_named(op).expect("сериализация").into());
    if let Err(e) = ws.send(encode(&ClientOp::Identify { token: token.into() })).await {
        return lost(&e, false);
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await;
    let mut ready = false;
    let mut seq = 0u64;
    loop {
        tokio::select! {
            msg = ws.next() => {
                let bytes = match msg {
                    Some(Ok(Ws::Binary(b))) => b,
                    Some(Ok(Ws::Close(_))) | None => return lost(&"сервер закрыл соединение", ready),
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => return lost(&e, ready),
                };
                match rmp_serde::from_slice::<ServerOp>(&bytes) {
                    Ok(ServerOp::Hello { heartbeat_interval_ms }) => {
                        heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_interval_ms));
                        heartbeat.tick().await;
                    }
                    Ok(ServerOp::Ready { user, guilds, unread, voice_states, admin, .. }) => {
                        ready = true;
                        on_event(GatewayEvent::Ready { user, guilds, unread, voice_states, admin });
                    }
                    Ok(ServerOp::Dispatch { seq: s, event }) => {
                        seq = s;
                        on_event(GatewayEvent::Event(event));
                    }
                    Ok(ServerOp::HeartbeatAck) => {}
                    Ok(ServerOp::InvalidSession { reason }) => return End::Invalid(reason),
                    // Незнакомое событие (сервер новее клиента) — пропускаем, а не рвём соединение:
                    // иначе после каждого обновления сервера старые клиенты переподключались бы по кругу
                    Err(e) => {
                        if std::env::var_os("FEATHER_LOG").is_some() {
                            eprintln!("шлюз: пропущено непонятное сообщение: {e}");
                        }
                    }
                }
            }
            op = rx.recv() => match op {
                Some(op) => if let Err(e) = ws.send(encode(&op)).await { return lost(&e, ready) },
                None => {
                    let _ = ws.close(None).await;
                    return End::Closed;
                }
            },
            _ = heartbeat.tick() => {
                if let Err(e) = ws.send(encode(&ClientOp::Heartbeat { seq })).await {
                    return lost(&e, ready);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feather_protocol::Snowflake;

    /// Событие, которого этот клиент не знает (как будто сервер новее)
    #[derive(serde::Serialize)]
    struct FutureDispatch {
        op: &'static str,
        d: FutureEvent,
    }
    #[derive(serde::Serialize)]
    struct FutureEvent {
        seq: u64,
        event: FutureBody,
    }
    #[derive(serde::Serialize)]
    struct FutureBody {
        t: &'static str,
        data: u32,
    }

    #[tokio::test]
    async fn unknown_events_do_not_drop_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Поддельный сервер: HELLO, READY, незнакомое событие, затем обычное
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let send = |op: &ServerOp| Ws::Binary(rmp_serde::to_vec_named(op).unwrap().into());
            ws.send(send(&ServerOp::Hello { heartbeat_interval_ms: 30_000 })).await.unwrap();
            let _identify = ws.next().await;
            let me = User { id: Snowflake(1), username: "me".into(), display_name: None, avatar: None };
            ws.send(send(&ServerOp::Ready { session_id: "s".into(), user: me, guilds: vec![], unread: vec![], voice_states: vec![], admin: false }))
                .await
                .unwrap();
            let future = FutureDispatch { op: "DISPATCH", d: FutureEvent { seq: 1, event: FutureBody { t: "SOMETHING_NEW", data: 7 } } };
            ws.send(Ws::Binary(rmp_serde::to_vec_named(&future).unwrap().into())).await.unwrap();
            let known = ServerOp::Dispatch { seq: 2, event: Event::ChannelAck { channel_id: Snowflake(5), message_id: Snowflake(6) } };
            ws.send(send(&known)).await.unwrap();
            // Держим соединение, пока клиент читает
            let _ = ws.next().await;
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let _handle = spawn(format!("ws://{addr}"), "token".into(), move |ev| {
            let _ = tx.send(ev);
        });
        let mut got_ack = false;
        for _ in 0..3 {
            match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await.expect("шлюз молчит").expect("закрыт") {
                GatewayEvent::Ready { .. } => {}
                GatewayEvent::Event(Event::ChannelAck { channel_id, .. }) => {
                    assert_eq!(channel_id, Snowflake(5));
                    got_ack = true;
                    break;
                }
                GatewayEvent::Reconnecting(why) => panic!("соединение разорвано из-за незнакомого события: {why}"),
                other => panic!("неожиданно: {other:?}"),
            }
        }
        assert!(got_ack, "известное событие после незнакомого не дошло");
    }
}

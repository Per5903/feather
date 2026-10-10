//! Сквозной тест: настоящий сервер, настоящий Postgres (из deploy/docker-compose.yml),
//! HTTP-клиент и WebSocket-шлюз. Каждый запуск создаёт временную базу и удаляет её.
//!
//! Запуск: docker compose -f deploy/docker-compose.yml up -d && cargo test -p feather-server

use std::net::SocketAddr;
use std::time::Duration;

use feather_protocol::*;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::Executor;
use tokio_tungstenite::tungstenite::Message as Ws;

const ADMIN_URL: &str = "postgres://feather:feather_dev_password@127.0.0.1:5432/feather";

struct TestServer {
    addr: SocketAddr,
    db_name: String,
    http: reqwest::Client,
}

impl TestServer {
    async fn start() -> Self {
        let admin_url = std::env::var("FEATHER_TEST_ADMIN_URL").unwrap_or_else(|_| ADMIN_URL.into());
        let db_name = format!(
            "feather_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos()
        );
        let admin = sqlx::PgPool::connect(&admin_url).await.expect("Postgres не запущен? docker compose up -d");
        admin.execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db_name}"))).await.unwrap();
        let mut cfg = feather_server::Config::from_env();
        cfg.data_dir = std::env::temp_dir().join(&db_name);
        cfg.database_url = format!("{}/{db_name}", admin_url.rsplit_once('/').unwrap().0);
        let app = feather_server::build(cfg).await.expect("сервер не собрался");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap()
        });
        Self { addr, db_name, http: reqwest::Client::new() }
    }

    async fn drop_db(self) {
        let admin_url = std::env::var("FEATHER_TEST_ADMIN_URL").unwrap_or_else(|_| ADMIN_URL.into());
        let admin = sqlx::PgPool::connect(&admin_url).await.unwrap();
        admin
            .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.db_name)))
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(&self.db_name));
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}/api/v1{path}", self.addr)
    }

    async fn req<B: Serialize, R: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        token: Option<&str>,
        body: Option<&B>,
    ) -> (StatusCode, Option<R>) {
        let mut rb = self.http.request(method, self.url(path));
        if let Some(t) = token {
            rb = rb.bearer_auth(t);
        }
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let resp = rb.send().await.unwrap();
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap();
        (status, serde_json::from_slice(&bytes).ok())
    }

    async fn post<B: Serialize, R: DeserializeOwned>(&self, path: &str, token: Option<&str>, body: &B) -> (StatusCode, Option<R>) {
        self.req(reqwest::Method::POST, path, token, Some(body)).await
    }

    async fn get<R: DeserializeOwned>(&self, path: &str, token: &str) -> (StatusCode, Option<R>) {
        self.req::<(), R>(reqwest::Method::GET, path, Some(token), None).await
    }

    async fn signup(&self, name: &str) -> LoginResponse {
        let creds = RegisterRequest { username: name.into(), password: "correct horse battery".into() };
        let (st, _) = self.post::<_, User>("/auth/register", None, &creds).await;
        assert_eq!(st, StatusCode::CREATED, "регистрация {name}");
        let (st, login) = self
            .post("/auth/login", None, &LoginRequest { username: name.into(), password: creds.password, device_name: None })
            .await;
        assert_eq!(st, StatusCode::OK);
        login.unwrap()
    }
}

/// Клиент шлюза на MessagePack — тот же формат, что у настоящего клиента
struct GatewayClient {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
}

impl GatewayClient {
    async fn connect(addr: SocketAddr, token: &str) -> (Self, ServerOp) {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/gateway")).await.unwrap();
        let mut c = Self { ws };
        assert!(matches!(c.next().await, ServerOp::Hello { .. }));
        c.send(&ClientOp::Identify { token: token.into() }).await;
        let ready = c.next().await;
        (c, ready)
    }

    async fn send(&mut self, op: &ClientOp) {
        self.ws.send(Ws::Binary(rmp_serde::to_vec_named(op).unwrap().into())).await.unwrap();
    }

    async fn next(&mut self) -> ServerOp {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(3), self.ws.next())
                .await
                .expect("шлюз молчит дольше 3 с")
                .expect("шлюз закрылся")
                .unwrap();
            if let Ws::Binary(b) = msg {
                return rmp_serde::from_slice(&b).unwrap();
            }
        }
    }

    async fn next_event(&mut self) -> Event {
        loop {
            match self.next().await {
                ServerOp::Dispatch { event, .. } => return event,
                ServerOp::HeartbeatAck => continue,
                other => panic!("ожидалось событие, пришло {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn full_flow() {
    let srv = TestServer::start().await;

    // --- регистрация и вход
    let alice = srv.signup("Alice").await;
    let bob = srv.signup("bob").await;
    let dup = RegisterRequest { username: "alice".into(), password: "whatever123".into() };
    let (st, err) = srv.post::<_, ApiError>("/auth/register", None, &dup).await;
    assert_eq!(st, StatusCode::CONFLICT, "имя занято без учёта регистра");
    assert_eq!(err.unwrap().code, "username_taken");
    let weak = RegisterRequest { username: "carol".into(), password: "short".into() };
    assert_eq!(srv.post::<_, ApiError>("/auth/register", None, &weak).await.0, StatusCode::BAD_REQUEST);
    let wrong = LoginRequest { username: "alice".into(), password: "nope-nope".into(), device_name: None };
    assert_eq!(srv.post::<_, ApiError>("/auth/login", None, &wrong).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(srv.get::<User>("/users/@me", "garbage").await.0, StatusCode::UNAUTHORIZED);
    let (_, me) = srv.get::<User>("/users/@me", &alice.token).await;
    assert_eq!(me.unwrap().username, "Alice");

    // --- сервер с каналами по умолчанию
    let (st, created) = srv
        .post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "Феникс".into() })
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let created = created.unwrap();
    let gid = created.guild.id;
    let text = created.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let voice = created.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    assert_eq!(created.channels.len(), 4);

    // Чужой сервер не виден
    assert_eq!(srv.get::<Guild>(&format!("/guilds/{gid}"), &bob.token).await.0, StatusCode::NOT_FOUND);
    let (st, _) = srv
        .post::<_, Message>(&format!("/channels/{text}/messages"), Some(&bob.token), &SendMessageRequest { content: "хак".into(), reply_to_id: None, nonce: None, attachment_ids: vec![] })
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "писать в чужой сервер нельзя");

    // --- инвайт: шлюз Alice должен узнать о новом участнике
    let (mut alice_gw, ready) = GatewayClient::connect(srv.addr, &alice.token).await;
    assert!(matches!(ready, ServerOp::Ready { ref guilds, .. } if guilds.len() == 1));
    alice_gw.send(&ClientOp::SubscribeGuild { guild_id: gid }).await;

    let (_, invite) = srv.post::<_, Invite>(&format!("/guilds/{gid}/invites"), Some(&alice.token), &CreateInviteRequest::default()).await;
    let invite = invite.unwrap();
    let (st, joined) = srv.post::<_, Guild>(&format!("/invites/{}", invite.code), Some(&bob.token), &()).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(joined.unwrap().id, gid);
    match alice_gw.next_event().await {
        Event::GuildMemberAdd { user, .. } => assert_eq!(user.username, "bob"),
        other => panic!("ожидался GUILD_MEMBER_ADD, пришло {other:?}"),
    }
    assert_eq!(srv.post::<_, ApiError>("/invites/nosuchcode", Some(&bob.token), &()).await.0, StatusCode::NOT_FOUND);

    // --- Bob в шлюзе, подписан на сервер: получает сообщения Alice в реальном времени
    let (mut bob_gw, ready) = GatewayClient::connect(srv.addr, &bob.token).await;
    assert!(matches!(ready, ServerOp::Ready { ref guilds, .. } if guilds[0].id == gid));
    bob_gw.send(&ClientOp::SubscribeGuild { guild_id: gid }).await;
    bob_gw.send(&ClientOp::Heartbeat { seq: 0 }).await;
    assert_eq!(bob_gw.next().await, ServerOp::HeartbeatAck); // подписка точно обработана

    let send = SendMessageRequest { content: "  го в рейд  ".into(), reply_to_id: None, nonce: Some("n-1".into()), attachment_ids: vec![] };
    let (st, sent) = srv.post::<_, Message>(&format!("/channels/{text}/messages"), Some(&alice.token), &send).await;
    assert_eq!(st, StatusCode::CREATED);
    let sent = sent.unwrap();
    assert_eq!(sent.content, "го в рейд", "пробелы по краям обрезаются");
    match bob_gw.next_event().await {
        Event::MessageCreate(m) => {
            assert_eq!(m.id, sent.id);
            assert_eq!(m.author.username, "Alice");
            assert_eq!(m.nonce.as_deref(), Some("n-1"));
        }
        other => panic!("ожидался MESSAGE_CREATE, пришло {other:?}"),
    }

    // --- без подписки приходит только счётчик непрочитанного
    bob_gw.send(&ClientOp::UnsubscribeGuild { guild_id: gid }).await;
    bob_gw.send(&ClientOp::Heartbeat { seq: 1 }).await;
    assert_eq!(bob_gw.next().await, ServerOp::HeartbeatAck);
    let (_, second) = srv
        .post::<_, Message>(&format!("/channels/{text}/messages"), Some(&alice.token), &SendMessageRequest { content: "кто на арену?".into(), reply_to_id: Some(sent.id), nonce: None, attachment_ids: vec![] })
        .await;
    let second = second.unwrap();
    match bob_gw.next_event().await {
        Event::UnreadUpdate { channel_id, last_message_id, .. } => {
            assert_eq!((channel_id, last_message_id), (text, second.id));
        }
        other => panic!("ожидался UNREAD_UPDATE, пришло {other:?}"),
    }
    bob_gw.send(&ClientOp::SubscribeGuild { guild_id: gid }).await;

    // --- история: новые сверху, пагинация через before
    let (_, hist) = srv.get::<Vec<Message>>(&format!("/channels/{text}/messages"), &bob.token).await;
    let hist = hist.unwrap();
    assert_eq!(hist.iter().map(|m| m.id).collect::<Vec<_>>(), vec![second.id, sent.id]);
    assert_eq!(hist[0].reply_to_id, Some(sent.id));
    let (_, older) = srv.get::<Vec<Message>>(&format!("/channels/{text}/messages?before={}&limit=10", second.id), &bob.token).await;
    assert_eq!(older.unwrap().len(), 1);

    // --- редактирование: только автор
    let edit = EditMessageRequest { content: "го в рейд через 5 минут".into() };
    let path = format!("/channels/{text}/messages/{}", sent.id);
    let (st, _) = srv.req::<_, ApiError>(reqwest::Method::PATCH, &path, Some(&bob.token), Some(&edit)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, edited) = srv.req::<_, Message>(reqwest::Method::PATCH, &path, Some(&alice.token), Some(&edit)).await;
    assert_eq!(st, StatusCode::OK);
    assert!(edited.unwrap().edited_at.is_some());
    assert!(matches!(bob_gw.next_event().await, Event::MessageUpdate(m) if m.content == edit.content));

    // --- удаление: автор или владелец сервера
    let (_, bobs) = srv
        .post::<_, Message>(&format!("/channels/{text}/messages"), Some(&bob.token), &SendMessageRequest { content: "спам".into(), reply_to_id: None, nonce: None, attachment_ids: vec![] })
        .await;
    let bobs = bobs.unwrap();
    assert!(matches!(bob_gw.next_event().await, Event::MessageCreate(_)));
    let (st, _) = srv.req::<(), ApiError>(reqwest::Method::DELETE, &path, Some(&bob.token), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "чужое удалять нельзя");
    let (st, _) = srv
        .req::<(), ApiError>(reqwest::Method::DELETE, &format!("/channels/{text}/messages/{}", bobs.id), Some(&alice.token), None)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT, "владелец сервера модерирует");
    assert!(matches!(bob_gw.next_event().await, Event::MessageDelete { id, .. } if id == bobs.id));

    // --- новые каналы создаёт только владелец
    let new_ch = CreateChannelRequest { name: "рейды".into(), kind: ChannelKind::Text, parent_id: None };
    assert_eq!(srv.post::<_, ApiError>(&format!("/guilds/{gid}/channels"), Some(&bob.token), &new_ch).await.0, StatusCode::FORBIDDEN);
    let (st, ch) = srv.post::<_, Channel>(&format!("/guilds/{gid}/channels"), Some(&alice.token), &new_ch).await;
    assert_eq!(st, StatusCode::CREATED);
    assert!(matches!(bob_gw.next_event().await, Event::ChannelCreate(c) if c.id == ch.as_ref().unwrap().id));

    // --- голос: токен LiveKit только для голосового канала
    assert_eq!(srv.post::<_, ApiError>(&format!("/channels/{text}/voice/join"), Some(&bob.token), &()).await.0, StatusCode::BAD_REQUEST);
    let (st, vj) = srv.post::<_, VoiceJoinResponse>(&format!("/channels/{voice}/voice/join"), Some(&bob.token), &()).await;
    assert_eq!(st, StatusCode::OK);
    let vj = vj.unwrap();
    assert_eq!(vj.room, format!("voice:{voice}"));
    assert_eq!(vj.token.split('.').count(), 3, "токен LiveKit — JWT");

    // --- выход отзывает токен сразу
    let (st, _) = srv.post::<_, ()>("/auth/logout", Some(&alice.token), &()).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(srv.get::<User>("/users/@me", &alice.token).await.0, StatusCode::UNAUTHORIZED);

    drop((alice_gw, bob_gw));
    srv.drop_db().await;
}

#[tokio::test]
async fn gateway_rejects_bad_token() {
    let srv = TestServer::start().await;
    let (mut gw, op) = GatewayClient::connect(srv.addr, "not-a-token").await;
    assert!(matches!(op, ServerOp::InvalidSession { .. }));
    // После INVALID_SESSION сервер закрывает соединение
    let closed = tokio::time::timeout(Duration::from_secs(3), gw.ws.next()).await.unwrap();
    assert!(matches!(closed, None | Some(Ok(Ws::Close(_))) | Some(Err(_))));
    srv.drop_db().await;
}

#[tokio::test]
async fn read_states() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await;
    let bob = srv.signup("bob").await;
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "R".into() }).await;
    let g = g.unwrap();
    let text = g.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let (_, inv) = srv.post::<_, Invite>(&format!("/guilds/{}/invites", g.guild.id), Some(&alice.token), &CreateInviteRequest::default()).await;
    srv.post::<_, Guild>(&format!("/invites/{}", inv.unwrap().code), Some(&bob.token), &()).await;

    let mut last = None;
    for t in ["раз", "два"] {
        let (_, m) = srv.post::<_, Message>(&format!("/channels/{text}/messages"), Some(&alice.token), &SendMessageRequest { content: t.into(), reply_to_id: None, nonce: None, attachment_ids: vec![] }).await;
        last = m.map(|m| m.id);
    }
    let last = last.unwrap();
    let unread_of = |op: ServerOp| match op {
        ServerOp::Ready { unread, .. } => unread,
        other => panic!("ожидался READY, пришло {other:?}"),
    };

    // У автора своих сообщений непрочитанного нет, у Bob — есть
    let (_, ready) = GatewayClient::connect(srv.addr, &alice.token).await;
    assert!(unread_of(ready).is_empty(), "свои сообщения не непрочитанные");
    let (mut bob_gw, ready) = GatewayClient::connect(srv.addr, &bob.token).await;
    assert_eq!(unread_of(ready), vec![UnreadChannel { guild_id: Some(g.guild.id), channel_id: text, last_message_id: last }]);

    // Bob дочитал: его же шлюз (другие устройства) получает CHANNEL_ACK
    let (st, _) = srv.post::<_, ()>(&format!("/channels/{text}/ack"), Some(&bob.token), &AckRequest { message_id: last }).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(bob_gw.next_event().await, Event::ChannelAck { channel_id: text, message_id: last });

    // Отметка не откатывается назад, а после переподключения непрочитанного нет
    let first = Snowflake(last.0 - 1);
    srv.post::<_, ()>(&format!("/channels/{text}/ack"), Some(&bob.token), &AckRequest { message_id: first }).await;
    let (_, ready) = GatewayClient::connect(srv.addr, &bob.token).await;
    assert!(unread_of(ready).is_empty());

    // Чужой канал отметить нельзя
    let carol = srv.signup("carol").await;
    let (st, _) = srv.post::<_, ApiError>(&format!("/channels/{text}/ack"), Some(&carol.token), &AckRequest { message_id: last }).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    drop(bob_gw);
    srv.drop_db().await;
}

#[tokio::test]
async fn voice_states() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await;
    let bob = srv.signup("bob").await;
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "V".into() }).await;
    let g = g.unwrap();
    let voice = g.channels.iter().find(|c| c.kind == ChannelKind::Voice).unwrap().id;
    let text = g.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let (_, inv) = srv.post::<_, Invite>(&format!("/guilds/{}/invites", g.guild.id), Some(&alice.token), &CreateInviteRequest::default()).await;
    srv.post::<_, Guild>(&format!("/invites/{}", inv.unwrap().code), Some(&bob.token), &()).await;

    // Bob видит голосовые состояния сервера, даже не открыв его (лёгкое событие)
    let (mut bob_gw, _) = GatewayClient::connect(srv.addr, &bob.token).await;
    let (mut alice_gw, _) = GatewayClient::connect(srv.addr, &alice.token).await;
    alice_gw.send(&ClientOp::VoiceState { channel_id: Some(voice), muted: false, deafened: false }).await;
    match bob_gw.next_event().await {
        Event::VoiceStateUpdate(v) => {
            assert_eq!((v.channel_id, v.user.username.as_str(), v.muted), (Some(voice), "alice", false));
        }
        other => panic!("ожидался VOICE_STATE_UPDATE, пришло {other:?}"),
    }

    // Микрофон выключен — тоже событие
    alice_gw.send(&ClientOp::VoiceState { channel_id: Some(voice), muted: true, deafened: false }).await;
    assert!(matches!(bob_gw.next_event().await, Event::VoiceStateUpdate(v) if v.muted));

    // Новый клиент получает состояние сразу в READY
    let (_, ready) = GatewayClient::connect(srv.addr, &bob.token).await;
    match ready {
        ServerOp::Ready { voice_states, .. } => {
            assert_eq!(voice_states.len(), 1);
            assert_eq!(voice_states[0].channel_id, Some(voice));
        }
        other => panic!("ожидался READY, пришло {other:?}"),
    }

    // Текстовый канал как голосовой не принимается
    alice_gw.send(&ClientOp::VoiceState { channel_id: Some(text), muted: false, deafened: false }).await;
    alice_gw.send(&ClientOp::Heartbeat { seq: 0 }).await;
    // Свои голосовые события Alice тоже получает (для других её устройств) — пропускаем их
    loop {
        match alice_gw.next().await {
            ServerOp::HeartbeatAck => break,
            ServerOp::Dispatch { event: Event::VoiceStateUpdate(v), .. } => {
                assert_ne!(v.channel_id, Some(text), "текстовый канал не голосовой");
            }
            other => panic!("неожиданно: {other:?}"),
        }
    }

    // Клиент Alice «упал»: сессия закрыта — Bob видит, что она вышла из канала
    drop(alice_gw);
    match bob_gw.next_event().await {
        Event::VoiceStateUpdate(v) => assert_eq!((v.channel_id, v.user.username.as_str()), (None, "alice")),
        other => panic!("ожидался выход из голоса, пришло {other:?}"),
    }

    drop(bob_gw);
    srv.drop_db().await;
}

#[tokio::test]
async fn gateway_ignores_unknown_ops() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await;
    let (mut gw, ready) = GatewayClient::connect(srv.addr, &alice.token).await;
    assert!(matches!(ready, ServerOp::Ready { .. }));
    // Команда из будущей версии клиента
    #[derive(Serialize)]
    struct Future {
        op: &'static str,
        d: u32,
    }
    gw.ws.send(Ws::Binary(rmp_serde::to_vec_named(&Future { op: "TELEPORT", d: 42 }).unwrap().into())).await.unwrap();
    gw.send(&ClientOp::Heartbeat { seq: 0 }).await;
    assert_eq!(gw.next().await, ServerOp::HeartbeatAck, "соединение живо после неизвестной команды");
    drop(gw);
    srv.drop_db().await;
}

#[tokio::test]
async fn rate_limits() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await; // 2 запроса авторизации из 10

    // Перебор паролей: не раньше 10-й попытки с одного IP — 429 и Retry-After.
    // Не завязываемся на скорость машины: на медленном CI ведро успевает чуть пополниться
    let wrong = LoginRequest { username: "alice".into(), password: "guess-guess".into(), device_name: None };
    let mut failed_logins = 0;
    let limited = loop {
        let resp = srv.http.post(srv.url("/auth/login")).json(&wrong).send().await.unwrap();
        if resp.status() == StatusCode::TOO_MANY_REQUESTS {
            break resp;
        }
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        failed_logins += 1;
        assert!(failed_logins < 20, "лимит так и не сработал");
    };
    assert!(failed_logins >= 8, "лимит сработал слишком рано: после {failed_logins}");
    assert!(limited.headers().get("retry-after").is_some());

    // Спам сообщениями: 5 подряд можно, шестое — 429
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "S".into() }).await;
    let text = g.unwrap().channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let msg = SendMessageRequest { content: "спам".into(), reply_to_id: None, nonce: None, attachment_ids: vec![] };
    let mut sent = 0;
    let err = loop {
        let (st, body) = srv.req::<_, serde_json::Value>(reqwest::Method::POST, &format!("/channels/{text}/messages"), Some(&alice.token), Some(&msg)).await;
        if st == StatusCode::TOO_MANY_REQUESTS {
            break body.unwrap();
        }
        assert_eq!(st, StatusCode::CREATED);
        sent += 1;
        assert!(sent < 15, "лимит сообщений так и не сработал");
    };
    assert!(sent >= 5, "лимит сработал раньше 5 сообщений: {sent}");
    assert_eq!(err["code"], "rate_limited");
    srv.drop_db().await;
}

#[tokio::test]
async fn typing() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await;
    let bob = srv.signup("bob").await;
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "T".into() }).await;
    let g = g.unwrap();
    let text = g.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let (_, inv) = srv.post::<_, Invite>(&format!("/guilds/{}/invites", g.guild.id), Some(&alice.token), &CreateInviteRequest::default()).await;
    srv.post::<_, Guild>(&format!("/invites/{}", inv.unwrap().code), Some(&bob.token), &()).await;

    let (mut bob_gw, _) = GatewayClient::connect(srv.addr, &bob.token).await;
    bob_gw.send(&ClientOp::SubscribeGuild { guild_id: g.guild.id }).await;
    let (mut alice_gw, _) = GatewayClient::connect(srv.addr, &alice.token).await;
    alice_gw.send(&ClientOp::SubscribeGuild { guild_id: g.guild.id }).await;
    bob_gw.send(&ClientOp::Heartbeat { seq: 0 }).await;
    assert_eq!(bob_gw.next().await, ServerOp::HeartbeatAck);

    alice_gw.send(&ClientOp::Typing { channel_id: text }).await;
    match bob_gw.next_event().await {
        Event::TypingStart { channel_id, user } => assert_eq!((channel_id, user.username.as_str()), (text, "alice")),
        other => panic!("ожидался TYPING_START, пришло {other:?}"),
    }
    // Себе «печатает» не приходит
    alice_gw.send(&ClientOp::Heartbeat { seq: 0 }).await;
    assert_eq!(alice_gw.next().await, ServerOp::HeartbeatAck);

    // Свёрнутому клиенту (игровой режим) — тоже нет
    bob_gw.send(&ClientOp::ClientState { background: true }).await;
    bob_gw.send(&ClientOp::Heartbeat { seq: 1 }).await;
    assert_eq!(bob_gw.next().await, ServerOp::HeartbeatAck);
    alice_gw.send(&ClientOp::Typing { channel_id: text }).await;
    bob_gw.send(&ClientOp::Heartbeat { seq: 2 }).await;
    assert_eq!(bob_gw.next().await, ServerOp::HeartbeatAck, "в игровом режиме TYPING не приходит");

    drop((alice_gw, bob_gw));
    srv.drop_db().await;
}

#[tokio::test]
async fn direct_messages() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await;
    let bob = srv.signup("bob").await;
    let carol = srv.signup("carol").await;
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "D".into() }).await;
    let g = g.unwrap();
    let (_, inv) = srv.post::<_, Invite>(&format!("/guilds/{}/invites", g.guild.id), Some(&alice.token), &CreateInviteRequest::default()).await;
    srv.post::<_, Guild>(&format!("/invites/{}", inv.unwrap().code), Some(&bob.token), &()).await;

    // Bob в шлюзе, ни на какой сервер не подписан: личные события приходят всё равно
    let (mut bob_gw, _) = GatewayClient::connect(srv.addr, &bob.token).await;

    let (st, dm) = srv.post::<_, DmChannel>("/users/@me/channels", Some(&alice.token), &CreateDmRequest { recipient_id: bob.user.id }).await;
    assert_eq!(st, StatusCode::OK);
    let dm = dm.unwrap();
    assert_eq!(dm.recipients.iter().map(|u| u.username.as_str()).collect::<Vec<_>>(), vec!["bob"]);
    match bob_gw.next_event().await {
        Event::DmCreate(d) => {
            assert_eq!(d.id, dm.id);
            assert_eq!(d.recipients[0].username, "alice", "Bob видит собеседника, а не себя");
        }
        other => panic!("ожидался DM_CREATE, пришло {other:?}"),
    }
    // Повторное открытие — тот же канал, без нового события
    let (_, again) = srv.post::<_, DmChannel>("/users/@me/channels", Some(&alice.token), &CreateDmRequest { recipient_id: bob.user.id }).await;
    assert_eq!(again.unwrap().id, dm.id);

    // Писать незнакомым (нет общего сервера) и самому себе нельзя
    let to_carol = CreateDmRequest { recipient_id: carol.user.id };
    assert_eq!(srv.post::<_, ApiError>("/users/@me/channels", Some(&alice.token), &to_carol).await.0, StatusCode::NOT_FOUND);
    let to_self = CreateDmRequest { recipient_id: alice.user.id };
    assert_eq!(srv.post::<_, ApiError>("/users/@me/channels", Some(&alice.token), &to_self).await.0, StatusCode::BAD_REQUEST);

    // Сообщение и «печатает» в личке доходят до Bob
    let (mut alice_gw, _) = GatewayClient::connect(srv.addr, &alice.token).await;
    alice_gw.send(&ClientOp::Typing { channel_id: dm.id }).await;
    assert!(matches!(bob_gw.next_event().await, Event::TypingStart { channel_id, .. } if channel_id == dm.id));
    let send = SendMessageRequest { content: "привет в личку".into(), reply_to_id: None, nonce: None, attachment_ids: vec![] };
    let (st, m) = srv.post::<_, Message>(&format!("/channels/{}/messages", dm.id), Some(&alice.token), &send).await;
    assert_eq!(st, StatusCode::CREATED);
    let m = m.unwrap();
    assert!(matches!(bob_gw.next_event().await, Event::MessageCreate(x) if x.id == m.id));

    // Посторонний не видит переписку
    assert_eq!(srv.get::<Vec<Message>>(&format!("/channels/{}/messages", dm.id), &carol.token).await.0, StatusCode::NOT_FOUND);

    // У Bob личка в непрочитанном (без сервера) и в списке диалогов
    let (_, ready) = GatewayClient::connect(srv.addr, &bob.token).await;
    match ready {
        ServerOp::Ready { unread, .. } => assert_eq!(unread, vec![UnreadChannel { guild_id: None, channel_id: dm.id, last_message_id: m.id }]),
        other => panic!("ожидался READY, пришло {other:?}"),
    }
    let (_, dms) = srv.get::<Vec<DmChannel>>("/users/@me/channels", &bob.token).await;
    let dms = dms.unwrap();
    assert_eq!((dms.len(), dms[0].last_message_id), (1, Some(m.id)));

    // В личке нет «модераторов»: Bob не может удалить сообщение Alice
    let (st, _) = srv.req::<(), ApiError>(reqwest::Method::DELETE, &format!("/channels/{}/messages/{}", dm.id, m.id), Some(&bob.token), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    drop((alice_gw, bob_gw));
    srv.drop_db().await;
}

#[tokio::test]
async fn roles_and_moderation() {
    use feather_protocol::perms::*;
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await; // владелец
    let bob = srv.signup("bob").await; // станет модератором
    let carol = srv.signup("carol").await; // обычный участник
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "R".into() }).await;
    let g = g.unwrap();
    let gid = g.guild.id;
    let text = g.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let (_, inv) = srv.post::<_, Invite>(&format!("/guilds/{gid}/invites"), Some(&alice.token), &CreateInviteRequest::default()).await;
    let code = inv.unwrap().code;
    for t in [&bob.token, &carol.token] {
        srv.post::<_, Guild>(&format!("/invites/{code}"), Some(t), &()).await;
    }

    // Обычный участник: приглашать может (@everyone), каналы создавать — нет
    let ch = CreateChannelRequest { name: "мод".into(), kind: ChannelKind::Text, parent_id: None };
    assert_eq!(srv.post::<_, ApiError>(&format!("/guilds/{gid}/channels"), Some(&bob.token), &ch).await.0, StatusCode::FORBIDDEN);
    assert_eq!(srv.post::<_, Invite>(&format!("/guilds/{gid}/invites"), Some(&bob.token), &CreateInviteRequest::default()).await.0, StatusCode::CREATED);

    // Роль «Модератор» и выдача её Bob
    let (mut carol_gw, _) = GatewayClient::connect(srv.addr, &carol.token).await;
    let mod_req = RoleRequest { name: Some("Модератор".into()), color: Some(0x3ba55c), permissions: Some(KICK_MEMBERS | MANAGE_MESSAGES | MANAGE_CHANNELS), position: None };
    let (st, role) = srv.post::<_, Role>(&format!("/guilds/{gid}/roles"), Some(&alice.token), &mod_req).await;
    assert_eq!(st, StatusCode::CREATED);
    let role = role.unwrap();
    assert!(role.position >= 1);
    let (st, _) = srv.req::<(), ()>(reqwest::Method::PUT, &format!("/guilds/{gid}/members/{}/roles/{}", bob.user.id, role.id), Some(&alice.token), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, members) = srv.get::<Vec<GuildMember>>(&format!("/guilds/{gid}/members"), &alice.token).await;
    let members = members.unwrap();
    assert_eq!(members.iter().find(|m| m.user.id == bob.user.id).unwrap().roles, vec![role.id]);

    // Модератор: создаёт каналы и удаляет чужие сообщения
    assert_eq!(srv.post::<_, Channel>(&format!("/guilds/{gid}/channels"), Some(&bob.token), &ch).await.0, StatusCode::CREATED);
    let (_, m) = srv.post::<_, Message>(&format!("/channels/{text}/messages"), Some(&carol.token), &SendMessageRequest { content: "флуд".into(), reply_to_id: None, nonce: None, attachment_ids: vec![] }).await;
    let (st, _) = srv.req::<(), ApiError>(reqwest::Method::DELETE, &format!("/channels/{text}/messages/{}", m.unwrap().id), Some(&bob.token), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    // Выдать права, которых нет самому, нельзя; управлять ролями без права — тоже
    let admin_req = RoleRequest { name: Some("админ".into()), permissions: Some(ADMINISTRATOR), ..Default::default() };
    assert_eq!(srv.post::<_, ApiError>(&format!("/guilds/{gid}/roles"), Some(&bob.token), &admin_req).await.0, StatusCode::FORBIDDEN);
    let upd = RoleRequest { permissions: Some(KICK_MEMBERS | MANAGE_MESSAGES | MANAGE_CHANNELS | MANAGE_ROLES), ..Default::default() };
    srv.req::<_, Role>(reqwest::Method::PATCH, &format!("/guilds/{gid}/roles/{}", role.id), Some(&alice.token), Some(&upd)).await;
    let (st, err) = srv.post::<_, ApiError>(&format!("/guilds/{gid}/roles"), Some(&bob.token), &admin_req).await;
    assert_eq!((st, err.unwrap().code.as_str()), (StatusCode::BAD_REQUEST, "perms_escalation"));

    // Кик: модератор выгоняет Carol, у неё сервер пропадает; владельца выгнать нельзя
    let (st, _) = srv.req::<(), ApiError>(reqwest::Method::DELETE, &format!("/guilds/{gid}/members/{}", alice.user.id), Some(&bob.token), None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = srv.req::<(), ()>(reqwest::Method::DELETE, &format!("/guilds/{gid}/members/{}", carol.user.id), Some(&bob.token), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    loop {
        match carol_gw.next_event().await {
            Event::GuildDelete { guild_id } => {
                assert_eq!(guild_id, gid);
                break;
            }
            _ => continue, // роли, участники — пропускаем
        }
    }
    assert_eq!(srv.get::<Vec<Message>>(&format!("/channels/{text}/messages"), &carol.token).await.0, StatusCode::NOT_FOUND);

    // Бан: вернуться по приглашению нельзя
    let (st, _) = srv.req::<(), ()>(reqwest::Method::PUT, &format!("/guilds/{gid}/bans/{}", carol.user.id), Some(&alice.token), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(srv.post::<_, ApiError>(&format!("/invites/{code}"), Some(&carol.token), &()).await.0, StatusCode::FORBIDDEN);

    // Без SEND_MESSAGES у @everyone писать нельзя (403), а модератору с ролью — тоже нельзя:
    // у роли «Модератор» этого права нет, а @everyone его больше не даёт
    let everyone = RoleRequest { permissions: Some(CREATE_INVITE | VOICE_CONNECT), ..Default::default() };
    srv.req::<_, Role>(reqwest::Method::PATCH, &format!("/guilds/{gid}/roles/{gid}"), Some(&alice.token), Some(&everyone)).await;
    let msg = SendMessageRequest { content: "можно?".into(), reply_to_id: None, nonce: None, attachment_ids: vec![] };
    assert_eq!(srv.post::<_, ApiError>(&format!("/channels/{text}/messages"), Some(&bob.token), &msg).await.0, StatusCode::FORBIDDEN);
    assert_eq!(srv.post::<_, Message>(&format!("/channels/{text}/messages"), Some(&alice.token), &msg).await.0, StatusCode::CREATED, "владелец может всё");

    // Выход с сервера сам: можно (кроме владельца)
    let (st, _) = srv.req::<(), ()>(reqwest::Method::DELETE, &format!("/guilds/{gid}/members/{}", bob.user.id), Some(&bob.token), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = srv.req::<(), ApiError>(reqwest::Method::DELETE, &format!("/guilds/{gid}/members/{}", alice.user.id), Some(&alice.token), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    drop(carol_gw);
    srv.drop_db().await;
}

#[tokio::test]
async fn attachments() {
    let srv = TestServer::start().await;
    let alice = srv.signup("alice").await;
    let bob = srv.signup("bob").await;
    let carol = srv.signup("carol").await;
    let (_, g) = srv.post::<_, GuildWithChannels>("/guilds", Some(&alice.token), &CreateGuildRequest { name: "A".into() }).await;
    let g = g.unwrap();
    let text = g.channels.iter().find(|c| c.kind == ChannelKind::Text).unwrap().id;
    let (_, inv) = srv.post::<_, Invite>(&format!("/guilds/{}/invites", g.guild.id), Some(&alice.token), &CreateInviteRequest::default()).await;
    srv.post::<_, Guild>(&format!("/invites/{}", inv.unwrap().code), Some(&bob.token), &()).await;

    let upload = |token: String, name: &'static str, ctype: &'static str, bytes: Vec<u8>| {
        let req = srv.http.post(srv.url(&format!("/channels/{text}/attachments?filename={name}"))).bearer_auth(token).header("content-type", ctype).body(bytes);
        async move { req.send().await.unwrap() }
    };

    // Настоящая картинка 640×480: размеры и превью
    let mut png = Vec::new();
    image::RgbImage::from_fn(640, 480, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let resp = upload(alice.token.clone(), "скрин.png", "image/png", png).await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let img: Attachment = resp.json().await.unwrap();
    assert_eq!((img.width, img.height, img.has_thumb, img.content_type.as_str()), (Some(640), Some(480), true, "image/png"));

    // Обычный файл и «картинка», которая на самом деле не картинка
    let txt: Attachment = upload(alice.token.clone(), "../../etc/passwd", "text/plain", b"hello".to_vec()).await.json().await.unwrap();
    assert_eq!((txt.filename.as_str(), txt.has_thumb), ("passwd", false), "путь из имени файла отрезается");
    let fake: Attachment = upload(alice.token.clone(), "x.png", "image/png", b"not an image".to_vec()).await.json().await.unwrap();
    assert_eq!(fake.content_type, "application/octet-stream", "тип картинки — только по содержимому");

    // Сообщение только из вложений (без текста)
    let send = SendMessageRequest { content: "".into(), attachment_ids: vec![img.id, txt.id], ..Default::default() };
    let (st, m) = srv.post::<_, Message>(&format!("/channels/{text}/messages"), Some(&alice.token), &send).await;
    assert_eq!(st, StatusCode::CREATED);
    let m = m.unwrap();
    assert_eq!(m.attachments.iter().map(|a| a.id).collect::<Vec<_>>(), vec![img.id, txt.id]);
    // Второй раз то же вложение не прикрепить; чужое — тоже
    assert_eq!(srv.post::<_, ApiError>(&format!("/channels/{text}/messages"), Some(&alice.token), &send).await.0, StatusCode::BAD_REQUEST);
    let steal = SendMessageRequest { content: "моё".into(), attachment_ids: vec![fake.id], ..Default::default() };
    assert_eq!(srv.post::<_, ApiError>(&format!("/channels/{text}/messages"), Some(&bob.token), &steal).await.0, StatusCode::BAD_REQUEST);

    // История отдаёт вложения
    let (_, hist) = srv.get::<Vec<Message>>(&format!("/channels/{text}/messages"), &bob.token).await;
    assert_eq!(hist.unwrap()[0].attachments.len(), 2);

    // Участник скачивает оригинал и превью; посторонний — нет
    let get = |path: String, token: String| {
        let req = srv.http.get(srv.url(&path)).bearer_auth(token);
        async move { req.send().await.unwrap() }
    };
    let orig = get(format!("/attachments/{}", img.id), bob.token.clone()).await;
    assert_eq!(orig.status(), StatusCode::OK);
    assert_eq!(orig.headers()["content-type"], "image/png");
    let thumb = get(format!("/attachments/{}/thumb", img.id), bob.token.clone()).await.bytes().await.unwrap();
    assert_eq!(&thumb[..2], &[0xFF, 0xD8], "превью — JPEG");
    let t = image::load_from_memory(&thumb).unwrap();
    assert!(t.width() <= 400 && t.height() <= 300);
    let file = get(format!("/attachments/{}", txt.id), bob.token.clone()).await;
    assert!(file.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"), "не картинки — только скачивание");
    assert_eq!(get(format!("/attachments/{}", img.id), carol.token.clone()).await.status(), StatusCode::NOT_FOUND);

    // Удалили сообщение — файлов больше нет
    srv.req::<(), ()>(reqwest::Method::DELETE, &format!("/channels/{text}/messages/{}", m.id), Some(&alice.token), None).await;
    assert_eq!(get(format!("/attachments/{}", img.id), alice.token.clone()).await.status(), StatusCode::NOT_FOUND);
    srv.drop_db().await;
}

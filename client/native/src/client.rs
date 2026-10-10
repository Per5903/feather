//! Настоящий клиент: вход, серверы, каналы, сообщения в реальном времени.
//!
//! Потоки: интерфейс Slint живёт в главном потоке, сеть — в tokio (`feather-core`).
//! Состояние интерфейса (`App`) лежит в thread_local главного потока; фоновые задачи
//! возвращают результаты через `post`, который выполняет замыкание в потоке интерфейса.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use feather_core::{Api, GatewayEvent, GatewayHandle};
use feather_protocol::{Channel, ChannelKind as Kind, ClientOp, Event, Guild, Message, Snowflake, User};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, VecModel, Weak};
use tokio::runtime::Handle;

use crate::session::{self, Session};
use crate::util::{initial, time_label, user_color};
use crate::{AppWindow, ChannelItem, ChannelKind, GuildItem, Member, Msg};

const DEFAULT_SERVER: &str = "http://127.0.0.1:8080";
const HISTORY_LIMIT: u32 = 100;
/// Сообщения одного автора в пределах 5 минут идут одной группой без шапки
const GROUP_GAP_MS: u64 = 5 * 60_000;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Выполнить в потоке интерфейса с доступом к состоянию (если пользователь вошёл)
fn post(f: impl FnOnce(&mut App) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        APP.with_borrow_mut(|app| {
            if let Some(app) = app.as_mut() {
                f(app);
            }
        })
    });
}

/// То же, но и до входа: для экрана логина
fn post_ui(ui: Weak<AppWindow>, f: impl FnOnce(&AppWindow) + Send + 'static) {
    let _ = ui.upgrade_in_event_loop(move |ui| f(&ui));
}

fn device_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Feather desktop".into())
}

pub fn run(ui: &AppWindow, rt: Handle) {
    let saved = session::load().unwrap_or_default();
    ui.set_server_url(if saved.server.is_empty() { DEFAULT_SERVER.into() } else { saved.server.as_str().into() });
    ui.set_login_name(saved.username.as_str().into());

    for register in [false, true] {
        let weak = ui.as_weak();
        let rt = rt.clone();
        let handler = move || login(weak.clone(), rt.clone(), register);
        if register { ui.on_register(handler) } else { ui.on_login(handler) }
    }
    wire_callbacks(ui);

    // Сохранённая сессия: входим без пароля
    if let Some(token) = saved.token {
        ui.set_login_busy(true);
        let api = Api::new(&saved.server).with_token(token);
        let weak = ui.as_weak();
        let rt2 = rt.clone();
        rt.spawn(async move {
            match api.me().await {
                Ok(me) => post_ui(weak.clone(), move |ui| App::start(ui, rt2, api, me)),
                Err(e) => post_ui(weak, move |ui| {
                    ui.set_login_busy(false);
                    if e.is_unauthorized() {
                        session::forget_token();
                        ui.set_login_error("Сессия истекла, войдите снова".into());
                    } else {
                        ui.set_login_error(e.to_string().into());
                    }
                }),
            }
        });
    }
}

fn login(weak: Weak<AppWindow>, rt: Handle, register: bool) {
    let Some(ui) = weak.upgrade() else { return };
    let (server, name, password) = (ui.get_server_url().to_string(), ui.get_login_name().to_string(), ui.get_login_password().to_string());
    if name.trim().is_empty() || password.is_empty() {
        ui.set_login_error("Введите имя и пароль".into());
        return;
    }
    ui.set_login_busy(true);
    ui.set_login_error(SharedString::new());
    let rt2 = rt.clone();
    rt.spawn(async move {
        let api = Api::new(&server);
        let result = async {
            if register {
                api.register(name.trim(), &password).await?;
            }
            api.login(name.trim(), &password, &device_name()).await
        }
        .await;
        post_ui(weak, move |ui| {
            ui.set_login_busy(false);
            match result {
                Ok(resp) => {
                    ui.set_login_password(SharedString::new());
                    let api = api.with_token(resp.token.clone());
                    session::save(&Session { server: api.base().into(), username: resp.user.username.clone(), token: Some(resp.token) });
                    App::start(ui, rt2, api, resp.user);
                }
                Err(e) => ui.set_login_error(e.to_string().into()),
            }
        });
    });
}

fn wire_callbacks(ui: &AppWindow) {
    ui.on_select_guild(|i| post(move |app| {
        if let Some(id) = app.guilds.get(i as usize).map(|g| g.id) {
            app.select_guild(id);
        }
    }));
    ui.on_select_channel(|i| post(move |app| {
        if let Some(c) = app.channels.get(i as usize).cloned() {
            app.select_channel(&c);
        }
    }));
    ui.on_send(|text| post(move |app| app.send(text.trim().to_owned())));
    ui.on_logout(|| post(|app| app.logout()));
    ui.on_invite(|| post(|app| app.invite()));

    let weak = ui.as_weak();
    ui.on_create_guild_dialog(move || open_dialog(&weak, "create"));
    let weak = ui.as_weak();
    ui.on_join_guild_dialog(move || open_dialog(&weak, "join"));
    ui.on_dialog_ok(|| post(|app| app.dialog_ok()));
}

fn open_dialog(weak: &Weak<AppWindow>, kind: &str) {
    if let Some(ui) = weak.upgrade() {
        ui.set_dialog_value(SharedString::new());
        ui.set_dialog(kind.into());
    }
}

/// Порядок как в Discord: каналы без категории, затем категории со своими каналами
fn ordered(mut cs: Vec<Channel>) -> Vec<Channel> {
    cs.sort_by_key(|c| (c.position, c.id));
    let mut out: Vec<Channel> = cs.iter().filter(|c| c.parent_id.is_none() && c.kind != Kind::Category).cloned().collect();
    for cat in cs.iter().filter(|c| c.kind == Kind::Category) {
        out.push(cat.clone());
        out.extend(cs.iter().filter(|c| c.parent_id == Some(cat.id)).cloned());
    }
    out
}

fn msg_row(prev: Option<&Message>, m: &Message) -> Msg {
    let ts = m.id.timestamp_ms();
    let head = prev.is_none_or(|p| p.author.id != m.author.id || ts.saturating_sub(p.id.timestamp_ms()) > GROUP_GAP_MS);
    let name = m.author.display_name.as_deref().unwrap_or(&m.author.username);
    Msg {
        head,
        author: name.into(),
        initial: initial(name),
        tint: user_color(m.author.id),
        time: time_label(ts as i64),
        text: m.content.as_str().into(),
        edited: m.edited_at.is_some(),
        ..Default::default()
    }
}

struct App {
    ui: Weak<AppWindow>,
    rt: Handle,
    api: Api,
    gw: Option<GatewayHandle>,
    me: User,
    guilds: Vec<Guild>,
    guild: Option<Snowflake>,
    /// В порядке отображения
    channels: Vec<Channel>,
    /// Непрочитанные каналы → их сервер (метка сервера = есть хоть один его канал)
    unread: HashMap<Snowflake, Snowflake>,
    channel: Option<Snowflake>,
    members: Vec<User>,
    /// Сообщения открытого канала, от старых к новым
    messages: Vec<Message>,
    model: Rc<VecModel<Msg>>,
    /// Номер загрузки: ответы на устаревшие запросы (пользователь уже ушёл в другой канал) игнорируем
    load_gen: u64,
}

impl App {
    fn start(ui: &AppWindow, rt: Handle, api: Api, me: User) {
        let model = Rc::new(VecModel::<Msg>::default());
        ui.set_messages(ModelRc::from(model.clone()));
        ui.set_me_name(me.username.as_str().into());
        ui.set_me_initial(initial(&me.username));
        ui.set_me_tint(user_color(me.id));
        ui.set_status(SharedString::new());
        ui.set_logged_in(true);

        // Шлюз: события из фона переправляем в поток интерфейса
        let gw = {
            let _guard = rt.enter();
            feather_core::gateway::spawn(api.gateway_url(), api.token().unwrap_or_default().into(), |ev| {
                post(move |app| app.on_gateway(ev))
            })
        };
        let app = App {
            ui: ui.as_weak(),
            rt,
            api,
            gw: Some(gw),
            me,
            guilds: Vec::new(),
            guild: None,
            channels: Vec::new(),
            unread: HashMap::new(),
            channel: None,
            members: Vec::new(),
            messages: Vec::new(),
            model,
            load_gen: 0,
        };
        APP.with_borrow_mut(|slot| *slot = Some(app));
    }

    fn ui(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    fn set_status(&self, text: impl Into<SharedString>) {
        if let Some(ui) = self.ui() {
            ui.set_status(text.into());
        }
    }

    fn gw_send(&self, op: ClientOp) {
        if let Some(gw) = &self.gw {
            gw.send(op);
        }
    }

    // ------------------------------------------------------------ отрисовка списков

    fn refresh_guilds(&self) {
        let Some(ui) = self.ui() else { return };
        let items: Vec<GuildItem> = self
            .guilds
            .iter()
            .map(|g| GuildItem {
                name: g.name.as_str().into(),
                // Инициалы как в Discord: первые буквы слов, максимум 3
                initial: g.name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect::<String>().into(),
                active: Some(g.id) == self.guild,
                unread: Some(g.id) != self.guild && self.unread.values().any(|gid| *gid == g.id),
            })
            .collect();
        ui.set_guilds(ModelRc::new(VecModel::from(items)));
        let name = self.guild.and_then(|id| self.guilds.iter().find(|g| g.id == id)).map(|g| g.name.clone()).unwrap_or_default();
        ui.set_guild_name(name.into());
    }

    fn refresh_channels(&self) {
        let Some(ui) = self.ui() else { return };
        let items: Vec<ChannelItem> = self
            .channels
            .iter()
            .map(|c| ChannelItem {
                name: if c.kind == Kind::Category { c.name.to_uppercase().into() } else { c.name.as_str().into() },
                kind: match c.kind {
                    Kind::Text => ChannelKind::Text,
                    Kind::Voice => ChannelKind::Voice,
                    Kind::Category => ChannelKind::Category,
                },
                active: Some(c.id) == self.channel,
                unread: self.unread.contains_key(&c.id),
            })
            .collect();
        ui.set_channels(ModelRc::new(VecModel::from(items)));
        let current = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id));
        ui.set_channel_name(current.map(|c| c.name.clone()).unwrap_or_default().into());
        ui.set_can_write(current.is_some_and(|c| c.kind == Kind::Text));
    }

    fn refresh_members(&self) {
        let Some(ui) = self.ui() else { return };
        let mut ms: Vec<&User> = self.members.iter().collect();
        ms.sort_by_key(|u| u.username.to_lowercase());
        let items: Vec<Member> = ms
            .into_iter()
            .map(|u| Member { name: u.username.as_str().into(), initial: initial(&u.username), tint: user_color(u.id) })
            .collect();
        ui.set_members(ModelRc::new(VecModel::from(items)));
    }

    fn rebuild_messages(&self) {
        let rows: Vec<Msg> = self.messages.iter().enumerate().map(|(i, m)| msg_row(i.checked_sub(1).map(|p| &self.messages[p]), m)).collect();
        self.model.set_vec(rows);
    }

    fn scroll_to_bottom_later(&self) {
        // После изменения модели ListView пересчитывает высоту на следующем кадре
        let weak = self.ui.clone();
        Timer::single_shot(Duration::from_millis(20), move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_scroll_to_bottom();
            }
        });
    }

    // ------------------------------------------------------------ навигация

    fn select_guild(&mut self, id: Snowflake) {
        if let Some(prev) = self.guild.filter(|p| *p != id) {
            self.gw_send(ClientOp::UnsubscribeGuild { guild_id: prev });
        }
        self.guild = Some(id);
        self.gw_send(ClientOp::SubscribeGuild { guild_id: id });
        self.channel = None;
        self.channels.clear();
        self.members.clear();
        self.messages.clear();
        self.model.set_vec(Vec::new());
        self.refresh_guilds();
        self.refresh_channels();
        self.refresh_members();

        self.load_gen += 1;
        let (api, generation) = (self.api.clone(), self.load_gen);
        self.rt.spawn(async move {
            let res = tokio::try_join!(api.channels(id), api.members(id));
            post(move |app| {
                if app.load_gen != generation {
                    return;
                }
                match res {
                    Ok((channels, members)) => {
                        app.channels = ordered(channels);
                        app.members = members;
                        app.refresh_members();
                        if let Some(first) = app.channels.iter().find(|c| c.kind == Kind::Text).cloned() {
                            app.select_channel(&first);
                        } else {
                            app.refresh_channels();
                        }
                    }
                    Err(e) => app.set_status(format!("Не удалось загрузить сервер: {e}")),
                }
            });
        });
    }

    fn select_channel(&mut self, c: &Channel) {
        match c.kind {
            Kind::Category => return,
            Kind::Voice => {
                self.set_status("Голосовые каналы появятся на этапе 2");
                return;
            }
            Kind::Text => {}
        }
        self.set_status("");
        self.channel = Some(c.id);
        self.unread.remove(&c.id);
        self.refresh_channels();
        self.refresh_guilds();
        self.messages.clear();
        self.model.set_vec(Vec::new());

        self.load_gen += 1;
        let (api, generation, id) = (self.api.clone(), self.load_gen, c.id);
        self.rt.spawn(async move {
            let res = api.history(id, None, HISTORY_LIMIT).await;
            post(move |app| {
                if app.load_gen != generation {
                    return;
                }
                match res {
                    Ok(mut history) => {
                        history.reverse(); // сервер отдаёт новые сверху
                        // Пока шла загрузка, могли прийти новые сообщения через шлюз
                        for m in std::mem::take(&mut app.messages) {
                            if !history.iter().any(|h| h.id == m.id) {
                                history.push(m);
                            }
                        }
                        app.messages = history;
                        app.rebuild_messages();
                        app.scroll_to_bottom_later();
                        app.ack_latest();
                    }
                    Err(e) => app.set_status(format!("Не удалось загрузить историю: {e}")),
                }
            });
        });
    }

    // ------------------------------------------------------------ сообщения

    fn add_message(&mut self, m: Message) {
        if self.messages.iter().rev().take(50).any(|x| x.id == m.id) {
            return; // уже есть: пришло и ответом на отправку, и событием шлюза
        }
        let stick = m.author.id == self.me.id || self.ui().is_some_and(|ui| ui.invoke_near_bottom());
        // Сообщения приходят по порядку ID; редкий случай «вне порядка» — полная пересборка
        if self.messages.last().is_some_and(|last| last.id > m.id) {
            self.messages.push(m);
            self.messages.sort_by_key(|x| x.id);
            self.rebuild_messages();
        } else {
            let row = msg_row(self.messages.last(), &m);
            self.messages.push(m);
            self.model.push(row);
        }
        if stick {
            self.scroll_to_bottom_later();
        }
    }

    /// Отметить открытый канал прочитанным до последнего сообщения
    fn ack_latest(&self) {
        let (Some(channel), Some(last)) = (self.channel, self.messages.last()) else { return };
        if last.author.id == self.me.id {
            return; // свои сообщения сервер отмечает прочитанными сам
        }
        let (api, id) = (self.api.clone(), last.id);
        self.rt.spawn(async move {
            let _ = api.ack(channel, id).await;
        });
    }

    fn send(&mut self, text: String) {
        let Some(channel) = self.channel else { return };
        if text.is_empty() {
            return;
        }
        let nonce = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.send_message(channel, &text, nonce).await;
            post(move |app| match res {
                // Ответ сервера добавляем сразу: не ждём шлюз (и работаем, даже если он переподключается)
                Ok(m) if app.channel == Some(m.channel_id) => app.add_message(m),
                Ok(_) => {}
                Err(e) => app.set_status(format!("Сообщение не отправлено: {e}")),
            });
        });
    }

    // ------------------------------------------------------------ события шлюза

    fn on_gateway(&mut self, ev: GatewayEvent) {
        match ev {
            GatewayEvent::Ready { user, guilds, unread } => {
                self.me = user;
                self.guilds = guilds;
                self.unread = unread.into_iter().map(|u| (u.channel_id, u.guild_id)).collect();
                if let Some(c) = self.channel {
                    self.unread.remove(&c);
                }
                self.set_status("");
                match self.guild.filter(|g| self.guilds.iter().any(|x| x.id == *g)) {
                    // Переподключение: подписка на сервере сбросилась, возобновляем и догружаем пропущенное
                    Some(g) => {
                        self.gw_send(ClientOp::SubscribeGuild { guild_id: g });
                        if let Some(c) = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id)).cloned() {
                            self.select_channel(&c);
                        }
                        self.refresh_guilds();
                    }
                    None => match self.guilds.first().map(|g| g.id) {
                        Some(first) => self.select_guild(first),
                        None => {
                            self.refresh_guilds();
                            self.set_status("У вас пока нет серверов: создайте свой (+) или войдите по приглашению (→)");
                        }
                    },
                }
            }
            GatewayEvent::Event(event) => self.on_event(event),
            GatewayEvent::Reconnecting(_) => self.set_status("Нет связи с сервером, переподключаюсь…"),
            GatewayEvent::SessionInvalid(_) => {
                self.logout();
                if let Some(ui) = self.ui() {
                    ui.set_login_error("Сессия истекла, войдите снова".into());
                }
            }
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::MessageCreate(m) => {
                if Some(m.channel_id) == self.channel {
                    self.add_message(m);
                    self.ack_latest();
                } else if m.author.id != self.me.id && self.channels.iter().any(|c| c.id == m.channel_id) {
                    if let Some(g) = self.guild {
                        self.unread.insert(m.channel_id, g);
                    }
                    self.refresh_channels();
                }
            }
            Event::MessageUpdate(m) => {
                if let Some(i) = self.messages.iter().position(|x| x.id == m.id) {
                    let row = msg_row(i.checked_sub(1).map(|p| &self.messages[p]), &m);
                    self.messages[i] = m;
                    self.model.set_row_data(i, row);
                }
            }
            Event::MessageDelete { id, .. } => {
                if let Some(i) = self.messages.iter().position(|x| x.id == id) {
                    self.messages.remove(i);
                    self.model.remove(i);
                    // У следующего сообщения могла появиться (или пропасть) шапка
                    if i < self.messages.len() {
                        let row = msg_row(i.checked_sub(1).map(|p| &self.messages[p]), &self.messages[i]);
                        self.model.set_row_data(i, row);
                    }
                }
            }
            Event::ChannelCreate(c) => {
                if Some(c.guild_id) == self.guild && !self.channels.iter().any(|x| x.id == c.id) {
                    let mut all = std::mem::take(&mut self.channels);
                    all.push(c);
                    self.channels = ordered(all);
                    self.refresh_channels();
                }
            }
            Event::GuildMemberAdd { guild_id, user } => {
                if Some(guild_id) == self.guild && !self.members.iter().any(|u| u.id == user.id) {
                    self.members.push(user);
                    self.refresh_members();
                }
            }
            Event::GuildCreate(g) => {
                if !self.guilds.iter().any(|x| x.id == g.id) {
                    self.guilds.push(g);
                    self.refresh_guilds();
                }
            }
            Event::UnreadUpdate { guild_id, channel_id, author_id, .. } => {
                if author_id != self.me.id {
                    self.unread.insert(channel_id, guild_id);
                    self.refresh_guilds();
                }
            }
            Event::ChannelAck { channel_id, .. } => {
                // Дочитано на другом устройстве
                if self.unread.remove(&channel_id).is_some() {
                    self.refresh_guilds();
                    self.refresh_channels();
                }
            }
        }
    }

    // ------------------------------------------------------------ серверы, инвайты, выход

    fn invite(&self) {
        let Some(guild) = self.guild else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.create_invite(guild).await;
            post(move |app| match res {
                Ok(inv) => {
                    if let Some(ui) = app.ui() {
                        ui.set_dialog_value(inv.code.into());
                        ui.set_dialog("invite".into());
                    }
                }
                Err(e) => app.set_status(format!("Не удалось создать приглашение: {e}")),
            });
        });
    }

    fn dialog_ok(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (kind, value) = (ui.get_dialog().to_string(), ui.get_dialog_value().trim().to_owned());
        if value.is_empty() {
            return;
        }
        ui.set_dialog(SharedString::new());
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = match kind.as_str() {
                "create" => api.create_guild(&value).await.map(|g| g.guild),
                "join" => api.accept_invite(&value).await,
                _ => return,
            };
            post(move |app| match res {
                Ok(g) => {
                    if !app.guilds.iter().any(|x| x.id == g.id) {
                        app.guilds.push(g.clone());
                    }
                    app.select_guild(g.id);
                }
                Err(e) => app.set_status(e.to_string()),
            });
        });
    }

    fn logout(&mut self) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            let _ = api.logout().await;
        });
        session::forget_token();
        // Закрываем шлюз: последняя копия GatewayHandle уничтожается
        self.gw = None;
        if let Some(ui) = self.ui() {
            ui.set_logged_in(false);
            ui.set_dialog(SharedString::new());
            ui.set_guilds(ModelRc::default());
            ui.set_channels(ModelRc::default());
            ui.set_members(ModelRc::default());
            ui.set_messages(ModelRc::default());
            ui.set_guild_name(SharedString::new());
            ui.set_channel_name(SharedString::new());
            ui.set_login_name(self.me.username.as_str().into());
        }
        // Состояние удаляем после выхода из текущего заимствования APP
        let _ = slint::invoke_from_event_loop(|| APP.with_borrow_mut(|a| *a = None));
    }
}

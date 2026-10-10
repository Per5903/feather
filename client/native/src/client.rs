//! Настоящий клиент: вход, серверы, каналы, сообщения в реальном времени.
//!
//! Потоки: интерфейс Slint живёт в главном потоке, сеть — в tokio (`feather-core`).
//! Состояние интерфейса (`App`) лежит в thread_local главного потока; фоновые задачи
//! возвращают результаты через `post`, который выполняет замыкание в потоке интерфейса.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use feather_core::{Api, GatewayEvent, GatewayHandle, HotkeyEvent, HotkeyHandle, VoiceEvent, VoiceOptions, VoiceSession};
use feather_protocol::{Channel, ChannelKind as Kind, ClientOp, DmChannel, Event, Guild, Message, Snowflake, User, VoiceState};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, VecModel, Weak};
use tokio::runtime::Handle;

use crate::session::{self, Session};
use crate::settings::{self, Settings};
use crate::util::{initial, time_label, user_color};
use crate::{AppWindow, ChannelItem, ChannelKind, GuildItem, Member, Msg, VoiceMember};

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
                    let prev = session::load().filter(|s| s.server == api.base() && s.username == resp.user.username);
                    session::save(&Session {
                        server: api.base().into(),
                        username: resp.user.username.clone(),
                        token: Some(resp.token),
                        ..prev.unwrap_or_default()
                    });
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
    ui.on_edit_message(|i| post(move |app| app.start_edit(i as usize)));
    ui.on_cancel_edit(|| post(|app| app.cancel_edit()));
    ui.on_delete_message(|i| post(move |app| app.ask_delete(i as usize)));
    ui.on_typing(|| post(|app| app.on_local_typing()));
    ui.on_load_older(|| post(|app| app.load_older()));
    ui.on_toggle_mute(|| post(|app| app.toggle_mute()));
    ui.on_open_settings(|| post(|app| app.open_settings()));
    ui.on_save_settings(|| post(|app| app.save_settings()));
    ui.on_configure_ptt(|| post(|app| {
        if let Some(h) = &app.hotkey {
            h.configure();
        }
    }));
    ui.on_toggle_deafen(|| post(|app| app.toggle_deafen()));
    ui.on_leave_voice(|| post(|app| app.leave_voice()));
    ui.on_logout(|| post(|app| app.logout()));
    ui.on_invite(|| post(|app| app.invite()));
    ui.on_add_channel(|i| post(move |app| app.ask_add_channel(i as usize)));
    ui.on_select_home(|| post(|app| app.select_home(None)));
    ui.on_message_member(|i| post(move |app| app.message_member(i as usize)));

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

fn msg_row(prev: Option<&Message>, m: &Message, me: Snowflake, moderator: bool) -> Msg {
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
        own: m.author.id == me,
        can_delete: m.author.id == me || moderator,
        ..Default::default()
    }
}

/// «Шторм печатает…», «Шторм и Лиса печатают…», «Несколько человек печатают…»
fn typing_label(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [a] => format!("{a} печатает…"),
        [a, b] => format!("{a} и {b} печатают…"),
        _ => "Несколько человек печатают…".into(),
    }
}

/// Сколько показывать «печатает» после последнего сигнала
const TYPING_SHOW: Duration = Duration::from_secs(10);
/// Как часто сам клиент сообщает «я печатаю»
const TYPING_SEND_EVERY: Duration = Duration::from_secs(8);

/// Голосовой канал, в котором мы сейчас
struct Voice {
    channel: Snowflake,
    channel_name: String,
    /// None — ещё подключаемся
    session: Option<VoiceSession>,
    me: String,
    speaking: Vec<String>,
    muted: bool,
    deafened: bool,
    /// Номер подключения: события старой сессии после переподключения игнорируем
    id: u64,
}

const APP_ID: &str = "app.feather.Feather";
const DEFAULT_DEVICE: &str = "По умолчанию";

/// Настройки голоса. Переменные окружения перекрывают настройки (для автотестов):
/// FEATHER_MIC / FEATHER_SPEAKER — подстрока имени устройства, FEATHER_NO_RNNOISE, FEATHER_NO_AEC
fn voice_options(s: &Settings) -> VoiceOptions {
    VoiceOptions {
        noise_suppression: s.noise_suppression && std::env::var_os("FEATHER_NO_RNNOISE").is_none(),
        echo_cancellation: s.echo_cancellation && std::env::var_os("FEATHER_NO_AEC").is_none(),
        input_device: std::env::var("FEATHER_MIC").ok().or_else(|| s.input_device.clone()),
        output_device: std::env::var("FEATHER_SPEAKER").ok().or_else(|| s.output_device.clone()),
        // В режиме PTT решает клавиша; FEATHER_NO_VAD — выключить для замеров
        voice_activation: !s.push_to_talk && std::env::var_os("FEATHER_NO_VAD").is_none(),
    }
}

/// После отпускания push-to-talk звук идёт ещё столько: не обрезаем последний слог
const PTT_RELEASE_TAIL: Duration = Duration::from_millis(250);

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
    /// Непрочитанные каналы → их сервер (None — личный). Метка сервера = есть хоть один его канал
    unread: HashMap<Snowflake, Option<Snowflake>>,
    /// Режим «Личные сообщения» и список диалогов
    dm_mode: bool,
    dms: Vec<DmChannel>,
    /// Участники в том порядке, в каком показаны (для кнопки «Написать»)
    members_view: Vec<User>,
    channel: Option<Snowflake>,
    members: Vec<User>,
    /// Сообщения открытого канала, от старых к новым
    messages: Vec<Message>,
    model: Rc<VecModel<Msg>>,
    /// Номер загрузки: ответы на устаревшие запросы (пользователь уже ушёл в другой канал) игнорируем
    load_gen: u64,
    /// Идёт подгрузка старых сообщений / в канале есть ещё более старые
    loading_older: bool,
    has_older: bool,
    voice: Option<Voice>,
    voice_seq: u64,
    /// Кто в голосовых каналах (по данным сервера): пользователь → состояние
    voice_states: HashMap<Snowflake, VoiceState>,
    settings: Settings,
    /// Глобальная клавиша push-to-talk (пока включён режим PTT)
    hotkey: Option<HotkeyHandle>,
    ptt_pressed: bool,
    /// Редактируемое сообщение
    editing: Option<Snowflake>,
    /// Сообщение, ожидающее подтверждения удаления
    pending_delete: Option<Snowflake>,
    /// Категория, в которой создаётся канал
    pending_parent: Option<Snowflake>,
    /// Кто печатает в открытом канале: пользователь → (имя, когда сообщил)
    typing: HashMap<Snowflake, (String, std::time::Instant)>,
    last_typing_sent: Option<std::time::Instant>,
    typing_timer: Timer,
    /// Списки устройств, показанные в настройках (индексы ComboBox → имена)
    device_lists: (Vec<String>, Vec<String>),
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
            dm_mode: false,
            dms: Vec::new(),
            members_view: Vec::new(),
            channel: None,
            members: Vec::new(),
            messages: Vec::new(),
            model,
            load_gen: 0,
            loading_older: false,
            has_older: false,
            voice: None,
            voice_seq: 0,
            voice_states: HashMap::new(),
            settings: settings::load(),
            hotkey: None,
            ptt_pressed: false,
            editing: None,
            pending_delete: None,
            pending_parent: None,
            typing: HashMap::new(),
            last_typing_sent: None,
            typing_timer: Timer::default(),
            device_lists: (Vec::new(), Vec::new()),
        };
        APP.with_borrow_mut(|slot| *slot = Some(app));
        APP.with_borrow_mut(|slot| {
            if let Some(app) = slot.as_mut() {
                app.update_hotkey();
                // Для снимков интерфейса в автотестах
                if std::env::var_os("FEATHER_OPEN_SETTINGS").is_some() {
                    app.open_settings();
                }
            }
        });
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
                unread: Some(g.id) != self.guild && self.unread.values().any(|gid| *gid == Some(g.id)),
            })
            .collect();
        ui.set_guilds(ModelRc::new(VecModel::from(items)));
        let name = if self.dm_mode {
            "Личные сообщения".to_owned()
        } else {
            self.guild.and_then(|id| self.guilds.iter().find(|g| g.id == id)).map(|g| g.name.clone()).unwrap_or_default()
        };
        ui.set_guild_name(name.into());
        ui.set_in_dm(self.dm_mode);
        ui.set_home_unread(self.unread.values().any(|g| g.is_none()));
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
                    Kind::Dm => ChannelKind::Dm,
                },
                active: Some(c.id) == self.channel || self.voice.as_ref().is_some_and(|v| v.channel == c.id),
                unread: self.unread.contains_key(&c.id),
                voice_members: self.voice_members(c.id),
                // Создавать каналы пока может владелец сервера (роли — следующий шаг)
                can_add: c.kind == Kind::Category && self.moderator(),
            })
            .collect();
        ui.set_channels(ModelRc::new(VecModel::from(items)));
        let current = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id));
        ui.set_channel_name(current.map(|c| c.name.clone()).unwrap_or_default().into());
        ui.set_can_write(current.is_some_and(|c| matches!(c.kind, Kind::Text | Kind::Dm)));
    }

    fn voice_members(&self, channel: Snowflake) -> ModelRc<VoiceMember> {
        let mut states: Vec<&VoiceState> = self.voice_states.values().filter(|v| v.channel_id == Some(channel)).collect();
        if states.is_empty() {
            return ModelRc::default();
        }
        states.sort_by_key(|v| v.user.username.to_lowercase());
        // Кто говорит — знаем, только если сами в этом канале (данные LiveKit)
        let speaking = self.voice.as_ref().filter(|v| v.channel == channel).map(|v| &v.speaking);
        let rows: Vec<VoiceMember> = states
            .into_iter()
            .map(|v| {
                let name = v.user.display_name.as_deref().unwrap_or(&v.user.username);
                VoiceMember {
                    name: name.into(),
                    initial: initial(name),
                    tint: user_color(v.user.id),
                    speaking: speaking.is_some_and(|s| s.contains(&v.user.id.to_string())),
                    muted: v.muted || v.deafened,
                }
            })
            .collect();
        ModelRc::new(VecModel::from(rows))
    }

    /// Сообщить серверу своё голосовое состояние (его увидят все участники сервера)
    fn send_voice_state(&self) {
        let op = match &self.voice {
            Some(v) if v.session.is_some() => ClientOp::VoiceState { channel_id: Some(v.channel), muted: v.muted, deafened: v.deafened },
            Some(_) => return, // ещё подключаемся
            None => ClientOp::VoiceState { channel_id: None, muted: false, deafened: false },
        };
        self.gw_send(op);
    }

    fn refresh_voice(&self) {
        let Some(ui) = self.ui() else { return };
        match &self.voice {
            Some(v) => {
                ui.set_voice_channel(v.channel_name.as_str().into());
                ui.set_voice_state(if v.session.is_some() { "Голос подключён" } else { "Подключение…" }.into());
                ui.set_voice_muted(v.muted || v.deafened);
                ui.set_voice_deafened(v.deafened);
            }
            None => ui.set_voice_channel(SharedString::new()),
        }
        self.refresh_channels();
    }

    fn refresh_members(&mut self) {
        let Some(ui) = self.ui() else { return };
        let mut ms: Vec<User> = self.members.clone();
        ms.sort_by_key(|u| u.username.to_lowercase());
        let items: Vec<Member> = ms
            .iter()
            .map(|u| Member {
                name: u.username.as_str().into(),
                initial: initial(&u.username),
                tint: user_color(u.id),
                can_message: u.id != self.me.id,
            })
            .collect();
        self.members_view = ms;
        ui.set_members(ModelRc::new(VecModel::from(items)));
    }

    // ------------------------------------------------------------ личные сообщения

    /// Личный канал как элемент списка каналов (имя — собеседники)
    fn dm_as_channel(d: &DmChannel) -> Channel {
        let name = d.recipients.iter().map(|u| u.display_name.clone().unwrap_or(u.username.clone())).collect::<Vec<_>>().join(", ");
        Channel { id: d.id, guild_id: Snowflake(0), kind: Kind::Dm, name, parent_id: None, position: 0 }
    }

    /// Свежие диалоги сверху
    fn sort_dms(&mut self) {
        self.dms.sort_by_key(|d| std::cmp::Reverse(d.last_message_id.unwrap_or(d.id)));
        if self.dm_mode {
            self.channels = self.dms.iter().map(Self::dm_as_channel).collect();
        }
    }

    /// Перейти в «Личные сообщения»; `open` — сразу открыть этот диалог
    fn select_home(&mut self, open: Option<Snowflake>) {
        if let Some(prev) = self.guild.take() {
            self.gw_send(ClientOp::UnsubscribeGuild { guild_id: prev });
        }
        self.dm_mode = true;
        self.channel = None;
        self.messages.clear();
        self.model.set_vec(Vec::new());
        self.members.clear();
        self.refresh_guilds();
        self.refresh_members();
        self.load_gen += 1;
        let (api, generation) = (self.api.clone(), self.load_gen);
        self.rt.spawn(async move {
            let res = api.dms().await;
            post(move |app| {
                if app.load_gen != generation || !app.dm_mode {
                    return;
                }
                match res {
                    Ok(dms) => {
                        app.dms = dms;
                        app.sort_dms();
                        let target = open
                            .and_then(|id| app.channels.iter().find(|c| c.id == id))
                            .or_else(|| app.channels.first())
                            .cloned();
                        match target {
                            Some(c) => app.select_channel(&c),
                            None => {
                                app.refresh_channels();
                                app.set_status("Диалогов пока нет: наведите на участника сервера и нажмите «Написать»");
                            }
                        }
                    }
                    Err(e) => app.set_status(format!("Не удалось загрузить диалоги: {e}")),
                }
            });
        });
    }

    /// «Написать» участнику: открыть (или создать) личный канал и перейти в него
    fn message_member(&mut self, i: usize) {
        let Some(user) = self.members_view.get(i).filter(|u| u.id != self.me.id).cloned() else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.open_dm(user.id).await;
            post(move |app| match res {
                Ok(dm) => {
                    if !app.dms.iter().any(|d| d.id == dm.id) {
                        app.dms.push(dm.clone());
                    }
                    app.select_home(Some(dm.id));
                }
                Err(e) => app.set_status(format!("Не удалось открыть диалог: {e}")),
            });
        });
    }

    /// Я владелец открытого сервера — могу удалять чужие сообщения
    fn moderator(&self) -> bool {
        self.guild.and_then(|g| self.guilds.iter().find(|x| x.id == g)).is_some_and(|g| g.owner_id == self.me.id)
    }

    fn row_at(&self, i: usize) -> Msg {
        msg_row(i.checked_sub(1).map(|p| &self.messages[p]), &self.messages[i], self.me.id, self.moderator())
    }

    fn rebuild_messages(&self) {
        let rows: Vec<Msg> = (0..self.messages.len()).map(|i| self.row_at(i)).collect();
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
        self.dm_mode = false;
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
                        // Открываем последний канал, где был пользователь, иначе первый текстовый
                        let remembered = session::load()
                            .and_then(|s| s.last_channels.get(&id.to_string()).cloned())
                            .and_then(|cid| app.channels.iter().find(|c| c.id.to_string() == cid && c.kind == Kind::Text).cloned());
                        // Для автотестов: FEATHER_AUTOJOIN_VOICE=1 сразу заходит в первый голосовой канал
                        if std::env::var_os("FEATHER_AUTOJOIN_VOICE").is_some()
                            && let Some(vc) = app.channels.iter().find(|c| c.kind == Kind::Voice).cloned()
                        {
                            app.join_voice(&vc);
                        }
                        if let Some(first) = remembered.or_else(|| app.channels.iter().find(|c| c.kind == Kind::Text).cloned()) {
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
                self.join_voice(c);
                return;
            }
            Kind::Text | Kind::Dm => {}
        }
        self.set_status("");
        self.typing.clear();
        self.refresh_typing();
        self.cancel_edit();
        self.channel = Some(c.id);
        if c.kind == Kind::Dm {
            // В личке справа — собеседник и я
            let dm = self.dms.iter().find(|d| d.id == c.id);
            self.members = dm.map(|d| d.recipients.clone()).unwrap_or_default();
            self.members.push(self.me.clone());
            self.refresh_members();
        } else {
            session::remember(&c.guild_id.to_string(), &c.id.to_string());
        }
        self.unread.remove(&c.id);
        self.refresh_channels();
        self.refresh_guilds();
        self.messages.clear();
        self.model.set_vec(Vec::new());
        self.loading_older = false;
        self.has_older = false;

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
                        app.has_older = history.len() as u32 == HISTORY_LIMIT;
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
                        // Для снимков интерфейса в автотестах: открыть правку своего последнего сообщения
                        if std::env::var_os("FEATHER_TEST_EDIT").is_some()
                            && let Some(i) = app.messages.iter().rposition(|m| m.author.id == app.me.id)
                        {
                            app.start_edit(i);
                        }
                    }
                    Err(e) => app.set_status(format!("Не удалось загрузить историю: {e}")),
                }
            });
        });
    }

    /// Подгрузка сообщений старше первого показанного (прокрутка вверх)
    fn load_older(&mut self) {
        let (Some(channel), Some(oldest)) = (self.channel, self.messages.first().map(|m| m.id)) else { return };
        if self.loading_older || !self.has_older {
            return;
        }
        self.loading_older = true;
        let (api, generation) = (self.api.clone(), self.load_gen);
        self.rt.spawn(async move {
            let res = api.history(channel, Some(oldest), HISTORY_LIMIT).await;
            post(move |app| {
                if app.load_gen != generation {
                    return;
                }
                app.loading_older = false;
                let Ok(mut older) = res else {
                    app.set_status("Не удалось загрузить старые сообщения");
                    return;
                };
                app.has_older = older.len() as u32 == HISTORY_LIMIT;
                if older.is_empty() {
                    return;
                }
                older.reverse();
                let added = older.len();
                if std::env::var_os("FEATHER_LOG").is_some() {
                    eprintln!("история: +{added} старых, всего {}, есть ещё: {}", added + app.messages.len(), app.has_older);
                }
                let Some(ui) = app.ui() else { return };
                let before = ui.invoke_content_height();
                older.append(&mut app.messages);
                app.messages = older;
                // Перестраиваем добавленные строки и первую старую (у неё могла пропасть шапка)
                let rows: Vec<Msg> = (0..=added.min(app.messages.len() - 1)).map(|i| app.row_at(i)).collect();
                let first_old = rows[added.min(rows.len() - 1)].clone();
                for (i, row) in rows.into_iter().take(added).enumerate() {
                    app.model.insert(i, row);
                }
                if added < app.messages.len() {
                    app.model.set_row_data(added, first_old);
                }
                // Лента выросла сверху: сдвигаем на прирост высоты, чтобы видимое место не убежало
                let weak = app.ui.clone();
                Timer::single_shot(Duration::from_millis(16), move || {
                    if let Some(ui) = weak.upgrade() {
                        let delta = ui.invoke_content_height() - before;
                        ui.invoke_shift_scroll(delta);
                    }
                });
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
            self.messages.push(m);
            let row = self.row_at(self.messages.len() - 1);
            self.model.push(row);
        }
        if stick {
            self.scroll_to_bottom_later();
        }
    }

    // ------------------------------------------------------------ правка, удаление, «печатает»

    fn set_editing_ui(&self, on: bool) {
        if let Some(ui) = self.ui() {
            ui.set_editing(on);
            if !on {
                ui.set_composer_text(SharedString::new());
            }
        }
    }

    fn start_edit(&mut self, i: usize) {
        let Some(m) = self.messages.get(i).filter(|m| m.author.id == self.me.id) else { return };
        self.editing = Some(m.id);
        if let Some(ui) = self.ui() {
            ui.set_composer_text(m.content.as_str().into());
            ui.set_editing(true);
        }
    }

    fn cancel_edit(&mut self) {
        self.editing = None;
        self.set_editing_ui(false);
    }

    fn ask_add_channel(&mut self, i: usize) {
        let Some(cat) = self.channels.get(i).filter(|c| c.kind == Kind::Category) else { return };
        self.pending_parent = Some(cat.id);
        if let Some(ui) = self.ui() {
            ui.set_dialog_value(SharedString::new());
            // В категории голосовых по умолчанию предлагаем голосовой канал
            let voice_cat = self.channels.iter().any(|c| c.parent_id == Some(cat.id) && c.kind == Kind::Voice);
            ui.set_dialog_switch(voice_cat);
            ui.set_dialog("channel".into());
        }
    }

    fn ask_delete(&mut self, i: usize) {
        let Some(m) = self.messages.get(i) else { return };
        if m.author.id != self.me.id && !self.moderator() {
            return;
        }
        self.pending_delete = Some(m.id);
        if let Some(ui) = self.ui() {
            // Показываем, что именно удаляем (коротко)
            let preview: String = m.content.chars().take(80).collect();
            ui.set_dialog_value(preview.into());
            ui.set_dialog("delete".into());
        }
    }

    fn delete_confirmed(&mut self) {
        let (Some(id), Some(channel)) = (self.pending_delete.take(), self.channel) else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.delete_message(channel, id).await {
                post(move |app| app.set_status(format!("Не удалось удалить: {e}")));
            }
            // Само удаление из ленты придёт событием MESSAGE_DELETE
        });
    }

    fn on_local_typing(&mut self) {
        let Some(channel) = self.channel else { return };
        if self.editing.is_some() || self.last_typing_sent.is_some_and(|t| t.elapsed() < TYPING_SEND_EVERY) {
            return;
        }
        self.last_typing_sent = Some(std::time::Instant::now());
        self.gw_send(ClientOp::Typing { channel_id: channel });
    }

    fn refresh_typing(&mut self) {
        self.typing.retain(|_, (_, at)| at.elapsed() < TYPING_SHOW);
        let mut names: Vec<&str> = self.typing.values().map(|(n, _)| n.as_str()).collect();
        names.sort();
        if let Some(ui) = self.ui() {
            ui.set_typing_text(typing_label(&names).into());
        }
        // Пока кто-то печатает — раз в секунду проверяем, не пора ли убрать надпись
        if self.typing.is_empty() {
            self.typing_timer.stop();
        } else if !self.typing_timer.running() {
            self.typing_timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), || post(|app| app.refresh_typing()));
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
        if let Some(id) = self.editing.take() {
            self.set_editing_ui(false);
            if self.messages.iter().any(|m| m.id == id && m.content == text) {
                return; // ничего не поменялось
            }
            let api = self.api.clone();
            self.rt.spawn(async move {
                let res = api.edit_message(channel, id, &text).await;
                post(move |app| match res {
                    Ok(m) => {
                        if let Some(i) = app.messages.iter().position(|x| x.id == m.id) {
                            app.messages[i] = m;
                            app.model.set_row_data(i, app.row_at(i));
                        }
                    }
                    Err(e) => app.set_status(format!("Не удалось изменить: {e}")),
                });
            });
            return;
        }
        self.last_typing_sent = None; // после отправки следующий набор снова сообщаем сразу
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
            GatewayEvent::Ready { user, guilds, unread, voice_states } => {
                self.me = user;
                self.guilds = guilds;
                self.voice_states = voice_states.into_iter().filter(|v| v.channel_id.is_some()).map(|v| (v.user.id, v)).collect();
                // Переподключение шлюза: сервер забыл наше голосовое состояние вместе со старой сессией
                self.send_voice_state();
                self.unread = unread.into_iter().map(|u| (u.channel_id, u.guild_id)).collect();
                if let Some(c) = self.channel {
                    self.unread.remove(&c);
                }
                self.set_status("");
                if self.dm_mode {
                    // Переподключение в личных сообщениях: догружаем открытый диалог
                    self.refresh_guilds();
                    if let Some(c) = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id)).cloned() {
                        self.select_channel(&c);
                    }
                    return;
                }
                match self.guild.filter(|g| self.guilds.iter().any(|x| x.id == *g)) {
                    // Переподключение: подписка на сервере сбросилась, возобновляем и догружаем пропущенное
                    Some(g) => {
                        self.gw_send(ClientOp::SubscribeGuild { guild_id: g });
                        if let Some(c) = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id)).cloned() {
                            self.select_channel(&c);
                        }
                        self.refresh_guilds();
                    }
                    // Для снимков интерфейса в автотестах: сразу в «Личные сообщения»
                    None if std::env::var_os("FEATHER_TEST_HOME").is_some() => self.select_home(None),
                    None => match session::load()
                        .and_then(|s| s.last_guild)
                        .and_then(|lg| self.guilds.iter().find(|g| g.id.to_string() == lg).map(|g| g.id))
                        .or_else(|| self.guilds.first().map(|g| g.id))
                    {
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
            Event::TypingStart { channel_id, user } => {
                if Some(channel_id) == self.channel {
                    let name = user.display_name.clone().unwrap_or(user.username.clone());
                    self.typing.insert(user.id, (name, std::time::Instant::now()));
                    self.refresh_typing();
                }
            }
            Event::MessageCreate(m) => {
                if self.typing.remove(&m.author.id).is_some() {
                    self.refresh_typing();
                }
                if Some(m.channel_id) == self.channel {
                    self.add_message(m);
                    self.ack_latest();
                } else if let Some(d) = self.dms.iter_mut().find(|d| d.id == m.channel_id) {
                    // Личное сообщение в неоткрытом диалоге: метка на «Личных сообщениях», диалог наверх
                    d.last_message_id = Some(m.id);
                    if m.author.id != self.me.id {
                        self.unread.insert(m.channel_id, None);
                    }
                    self.sort_dms();
                    self.refresh_guilds();
                    self.refresh_channels();
                } else if m.author.id != self.me.id && self.channels.iter().any(|c| c.id == m.channel_id) {
                    if let Some(g) = self.guild {
                        self.unread.insert(m.channel_id, Some(g));
                    }
                    self.refresh_channels();
                }
            }
            Event::MessageUpdate(m) => {
                if let Some(i) = self.messages.iter().position(|x| x.id == m.id) {
                    self.messages[i] = m;
                    self.model.set_row_data(i, self.row_at(i));
                }
            }
            Event::MessageDelete { id, .. } => {
                if let Some(i) = self.messages.iter().position(|x| x.id == id) {
                    self.messages.remove(i);
                    self.model.remove(i);
                    // У следующего сообщения могла появиться (или пропасть) шапка
                    if i < self.messages.len() {
                        self.model.set_row_data(i, self.row_at(i));
                    }
                    if self.editing == Some(id) {
                        self.cancel_edit();
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
            Event::DmCreate(d) => {
                if !self.dms.iter().any(|x| x.id == d.id) {
                    self.dms.push(d);
                    self.sort_dms();
                    self.refresh_channels();
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
                    self.unread.insert(channel_id, Some(guild_id));
                    self.refresh_guilds();
                }
            }
            Event::VoiceStateUpdate(v) => {
                if v.channel_id.is_some() {
                    self.voice_states.insert(v.user.id, v);
                } else {
                    self.voice_states.remove(&v.user.id);
                }
                self.refresh_channels();
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

    // ------------------------------------------------------------ голос

    fn join_voice(&mut self, c: &Channel) {
        if self.voice.as_ref().is_some_and(|v| v.channel == c.id) {
            return;
        }
        self.leave_voice();
        self.voice_seq += 1;
        let id = self.voice_seq;
        self.voice = Some(Voice {
            channel: c.id,
            channel_name: c.name.clone(),
            session: None,
            me: self.me.id.to_string(),
            speaking: Vec::new(),
            // FEATHER_START_MUTED=1 — заходить с выключенным микрофоном
            muted: std::env::var_os("FEATHER_START_MUTED").is_some(),
            deafened: false,
            id,
        });
        self.refresh_voice();
        let (api, channel, opts) = (self.api.clone(), c.id, voice_options(&self.settings));
        self.rt.spawn(async move {
            let joined = match api.voice_join(channel).await {
                Ok(j) => VoiceSession::join(&j.url, &j.token, opts, move |ev| post(move |app| app.on_voice(id, ev))).await,
                Err(e) => Err(e.to_string()),
            };
            post(move |app| {
                let current = app.voice.as_mut().filter(|v| v.id == id);
                match (joined, current) {
                    (Ok(session), Some(v)) => {
                        session.set_deafened(v.deafened);
                        v.session = Some(session);
                        app.apply_mic();
                        app.refresh_voice();
                        app.send_voice_state();
                    }
                    // Пользователь уже ушёл из канала, пока мы подключались
                    (Ok(session), None) => {
                        app.rt.spawn(session.leave());
                    }
                    (Err(e), _) => {
                        if app.voice.as_ref().is_some_and(|v| v.id == id) {
                            app.voice = None;
                            app.refresh_voice();
                        }
                        app.set_status(format!("Голос: {e}"));
                    }
                }
            });
        });
    }

    fn on_voice(&mut self, id: u64, ev: VoiceEvent) {
        let Some(v) = self.voice.as_mut().filter(|v| v.id == id) else { return };
        match ev {
            VoiceEvent::Joined { me, .. } => v.me = me,
            // Список участников берём с сервера (VOICE_STATE), а не из LiveKit
            VoiceEvent::PeerJoined(_) | VoiceEvent::PeerLeft(_) => return,
            VoiceEvent::Speaking(who) => v.speaking = who,
            VoiceEvent::Warning(w) => {
                self.set_status(w);
                return;
            }
            VoiceEvent::Disconnected(reason) => {
                self.voice = None;
                self.send_voice_state();
                self.set_status(format!("Голос отключён ({reason})"));
            }
        }
        self.refresh_voice();
    }

    fn toggle_mute(&mut self) {
        let Some(v) = self.voice.as_mut() else { return };
        // Как в Discord: включение микрофона снимает и «выключенный звук»
        if v.deafened {
            v.deafened = false;
            v.muted = false;
        } else {
            v.muted = !v.muted;
        }
        if let Some(s) = &v.session {
            s.set_deafened(v.deafened);
        }
        self.apply_mic();
        self.refresh_voice();
        self.send_voice_state();
    }

    fn toggle_deafen(&mut self) {
        let Some(v) = self.voice.as_mut() else { return };
        v.deafened = !v.deafened;
        if let Some(s) = &v.session {
            s.set_deafened(v.deafened);
        }
        self.refresh_voice();
        self.send_voice_state();
    }

    /// Микрофон реально передаёт звук: не выключен вручную и (в режиме PTT) клавиша нажата
    fn apply_mic(&self) {
        if let Some(v) = &self.voice
            && let Some(s) = &v.session
        {
            s.set_muted(v.muted || (self.settings.push_to_talk && !self.ptt_pressed));
        }
    }

    /// Включить/выключить глобальную клавишу в соответствии с настройкой PTT
    fn update_hotkey(&mut self) {
        if !self.settings.push_to_talk {
            self.hotkey = None;
            self.ptt_pressed = false;
            return;
        }
        if self.hotkey.is_none() {
            let _guard = self.rt.enter();
            self.hotkey = Some(feather_core::hotkey::register(APP_ID, |ev| post(move |app| app.on_hotkey(ev))));
        }
    }

    fn on_hotkey(&mut self, ev: HotkeyEvent) {
        match ev {
            HotkeyEvent::Bound(key) => {
                if let Some(ui) = self.ui() {
                    ui.set_ptt_key(key.as_str().into());
                }
                if key.is_empty() {
                    self.set_status("Назначьте клавишу push-to-talk в открывшемся окне");
                }
            }
            HotkeyEvent::Pressed => {
                self.ptt_pressed = true;
                self.apply_mic();
            }
            HotkeyEvent::Released => {
                self.ptt_pressed = false;
                // Выключаем микрофон с небольшой задержкой, если клавишу не нажали снова
                Timer::single_shot(PTT_RELEASE_TAIL, || post(|app| app.apply_mic()));
            }
            HotkeyEvent::Unavailable(e) => self.set_status(format!("Push-to-talk недоступен: {e}")),
        }
    }

    // ------------------------------------------------------------ настройки

    fn open_settings(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (inputs, outputs) = feather_core::list_devices();
        let with_default = |list: &[String]| -> Vec<SharedString> {
            std::iter::once(DEFAULT_DEVICE.into()).chain(list.iter().map(|d| d.as_str().into())).collect()
        };
        let index = |list: &[String], want: &Option<String>| -> i32 {
            want.as_ref().and_then(|w| list.iter().position(|d| d == w)).map_or(0, |i| i as i32 + 1)
        };
        ui.set_input_devices(ModelRc::new(VecModel::from(with_default(&inputs))));
        ui.set_output_devices(ModelRc::new(VecModel::from(with_default(&outputs))));
        ui.set_set_input(index(&inputs, &self.settings.input_device));
        ui.set_set_output(index(&outputs, &self.settings.output_device));
        ui.set_set_ptt(self.settings.push_to_talk);
        ui.set_set_noise(self.settings.noise_suppression);
        ui.set_set_echo(self.settings.echo_cancellation);
        ui.set_set_economy(self.settings.economy_mode);
        self.device_lists = (inputs, outputs);
        ui.set_dialog("settings".into());
    }

    fn save_settings(&mut self) {
        let Some(ui) = self.ui() else { return };
        let pick = |list: &[String], i: i32| -> Option<String> { (i > 0).then(|| list.get(i as usize - 1).cloned()).flatten() };
        let economy_before = self.settings.economy_mode;
        self.settings = Settings {
            push_to_talk: ui.get_set_ptt(),
            input_device: pick(&self.device_lists.0, ui.get_set_input()),
            output_device: pick(&self.device_lists.1, ui.get_set_output()),
            noise_suppression: ui.get_set_noise(),
            echo_cancellation: ui.get_set_echo(),
            economy_mode: ui.get_set_economy(),
        };
        settings::save(&self.settings);
        ui.set_dialog(SharedString::new());
        self.update_hotkey();
        self.apply_mic();
        if self.settings.economy_mode != economy_before {
            self.set_status("Режим отрисовки сменится после перезапуска Feather");
        }
    }

    fn leave_voice(&mut self) {
        let was = self.voice.take();
        if let Some(session) = was.and_then(|v| v.session) {
            self.rt.spawn(session.leave());
            self.send_voice_state();
        }
        self.refresh_voice();
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
        if kind == "delete" {
            ui.set_dialog(SharedString::new());
            self.delete_confirmed();
            return;
        }
        if kind == "channel" {
            let (Some(guild), parent) = (self.guild, self.pending_parent.take()) else { return };
            if value.is_empty() {
                return;
            }
            let ch_kind = if ui.get_dialog_switch() { Kind::Voice } else { Kind::Text };
            ui.set_dialog(SharedString::new());
            let api = self.api.clone();
            self.rt.spawn(async move {
                // Сам канал появится в списке событием CHANNEL_CREATE
                if let Err(e) = api.create_channel(guild, &value, ch_kind, parent).await {
                    post(move |app| app.set_status(format!("Канал не создан: {e}")));
                }
            });
            return;
        }
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
        self.leave_voice();
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

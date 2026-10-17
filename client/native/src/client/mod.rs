//! Настоящий клиент: вход, серверы, каналы, сообщения в реальном времени.
//!
//! Потоки: интерфейс Slint живёт в главном потоке, сеть — в tokio (`feather-core`).
//! Состояние интерфейса (`App`) лежит в thread_local главного потока; фоновые задачи
//! возвращают результаты через `post`, который выполняет замыкание в потоке интерфейса.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use feather_core::voice::Cue;
use feather_core::{Api, GatewayEvent, GatewayHandle, HotkeyEvent, HotkeyHandle, Notifier, VoiceEvent, VoiceOptions, VoiceSession};
use feather_protocol::{
    AdminUser, Attachment, Channel, ChannelKind as Kind, ClientOp, DmChannel, Event, Guild, GuildMember, Message, OverwriteKind, Reaction, Role,
    RoleRequest, SendMessageRequest, Snowflake, User, VoiceState, perms,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, VecModel, Weak};
use tokio::runtime::Handle;

use crate::session::{self, Session};
use crate::settings::{self, Settings};
use crate::util::{color, initial, time_label, user_color};
use crate::{AppWindow, ChannelItem, ChannelKind, GuildItem, ListRow, Member, MentionItem, Msg, PermItem, ReactionChip, RoleItem, RoleToggle, VoiceMember};

mod account;
mod extras;
mod manage;
mod screen;
pub mod render;

/// Страница релизов: куда ведёт «Вышла новая версия»
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Пусто: адрес Feather каждому сообщает тот, кто зовёт (в поле — пример)
const DEFAULT_SERVER: &str = "";
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
        .or_else(|| std::env::var("COMPUTERNAME").ok()) // Windows
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

    // Переключились на регистрацию — узнаём у сервера Feather, кто может регистрироваться
    {
        let weak = ui.as_weak();
        let rt = rt.clone();
        ui.on_login_mode_changed(move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.set_login_error(SharedString::new());
            ui.set_registration_hint(SharedString::new());
            if !ui.get_register_mode() || ui.get_server_url().trim().is_empty() {
                return;
            }
            let api = Api::new(ui.get_server_url().as_str());
            let weak = weak.clone();
            rt.spawn(async move {
                let hint = match api.instance().await.map(|i| i.registration) {
                    Ok(feather_protocol::RegistrationMode::Open) => "Код приглашения необязателен: с ним вы сразу попадёте в сообщество.",
                    Ok(feather_protocol::RegistrationMode::Invite) => "На этом Feather регистрация только по приглашению: нужен код.",
                    Ok(feather_protocol::RegistrationMode::Closed) => "Регистрация на этом Feather закрыта.",
                    Err(_) => "",
                };
                post_ui(weak, move |ui| ui.set_registration_hint(hint.into()));
            });
        });
    }

    // Для снимков интерфейса в автотестах: сразу экран регистрации
    if std::env::var_os("FEATHER_TEST_REGISTER").is_some() {
        ui.set_register_mode(true);
        ui.invoke_login_mode_changed();
    }

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
    let invite = ui.get_login_invite().trim().to_owned();
    let totp = ui.get_login_totp().trim().to_owned();
    if server.trim().is_empty() {
        ui.set_login_error("Введите адрес Feather — его даёт владелец сервера".into());
        return;
    }
    if name.trim().is_empty() || password.is_empty() {
        ui.set_login_error("Введите имя и пароль".into());
        return;
    }
    if register {
        if password != ui.get_login_password2().as_str() {
            ui.set_login_error("Пароли не совпадают".into());
            return;
        }
        if password.chars().count() < 8 {
            ui.set_login_error("Пароль — от 8 символов".into());
            return;
        }
    }
    ui.set_login_busy(true);
    ui.set_login_error(SharedString::new());
    let rt2 = rt.clone();
    rt.spawn(async move {
        let api = Api::new(&server);
        let result = async {
            if register {
                api.register(name.trim(), &password, (!invite.is_empty()).then_some(invite.as_str())).await?;
            }
            api.login(name.trim(), &password, &device_name(), (!totp.is_empty()).then_some(totp.as_str())).await
        }
        .await;
        post_ui(weak, move |ui| {
            ui.set_login_busy(false);
            match result {
                Ok(resp) => {
                    ui.set_login_password(SharedString::new());
                    ui.set_login_password2(SharedString::new());
                    ui.set_login_invite(SharedString::new());
                    ui.set_login_totp(SharedString::new());
                    ui.set_login_need_totp(false);
                    ui.set_register_mode(false);
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
                Err(e) => {
                    // Пароль верный, нужен второй фактор: показываем поле кода
                    if e.code() == Some("totp_required") {
                        ui.set_login_need_totp(true);
                    }
                    ui.set_login_error(e.to_string().into());
                }
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
    ui.on_attach(|| post(|app| app.attach()));
    ui.on_remove_pending(|i| post(move |app| app.remove_pending(i as usize)));
    ui.on_open_image(|i| post(move |app| app.open_image(i as usize)));
    ui.on_open_files(|i| post(move |app| app.open_files(i as usize)));
    ui.on_load_older(|| post(|app| app.load_older()));
    ui.on_toggle_mute(|| post(|app| app.toggle_mute()));
    ui.on_open_settings(|| post(|app| app.open_settings()));
    ui.on_save_settings(|| post(|app| app.save_settings()));
    ui.on_configure_ptt(|| post(|app| {
        if let Some(h) = &app.hotkey {
            h.configure();
            // На Windows клавишу ловит сам Feather: подсказываем, что делать
            #[cfg(windows)]
            if let Some(ui) = app.ui() {
                ui.set_ptt_key("нажмите клавишу или кнопку мыши…".into());
            }
        }
    }));
    ui.on_toggle_deafen(|| post(|app| app.toggle_deafen()));
    ui.on_leave_voice(|| post(|app| app.leave_voice()));
    ui.on_logout(|| post(|app| app.logout()));
    ui.on_invite(|| post(|app| app.invite()));
    ui.on_add_channel(|i| post(move |app| app.ask_add_channel(i as usize)));
    ui.on_select_home(|| post(|app| app.select_home(None)));
    ui.on_message_member(|i| post(move |app| app.message_member(i as usize)));
    ui.on_kick_member(|i| post(move |app| app.ask_kick(i as usize)));
    ui.on_open_roles(|| post(|app| app.open_roles()));
    ui.on_select_role(|i| post(move |app| app.select_role(i as usize)));
    ui.on_create_role(|| post(|app| app.create_role()));
    ui.on_save_role(|| post(|app| app.save_role()));
    ui.on_delete_role(|| post(|app| app.delete_role()));
    ui.on_pick_role_color(|i| post(move |app| app.pick_role_color(i as usize)));
    ui.on_member_roles(|i| post(move |app| app.open_member_roles(i as usize)));
    ui.on_save_member_roles(|| post(|app| app.save_member_roles()));

    ui.on_ban_member(|i| post(move |app| app.ask_ban(i as usize)));
    ui.on_reply_message(|i| post(move |app| app.start_reply(i as usize)));
    ui.on_cancel_reply(|| post(|app| app.cancel_reply()));
    ui.on_react_message(|i| post(move |app| app.open_reaction_picker(i as usize)));
    ui.on_toggle_reaction(|i, e| post(move |app| app.toggle_reaction(i as usize, e.to_string())));
    ui.on_emoji_picked(|e| post(move |app| app.emoji_picked(e.to_string())));
    ui.on_pin_message(|i| post(move |app| app.toggle_pin(i as usize)));
    ui.on_thread_message(|i| post(move |app| app.ask_thread(i as usize)));
    ui.on_open_thread(|i| post(move |app| app.open_thread(i as usize)));
    ui.on_copy_message(|i| post(move |app| app.copy_message(i as usize)));
    ui.on_open_link(|url| {
        feather_core::files::open_url(&url);
    });
    ui.on_open_embed(|i| post(move |app| app.open_embed(i as usize)));
    ui.on_composer_changed(|t| post(move |app| app.composer_changed(t.to_string())));
    ui.on_pick_mention(|i| post(move |app| app.pick_mention(i as usize)));
    ui.on_paste_image(|| {
        // Синхронно: решаем, перехватывать ли Ctrl+V. Картинку читаем сразу (буфер меняется)
        match feather_core::files::clipboard_image_png() {
            Some(png) => {
                post(move |app| app.attach_bytes("скриншот.png".into(), png, "image/png"));
                true
            }
            None => false,
        }
    });
    ui.on_open_pins(|| post(|app| app.open_pins()));
    ui.on_open_search(|| post(|app| app.open_search()));
    ui.on_list_search(|| post(|app| app.run_search()));
    ui.on_list_open(|i| post(move |app| app.list_open(i as usize)));
    ui.on_list_action(|i| post(move |app| app.list_action(i as usize)));
    ui.on_voice_member(|id| post(move |app| app.open_volume(id.to_string())));
    ui.on_toggle_screen(|| post(|app| app.toggle_screen()));
    ui.on_start_screen(|| post(|app| app.start_screen()));
    ui.on_watch_screen(|id| post(move |app| app.watch_screen(id.to_string())));
    ui.on_close_screen(|| post(|app| app.close_screen()));
    ui.on_toggle_fullscreen(|| post(|app| app.toggle_fullscreen()));
    ui.on_volume_changed(|v| post(move |app| app.volume_changed(v)));
    ui.on_open_update(|| post(|app| app.open_update()));
    ui.on_channel_settings(|i| post(move |app| app.open_channel_settings(i as usize)));
    ui.on_ch_save(|| post(|app| app.save_channel_settings()));
    ui.on_ch_delete(|| post(|app| app.delete_channel()));
    ui.on_open_guild_settings(|| post(|app| app.open_guild_settings()));
    ui.on_guild_tab_changed(|i| post(move |app| app.guild_tab_changed(i as usize)));
    ui.on_guild_save_name(|| post(|app| app.guild_save_name()));
    ui.on_guild_transfer(|| post(|app| app.guild_transfer()));
    ui.on_guild_delete(|| post(|app| app.guild_delete()));
    ui.on_guild_leave(|| post(|app| app.guild_leave()));
    ui.on_guild_unban(|i| post(move |app| app.guild_unban(i as usize)));
    ui.on_open_account(|| post(|app| app.open_account()));
    ui.on_account_save_profile(|| post(|app| app.account_save_profile()));
    ui.on_account_pick_avatar(|| post(|app| app.account_pick_avatar()));
    ui.on_account_remove_avatar(|| post(|app| app.account_remove_avatar()));
    ui.on_account_change_password(|| post(|app| app.account_change_password()));
    ui.on_account_start_totp(|| post(|app| app.account_start_totp()));
    ui.on_account_enable_totp(|| post(|app| app.account_enable_totp()));
    ui.on_account_disable_totp(|| post(|app| app.account_disable_totp()));
    ui.on_account_delete(|| post(|app| app.account_delete()));
    ui.on_open_admin(|| post(|app| app.open_admin()));
    ui.on_admin_set_registration(|i| post(move |app| app.admin_set_registration(i)));
    ui.on_admin_reset_password(|i| post(move |app| app.admin_reset_password(i as usize)));
    ui.on_admin_delete_user(|i| post(move |app| app.admin_ask_delete(i as usize)));

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
    // Ветки — сразу под своим каналом, новые сверху
    let with_threads = |c: &Channel, out: &mut Vec<Channel>| {
        out.push(c.clone());
        let mut threads: Vec<&Channel> = cs.iter().filter(|t| t.kind == Kind::Thread && t.parent_id == Some(c.id)).collect();
        threads.sort_by_key(|t| std::cmp::Reverse(t.id));
        out.extend(threads.into_iter().cloned());
    };
    let mut out = Vec::new();
    // Без категории — и те, чья категория мне не видна (закрыта, а сам канал открыт)
    let orphan = |c: &Channel| c.parent_id.is_none_or(|p| !cs.iter().any(|x| x.id == p));
    for c in cs.iter().filter(|c| orphan(c) && !matches!(c.kind, Kind::Category | Kind::Thread)) {
        with_threads(c, &mut out);
    }
    for cat in cs.iter().filter(|c| c.kind == Kind::Category) {
        out.push(cat.clone());
        for c in cs.iter().filter(|c| c.parent_id == Some(cat.id) && c.kind != Kind::Thread) {
            with_threads(c, &mut out);
        }
    }
    out
}

fn msg_row(prev: Option<&Message>, m: &Message, me: Snowflake, moderator: bool) -> Msg {
    let ts = m.id.timestamp_ms();
    let head = prev.is_none_or(|p| p.author.id != m.author.id || ts.saturating_sub(p.id.timestamp_ms()) > GROUP_GAP_MS);
    let name = m.author.shown_name();
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
        mention: false,
        pinned: m.pinned,
        ..Default::default()
    }
}

/// Упомянут ли пользователь в тексте: «@имя» без учёта регистра, целым словом
fn mentions(text: &str, username: &str) -> bool {
    render::contains_mention(text, username)
}

/// Реакции сообщения: поставить/убрать (для событий шлюза и мгновенного отклика)
fn apply_reaction(m: &mut Message, emoji: &str, mine: bool, add: bool) {
    match m.reactions.iter_mut().position(|r| r.emoji == emoji) {
        Some(i) => {
            let r = &mut m.reactions[i];
            if add {
                if mine && r.me {
                    return;
                }
                r.count += 1;
                r.me |= mine;
            } else {
                if mine && !r.me {
                    return;
                }
                r.count = r.count.saturating_sub(1);
                if mine {
                    r.me = false;
                }
                if r.count == 0 {
                    m.reactions.remove(i);
                }
            }
        }
        None if add => m.reactions.push(Reaction { emoji: emoji.to_owned(), count: 1, me: mine }),
        None => {}
    }
}

/// Первая картинка с превью среди вложений
fn first_image(m: &Message) -> Option<&Attachment> {
    m.attachments.iter().find(|a| a.has_thumb)
}

/// Размер превью в ленте: вписать в 400×300, сохраняя пропорции
fn preview_size(a: &Attachment) -> (f32, f32) {
    let (w, h) = (a.width.unwrap_or(400) as f32, a.height.unwrap_or(300) as f32);
    let k = (400.0 / w).min(300.0 / h).min(1.0);
    ((w * k).max(40.0), (h * k).max(40.0))
}

/// Сколько превью держать в памяти (каждое ≤ 400×300 RGB ≈ 360 КБ)
const THUMB_CACHE: usize = 80;
const MAX_UPLOAD: u64 = 25 * 1024 * 1024;

/// Файл, прикреплённый к ещё не отправленному сообщению
struct PendingFile {
    name: String,
    size: u64,
    attachment: Option<Attachment>,
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
    stats: feather_core::voice::VoiceStats,
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
        volumes: s.volumes.clone(),
    }
}

/// Цвета ролей (порядок как в окне «Роли»)
const ROLE_COLORS: [u32; 8] = [0x99aab5, 0x3ba55c, 0x6cb6ff, 0xc792ea, 0xf07178, 0xe8a14b, 0xffcb6b, 0x5ccfe6];

/// Права в окне «Роли»: бит, название, пояснение
const PERM_LIST: &[(u64, &str, &str)] = &[
    (perms::ADMINISTRATOR, "Администратор", "Все права. Выдавайте только тем, кому доверяете как себе"),
    (perms::MANAGE_CHANNELS, "Управлять каналами", "Создавать каналы и категории"),
    (perms::MANAGE_ROLES, "Управлять ролями", "Создавать роли ниже своей и выдавать их"),
    (perms::KICK_MEMBERS, "Выгонять", "Удалять участников с сервера"),
    (perms::BAN_MEMBERS, "Банить", "Выгонять без права вернуться"),
    (perms::MANAGE_MESSAGES, "Удалять сообщения", "Удалять чужие сообщения"),
    (perms::CREATE_INVITE, "Приглашать", "Создавать коды приглашения"),
    (perms::SEND_MESSAGES, "Писать сообщения", "В текстовых каналах"),
    (perms::VOICE_CONNECT, "Заходить в голосовые", "Слушать голосовые каналы"),
    (perms::VOICE_SPEAK, "Говорить", "Включать микрофон в голосовых каналах"),
    (perms::VIEW_CHANNEL, "Видеть каналы", "Без этого права видны только каналы, открытые роли отдельно"),
    (perms::MANAGE_GUILD, "Управлять сообществом", "Переименовывать сообщество, читать журнал модерации"),
];

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
    /// Участники в том порядке, в каком показаны (для кнопок «Написать», «Выгнать»)
    members_view: Vec<User>,
    /// Роли открытого сервера и роли участников
    roles: Vec<Role>,
    member_roles: HashMap<Snowflake, Vec<Snowflake>>,
    /// Кого собираемся выгнать (ждём подтверждения)
    pending_kick: Option<Snowflake>,
    /// Окно «Роли»: выбранная роль и выбранный цвет
    role_selected: Option<Snowflake>,
    role_color: usize,
    /// Превью картинок: кэш (с порядком для вытеснения) и что сейчас грузится
    thumbs: HashMap<Snowflake, slint::Image>,
    thumb_order: VecDeque<Snowflake>,
    thumbs_loading: HashSet<Snowflake>,
    pending_files: Vec<PendingFile>,
    /// Окно «Роли участника»: участник и роли, показанные в окне
    roles_target: Option<Snowflake>,
    roles_offered: Vec<Snowflake>,
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
    notifier: Option<Notifier>,
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
    /// Я администратор сервера Feather
    admin: bool,
    /// Аватары: пользователь → (версия, картинка); что сейчас грузится
    avatars: HashMap<Snowflake, (Snowflake, slint::Image)>,
    avatars_loading: HashSet<(Snowflake, Snowflake)>,
    /// Отвечаем на это сообщение
    reply_to: Option<Snowflake>,
    /// Эмодзи из панели пойдёт реакцией на это сообщение (None — в поле ввода)
    reacting_to: Option<Snowflake>,
    /// Из этого сообщения создаётся ветка
    pending_thread: Option<Snowflake>,
    pending_ban: Option<Snowflake>,
    /// Настройки канала: какой канал и какие роли показаны в окне
    settings_channel: Option<Snowflake>,
    settings_roles: Vec<Snowflake>,
    /// Окно громкости: чья
    volume_target: Option<Snowflake>,
    /// Окно-список (поиск или закреплённые) и сообщения в нём
    list_kind: extras::ListKind,
    list_messages: Vec<Message>,
    /// Настройки сообщества: вкладки, кандидаты в новые владельцы, забаненные
    guild_tabs: Vec<manage::GuildTab>,
    owner_candidates: Vec<User>,
    bans: Vec<User>,
    /// Панель администратора
    admin_users: Vec<AdminUser>,
    pending_user_delete: Option<Snowflake>,
    /// Подсказки @упоминаний: кого предлагаем
    mention_candidates: Vec<User>,
    /// Страница новой версии
    update_url: Option<String>,
    /// Показ экрана: кто сейчас в эфире, кого смотрю, опрос кадров, мои источники, я показываю
    screen_live: HashSet<Snowflake>,
    watching: Option<Snowflake>,
    screen_timer: Timer,
    screen_sources: Vec<feather_core::screen::Source>,
    sharing: bool,
}

impl App {
    fn start(ui: &AppWindow, rt: Handle, api: Api, me: User) {
        let model = Rc::new(VecModel::<Msg>::default());
        ui.set_messages(ModelRc::from(model.clone()));
        ui.set_me_name(me.shown_name().into());
        ui.set_me_initial(initial(me.shown_name()));
        ui.set_me_tint(user_color(me.id));
        ui.set_me_has_avatar(false);
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
            roles: Vec::new(),
            member_roles: HashMap::new(),
            pending_kick: None,
            role_selected: None,
            role_color: 0,
            roles_target: None,
            thumbs: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumbs_loading: HashSet::new(),
            pending_files: Vec::new(),
            roles_offered: Vec::new(),
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
            notifier: None,
            hotkey: None,
            ptt_pressed: false,
            editing: None,
            pending_delete: None,
            pending_parent: None,
            typing: HashMap::new(),
            last_typing_sent: None,
            typing_timer: Timer::default(),
            device_lists: (Vec::new(), Vec::new()),
            admin: false,
            avatars: HashMap::new(),
            avatars_loading: HashSet::new(),
            reply_to: None,
            reacting_to: None,
            pending_thread: None,
            pending_ban: None,
            settings_channel: None,
            settings_roles: Vec::new(),
            volume_target: None,
            list_kind: extras::ListKind::None,
            list_messages: Vec::new(),
            guild_tabs: Vec::new(),
            owner_candidates: Vec::new(),
            bans: Vec::new(),
            admin_users: Vec::new(),
            pending_user_delete: None,
            mention_candidates: Vec::new(),
            update_url: None,
            screen_live: HashSet::new(),
            watching: None,
            screen_timer: Timer::default(),
            screen_sources: Vec::new(),
            sharing: false,
        };
        APP.with_borrow_mut(|slot| *slot = Some(app));
        APP.with_borrow_mut(|slot| {
            if let Some(app) = slot.as_mut() {
                app.notifier = {
                    let _guard = app.rt.enter();
                    Some(Notifier::spawn(APP_ID))
                };
                app.update_hotkey();
                app.refresh_me();
                app.check_update();
                // Для снимков интерфейса в автотестах
                if std::env::var_os("FEATHER_OPEN_SETTINGS").is_some() {
                    app.open_settings();
                }
                if std::env::var_os("FEATHER_OPEN_ACCOUNT").is_some() {
                    app.open_account();
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
        ui.set_can_invite(self.can(perms::CREATE_INVITE));
        ui.set_can_manage_roles(!self.dm_mode && self.can(perms::MANAGE_ROLES));
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
                    Kind::Thread => ChannelKind::Thread,
                },
                active: Some(c.id) == self.channel || self.voice.as_ref().is_some_and(|v| v.channel == c.id),
                unread: self.unread.contains_key(&c.id),
                voice_members: self.voice_members(c.id),
                can_add: c.kind == Kind::Category && self.can(perms::MANAGE_CHANNELS),
                can_manage: c.kind != Kind::Dm && self.can(perms::MANAGE_CHANNELS),
                private: Self::is_private(c),
            })
            .collect();
        ui.set_channels(ModelRc::new(VecModel::from(items)));
        let current = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id));
        ui.set_channel_name(current.map(|c| c.name.clone()).unwrap_or_default().into());
        ui.set_in_thread(current.is_some_and(|c| c.kind == Kind::Thread));
        ui.set_can_write(current.is_some_and(|c| matches!(c.kind, Kind::Text | Kind::Dm | Kind::Thread)) && self.can_send_here());
    }

    /// Закрытый канал: @everyone не видит
    fn is_private(c: &Channel) -> bool {
        c.overwrites.iter().any(|o| o.kind == OverwriteKind::Role && o.id == c.guild_id && o.deny & perms::VIEW_CHANNEL != 0)
    }

    /// Мои права в канале с учётом его настроек (как считает сервер)
    fn channel_perms(&self, c: &Channel) -> u64 {
        let base = self.my_perms();
        let source = if c.kind == Kind::Thread { c.parent_id.and_then(|p| self.channels.iter().find(|x| x.id == p)).unwrap_or(c) } else { c };
        let mine = self.member_roles.get(&self.me.id);
        let (mut everyone, mut member, mut roles) = (None, None, (0u64, 0u64));
        for o in &source.overwrites {
            match o.kind {
                OverwriteKind::Member if o.id == self.me.id => member = Some((o.allow, o.deny)),
                OverwriteKind::Role if o.id == c.guild_id => everyone = Some((o.allow, o.deny)),
                OverwriteKind::Role if mine.is_some_and(|m| m.contains(&o.id)) => {
                    roles.0 |= o.allow;
                    roles.1 |= o.deny;
                }
                _ => {}
            }
        }
        perms::apply_overwrites(base, everyone, roles, member)
    }

    /// В открытый канал можно писать (в личке — всегда)
    fn can_send_here(&self) -> bool {
        if self.dm_mode {
            return true;
        }
        let Some(c) = self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id)) else { return false };
        self.channel_perms(c) & perms::SEND_MESSAGES != 0
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
                let name = v.user.shown_name();
                let (avatar, has_avatar) = self.avatar_of(&v.user);
                VoiceMember {
                    id: v.user.id.to_string().into(),
                    name: name.into(),
                    initial: initial(name),
                    tint: user_color(v.user.id),
                    avatar,
                    has_avatar,
                    speaking: speaking.is_some_and(|s| s.contains(&v.user.id.to_string())),
                    muted: v.muted || v.deafened,
                    me: v.user.id == self.me.id,
                    volume_changed: self.settings.volumes.get(&v.user.id.to_string()).is_some_and(|g| (*g - 1.0).abs() > 0.01),
                    live: if v.user.id == self.me.id { self.sharing } else { self.screen_live.contains(&v.user.id) },
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
                ui.set_voice_sharing(self.sharing);
            }
            None => ui.set_voice_channel(SharedString::new()),
        }
        self.refresh_ping();
        self.refresh_channels();
    }

    fn refresh_ping(&self) {
        let Some(ui) = self.ui() else { return };
        let st = self.voice.as_ref().filter(|v| v.session.is_some()).map(|v| &v.stats);
        let rtt = st.and_then(|s| s.rtt_ms);
        ui.set_voice_ping(rtt.map(|r| r as i32).unwrap_or(-1));
        let mut extra = Vec::new();
        if let Some(st) = st {
            // UDP — норма; TCP и ретранслятор дают задержки, о них стоит знать
            if !st.transport.is_empty() && st.transport != "UDP" {
                extra.push(st.transport.clone());
            }
            if st.loss_pct >= 1.0 {
                extra.push(format!("потери {:.0}%", st.loss_pct));
            }
        }
        ui.set_voice_net(extra.join(" · ").into());
    }

    /// Роли сменились: перерисовать всё, что от них зависит (цвета, кнопки, права)
    fn after_roles_changed(&mut self) {
        if self.ui().is_some_and(|ui| ui.get_dialog() == "roles") {
            self.refresh_roles_dialog();
        }
        self.refresh_guilds();
        self.refresh_channels();
        self.refresh_members();
        self.rebuild_messages();
    }

    fn refresh_members(&mut self) {
        let Some(ui) = self.ui() else { return };
        let mut ms: Vec<User> = self.members.clone();
        // Старшие роли выше, внутри — по имени
        ms.sort_by_key(|u| (std::cmp::Reverse(self.top_position(u.id)), u.username.to_lowercase()));
        let (can_kick, can_ban, my_top) = (self.can(perms::KICK_MEMBERS), self.can(perms::BAN_MEMBERS), self.top_position(self.me.id));
        let can_roles = !self.dm_mode && self.can(perms::MANAGE_ROLES);
        let items: Vec<Member> = ms
            .iter()
            .map(|u| {
                let (avatar, has_avatar) = self.avatar_of(u);
                let below = u.id != self.me.id && my_top > self.top_position(u.id);
                Member {
                    name: u.shown_name().into(),
                    initial: initial(u.shown_name()),
                    tint: self.user_tint(u.id),
                    avatar,
                    has_avatar,
                    can_message: u.id != self.me.id,
                    can_kick: !self.dm_mode && can_kick && below,
                    can_ban: !self.dm_mode && can_ban && below,
                    can_roles: can_roles && (u.id == self.me.id || my_top > self.top_position(u.id)),
                }
            })
            .collect();
        self.request_avatars(ms.iter());
        self.members_view = ms;
        ui.set_members(ModelRc::new(VecModel::from(items)));
    }

    // ------------------------------------------------------------ личные сообщения

    /// Личный канал как элемент списка каналов (имя — собеседники)
    fn dm_as_channel(d: &DmChannel) -> Channel {
        let name = d.recipients.iter().map(|u| u.shown_name().to_owned()).collect::<Vec<_>>().join(", ");
        Channel { id: d.id, guild_id: Snowflake(0), kind: Kind::Dm, name, parent_id: None, position: 0, overwrites: Vec::new() }
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

    /// Мои права на открытом сервере (в личке — никаких «серверных» прав)
    fn my_perms(&self) -> u64 {
        let Some(g) = self.guild.and_then(|g| self.guilds.iter().find(|x| x.id == g)) else { return 0 };
        let mine = self.member_roles.get(&self.me.id);
        let role_perms = self.roles.iter().filter(|r| r.id == g.id || mine.is_some_and(|m| m.contains(&r.id))).map(|r| r.permissions);
        perms::resolve(g.owner_id == self.me.id, role_perms)
    }

    fn can(&self, p: u64) -> bool {
        self.my_perms() & p == p
    }

    /// Могу удалять чужие сообщения
    fn moderator(&self) -> bool {
        self.can(perms::MANAGE_MESSAGES)
    }

    /// Позиция высшей роли (владелец — выше всех)
    fn top_position(&self, user: Snowflake) -> i32 {
        if self.guild.and_then(|g| self.guilds.iter().find(|x| x.id == g)).is_some_and(|g| g.owner_id == user) {
            return i32::MAX;
        }
        let ids = self.member_roles.get(&user);
        self.roles.iter().filter(|r| ids.is_some_and(|i| i.contains(&r.id))).map(|r| r.position).max().unwrap_or(0)
    }

    /// Цвет имени: цвет высшей цветной роли, иначе цвет по ID
    fn user_tint(&self, user: Snowflake) -> slint::Color {
        let ids = self.member_roles.get(&user);
        self.roles
            .iter()
            .filter(|r| r.color != 0 && ids.is_some_and(|i| i.contains(&r.id)))
            .max_by_key(|r| r.position)
            .map_or_else(|| user_color(user), |r| color(r.color))
    }

    fn row_at(&self, i: usize) -> Msg {
        let m = &self.messages[i];
        let base = msg_row(i.checked_sub(1).map(|p| &self.messages[p]), m, self.me.id, self.moderator());
        let img = first_image(m);
        let (img_w, img_h) = img.map(preview_size).unwrap_or((0.0, 0.0));
        let cached = img.and_then(|a| self.thumbs.get(&a.id));
        let files = m
            .attachments
            .iter()
            .filter(|a| Some(a.id) != img.map(|i| i.id))
            .map(|a| format!("{} · {}", a.filename, feather_core::files::human_size(a.size)))
            .collect::<Vec<_>>()
            .join("\n");
        let (avatar, has_avatar) = self.avatar_of(&m.author);
        // Разметка: только если в тексте есть что размечать (обычный Text лучше переносит строки)
        let names = self.mention_palette();
        let plain_names: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
        let rich = render::needs_markup(&m.content, &plain_names);
        let body = if rich {
            slint::StyledText::from_markdown(&render::markup(&m.content, &names, m.edited_at.is_some())).ok()
        } else {
            None
        };
        let reply = m.reply_to_id.map(|r| match self.messages.iter().find(|x| x.id == r) {
            Some(orig) => (orig.author.shown_name().to_owned(), self.user_tint(orig.author.id), render::snippet(&orig.content, 120)),
            None => ("Сообщение".to_owned(), slint::Color::from_rgb_u8(0x8e, 0x92, 0x97), "не загружено или удалено".to_owned()),
        });
        let in_guild_text = !self.dm_mode && self.channel.and_then(|id| self.channels.iter().find(|c| c.id == id)).is_some_and(|c| c.kind == Kind::Text);
        let embed = m.embeds.first();
        // Ответ на моё сообщение — тоже обращение ко мне
        let reply_to_me = m.reply_to_id.is_some_and(|r| self.messages.iter().any(|x| x.id == r && x.author.id == self.me.id));
        Msg {
            mention: m.author.id != self.me.id && (mentions(&m.content, &self.me.username) || reply_to_me),
            tint: self.user_tint(m.author.id),
            avatar,
            has_avatar,
            rich: body.is_some(),
            body: body.unwrap_or_default(),
            reply_author: reply.as_ref().map(|r| r.0.as_str()).unwrap_or_default().into(),
            reply_tint: reply.as_ref().map(|r| r.1).unwrap_or_default(),
            reply_text: reply.as_ref().map(|r| r.2.as_str()).unwrap_or_default().into(),
            can_pin: self.dm_mode || self.moderator(),
            can_thread: in_guild_text && m.thread_id.is_none() && self.can_send_here(),
            thread_name: m
                .thread_id
                .map(|t| self.channels.iter().find(|c| c.id == t).map(|c| c.name.clone()).unwrap_or_else(|| "ветка".into()))
                .unwrap_or_default()
                .into(),
            reactions: ModelRc::new(VecModel::from(
                m.reactions.iter().map(|r| ReactionChip { emoji: r.emoji.as_str().into(), count: r.count as i32, me: r.me }).collect::<Vec<_>>(),
            )),
            embed_site: embed.and_then(|e| e.site_name.as_deref()).unwrap_or_default().into(),
            embed_title: embed.and_then(|e| e.title.as_deref()).unwrap_or_default().into(),
            embed_desc: embed.and_then(|e| e.description.as_deref()).unwrap_or_default().into(),
            has_image: img.is_some(),
            img_w,
            img_h,
            img_label: img.map(|a| a.filename.as_str()).unwrap_or_default().into(),
            img_tint: slint::Color::from_rgb_u8(0x2b, 0x2d, 0x31),
            image: cached.cloned().unwrap_or_default(),
            image_loaded: cached.is_some(),
            files: files.into(),
            ..base
        }
    }

    /// Уведомление о сообщении в неоткрытом канале: личное или упоминание
    fn maybe_notify(&self, m: &Message, dm: bool) {
        if !self.settings.notifications || m.author.id == self.me.id || Some(m.channel_id) == self.channel {
            return;
        }
        let author = m.author.shown_name();
        let title = if dm {
            author.to_owned()
        } else if mentions(&m.content, &self.me.username) {
            let channel = self.channels.iter().find(|c| c.id == m.channel_id).map(|c| c.name.as_str()).unwrap_or("канал");
            format!("{author} в #{channel}")
        } else {
            return;
        };
        if let Some(n) = &self.notifier {
            n.show(&m.channel_id.to_string(), &title, &m.content);
        }
    }

    fn rebuild_messages(&mut self) {
        let rows: Vec<Msg> = (0..self.messages.len()).map(|i| self.row_at(i)).collect();
        self.model.set_vec(rows);
        self.request_thumbs();
        let authors: Vec<User> = self.messages.iter().map(|m| m.author.clone()).collect();
        self.request_avatars(authors.iter());
    }

    /// Перерисовать одно сообщение по id
    fn refresh_message(&mut self, id: Snowflake) {
        if let Some(i) = self.messages.iter().position(|m| m.id == id) {
            self.model.set_row_data(i, self.row_at(i));
        }
    }

    /// Догрузить превью картинок, которых ещё нет в кэше
    fn request_thumbs(&mut self) {
        let wanted: Vec<Snowflake> = self
            .messages
            .iter()
            .filter_map(first_image)
            .map(|a| a.id)
            .filter(|id| !self.thumbs.contains_key(id) && !self.thumbs_loading.contains(id))
            .collect();
        for id in wanted {
            self.thumbs_loading.insert(id);
            let api = self.api.clone();
            self.rt.spawn(async move {
                // Декодируем в фоне; в поток интерфейса уходит готовый буфер пикселей
                let decoded = api.download(id, true).await.ok().and_then(|bytes| {
                    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg).ok()?.to_rgb8();
                    Some(slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(img.as_raw(), img.width(), img.height()))
                });
                post(move |app| app.thumb_loaded(id, decoded));
            });
        }
    }

    fn thumb_loaded(&mut self, id: Snowflake, buffer: Option<slint::SharedPixelBuffer<slint::Rgb8Pixel>>) {
        self.thumbs_loading.remove(&id);
        let Some(buffer) = buffer else { return };
        self.thumbs.insert(id, slint::Image::from_rgb8(buffer));
        self.thumb_order.push_back(id);
        while self.thumb_order.len() > THUMB_CACHE {
            if let Some(old) = self.thumb_order.pop_front() {
                self.thumbs.remove(&old);
            }
        }
        let stick = self.ui().is_some_and(|ui| ui.invoke_near_bottom());
        for i in 0..self.messages.len() {
            if first_image(&self.messages[i]).is_some_and(|a| a.id == id) {
                self.model.set_row_data(i, self.row_at(i));
            }
        }
        if stick {
            self.scroll_to_bottom_later();
        }
    }

    // ------------------------------------------------------------ вложения: отправка и открытие

    fn refresh_pending(&self) {
        let Some(ui) = self.ui() else { return };
        let items: Vec<SharedString> = self
            .pending_files
            .iter()
            .map(|f| match &f.attachment {
                Some(_) => format!("{} · {}", f.name, feather_core::files::human_size(f.size)).into(),
                None => format!("{} · загрузка…", f.name).into(),
            })
            .collect();
        ui.set_pending_files(ModelRc::new(VecModel::from(items)));
    }

    fn attach(&mut self) {
        let Some(channel) = self.channel else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            for path in feather_core::files::pick_files().await {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
                let size = tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
                if size == 0 || size > MAX_UPLOAD {
                    let n = name.clone();
                    post(move |app| app.set_status(format!("«{n}»: файл пустой или больше 25 МБ")));
                    continue;
                }
                let n = name.clone();
                post(move |app| {
                    app.pending_files.push(PendingFile { name: n, size, attachment: None });
                    app.refresh_pending();
                });
                let res = match tokio::fs::read(&path).await {
                    Ok(bytes) => api.upload(channel, &name, feather_core::files::content_type(&path), bytes).await,
                    Err(e) => Err(feather_core::Error::Network(e.to_string())),
                };
                post(move |app| {
                    let slot = app.pending_files.iter().position(|f| f.name == name && f.attachment.is_none());
                    match (res, slot) {
                        (Ok(a), Some(i)) => app.pending_files[i].attachment = Some(a),
                        (Err(e), Some(i)) => {
                            app.pending_files.remove(i);
                            app.set_status(format!("«{name}» не загружен: {e}"));
                        }
                        _ => {} // файл уже убрали из списка
                    }
                    app.refresh_pending();
                });
            }
        });
    }

    fn remove_pending(&mut self, i: usize) {
        if i < self.pending_files.len() {
            self.pending_files.remove(i);
            self.refresh_pending();
        }
    }

    /// Скачать вложения в «Загрузки/Feather» и открыть системной программой
    fn open_attachments(&self, list: Vec<Attachment>) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            let dir = feather_core::files::downloads_dir();
            if tokio::fs::create_dir_all(&dir).await.is_err() {
                return;
            }
            let mut last = None;
            for a in list {
                // id в имени: файлы с одинаковыми названиями не затрут друг друга
                let path = dir.join(format!("{}-{}", a.id, a.filename));
                if tokio::fs::metadata(&path).await.is_err() {
                    match api.download(a.id, false).await {
                        Ok(bytes) => {
                            let _ = tokio::fs::write(&path, bytes).await;
                        }
                        Err(e) => {
                            post(move |app| app.set_status(format!("Не удалось скачать: {e}")));
                            continue;
                        }
                    }
                }
                last = Some(path);
            }
            if let Some(p) = last {
                feather_core::files::open_path(&p);
            }
        });
    }

    fn open_image(&self, i: usize) {
        if let Some(a) = self.messages.get(i).and_then(first_image) {
            self.open_attachments(vec![a.clone()]);
        }
    }

    fn open_files(&self, i: usize) {
        if let Some(m) = self.messages.get(i) {
            let img = first_image(m).map(|a| a.id);
            self.open_attachments(m.attachments.iter().filter(|a| Some(a.id) != img).cloned().collect());
        }
    }

    fn scroll_to_bottom_later(&self) {
        // ListView узнаёт настоящие высоты строк (картинки, перенос текста) только после
        // отрисовки, поэтому прокручиваем дважды: сразу после кадра и после уточнения высот
        for delay in [20, 150, 450] {
            let weak = self.ui.clone();
            Timer::single_shot(Duration::from_millis(delay), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_scroll_to_bottom();
                }
            });
        }
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
            let res = tokio::try_join!(api.channels(id), api.members(id), api.roles(id));
            post(move |app| {
                if app.load_gen != generation {
                    return;
                }
                match res {
                    Ok((channels, members, roles)) => {
                        app.channels = ordered(channels);
                        app.roles = roles;
                        app.member_roles = members.iter().map(|m| (m.user.id, m.roles.clone())).collect();
                        app.members = members.into_iter().map(|m: GuildMember| m.user).collect();
                        app.refresh_guilds();
                        // Для снимков интерфейса в автотестах
                        if std::env::var_os("FEATHER_TEST_ROLES").is_some() {
                            app.open_roles();
                        }
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
                        // Для снимков интерфейса в автотестах: открыть окно
                        match std::env::var("FEATHER_TEST_DIALOG").as_deref() {
                            Ok("guild") => app.open_guild_settings(),
                            Ok("audit") => {
                                app.open_guild_settings();
                                let last = app.guild_tabs.len().saturating_sub(1);
                                if let Some(ui) = app.ui() {
                                    ui.set_guild_tab(last as i32);
                                }
                                app.guild_tab_changed(last);
                            }
                            Ok("admin") => app.open_admin(),
                            Ok("channel") => {
                                if let Some(i) = app.channels.iter().position(|c| c.kind == Kind::Text) {
                                    app.open_channel_settings(i);
                                }
                            }
                            Ok("emoji") => {
                                if let Some(ui) = app.ui() {
                                    ui.set_emoji_for(-1);
                                }
                            }
                            _ => {}
                        }
                    }
                    Err(e) => app.set_status(format!("Не удалось загрузить сообщество: {e}")),
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
            Kind::Text | Kind::Dm | Kind::Thread => {}
        }
        self.set_status("");
        self.typing.clear();
        self.refresh_typing();
        self.cancel_edit();
        self.cancel_reply();
        self.close_emoji();
        self.pending_files.clear();
        self.refresh_pending();
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
                app.request_thumbs();
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
            self.request_thumbs();
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
        // usize::MAX (из -1) — последнее своё сообщение: стрелка вверх в пустом поле
        let i = if i == usize::MAX { self.messages.iter().rposition(|m| m.author.id == self.me.id).unwrap_or(usize::MAX) } else { i };
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

    // ------------------------------------------------------------ роли

    /// Роли по старшинству: от высшей к @everyone
    fn roles_sorted(&self) -> Vec<Role> {
        let mut rs = self.roles.clone();
        rs.sort_by_key(|r| (std::cmp::Reverse(r.position), r.id));
        rs
    }

    fn can_edit_role(&self, r: &Role) -> bool {
        self.can(perms::MANAGE_ROLES) && (self.guild == Some(r.id) || self.top_position(self.me.id) > r.position)
    }

    fn open_roles(&mut self) {
        self.role_selected = self.roles_sorted().first().map(|r| r.id);
        self.refresh_roles_dialog();
        if let Some(ui) = self.ui() {
            ui.set_dialog("roles".into());
        }
    }

    fn refresh_roles_dialog(&mut self) {
        let Some(ui) = self.ui() else { return };
        let rs = self.roles_sorted();
        if !rs.iter().any(|r| Some(r.id) == self.role_selected) {
            self.role_selected = rs.first().map(|r| r.id);
        }
        let items: Vec<RoleItem> = rs
            .iter()
            .map(|r| RoleItem {
                name: r.name.as_str().into(),
                tint: if r.color == 0 { slint::Color::from_rgb_u8(0x99, 0xaa, 0xb5) } else { color(r.color) },
                selected: Some(r.id) == self.role_selected,
                editable: self.can_edit_role(r),
            })
            .collect();
        ui.set_role_items(ModelRc::new(VecModel::from(items)));
        if let Some(r) = rs.iter().find(|r| Some(r.id) == self.role_selected) {
            let perms_items: Vec<PermItem> = PERM_LIST
                .iter()
                .map(|(bit, title, hint)| PermItem { title: (*title).into(), hint: (*hint).into(), checked: r.permissions & bit != 0 })
                .collect();
            ui.set_perm_items(ModelRc::new(VecModel::from(perms_items)));
            ui.set_role_name(r.name.as_str().into());
            self.role_color = ROLE_COLORS.iter().position(|c| *c == r.color).unwrap_or(0);
            ui.set_role_color_index(self.role_color as i32);
            ui.set_role_is_everyone(self.guild == Some(r.id));
            ui.set_role_editable(self.can_edit_role(r));
        }
    }

    fn select_role(&mut self, i: usize) {
        self.role_selected = self.roles_sorted().get(i).map(|r| r.id);
        self.refresh_roles_dialog();
    }

    fn pick_role_color(&mut self, i: usize) {
        self.role_color = i.min(ROLE_COLORS.len() - 1);
        if let Some(ui) = self.ui() {
            ui.set_role_color_index(self.role_color as i32);
        }
    }

    fn create_role(&mut self) {
        let Some(guild) = self.guild else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let req = RoleRequest { name: Some("новая роль".into()), color: Some(ROLE_COLORS[2]), permissions: Some(0), position: None };
            let res = api.create_role(guild, &req).await;
            post(move |app| match res {
                Ok(r) => {
                    if !app.roles.iter().any(|x| x.id == r.id) {
                        app.roles.push(r.clone());
                    }
                    app.role_selected = Some(r.id);
                    app.refresh_roles_dialog();
                }
                Err(e) => app.set_status(format!("Роль не создана: {e}")),
            });
        });
    }

    fn save_role(&mut self) {
        let (Some(guild), Some(role), Some(ui)) = (self.guild, self.role_selected, self.ui()) else { return };
        let items = ui.get_perm_items();
        let bits = (0..items.row_count())
            .filter_map(|i| items.row_data(i))
            .zip(PERM_LIST)
            .filter(|(item, _)| item.checked)
            .fold(0u64, |acc, (_, (bit, _, _))| acc | bit);
        let everyone = guild == role;
        let req = RoleRequest {
            name: (!everyone).then(|| ui.get_role_name().to_string()),
            color: (!everyone).then(|| ROLE_COLORS[self.role_color]),
            permissions: Some(bits),
            position: None,
        };
        let api = self.api.clone();
        self.rt.spawn(async move {
            match api.update_role(guild, role, &req).await {
                Ok(_) => post(|app| app.set_status("Роль сохранена")),
                Err(e) => post(move |app| app.set_status(format!("Роль не сохранена: {e}"))),
            }
        });
    }

    fn delete_role(&mut self) {
        let (Some(guild), Some(role)) = (self.guild, self.role_selected) else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.delete_role(guild, role).await {
                post(move |app| app.set_status(format!("Роль не удалена: {e}")));
            }
        });
    }

    fn open_member_roles(&mut self, i: usize) {
        let Some(u) = self.members_view.get(i).cloned() else { return };
        let my_top = self.top_position(self.me.id);
        let has = self.member_roles.get(&u.id).cloned().unwrap_or_default();
        let offered: Vec<Role> = self.roles_sorted().into_iter().filter(|r| Some(r.id) != self.guild && my_top > r.position).collect();
        let items: Vec<PermItem> = offered
            .iter()
            .map(|r| PermItem { title: r.name.as_str().into(), hint: SharedString::new(), checked: has.contains(&r.id) })
            .collect();
        self.roles_target = Some(u.id);
        self.roles_offered = offered.iter().map(|r| r.id).collect();
        if let Some(ui) = self.ui() {
            ui.set_roles_member_name(u.username.as_str().into());
            ui.set_member_role_items(ModelRc::new(VecModel::from(items)));
            ui.set_dialog("member-roles".into());
        }
    }

    fn save_member_roles(&mut self) {
        let (Some(guild), Some(user), Some(ui)) = (self.guild, self.roles_target, self.ui()) else { return };
        ui.set_dialog(SharedString::new());
        let items = ui.get_member_role_items();
        let has = self.member_roles.get(&user).cloned().unwrap_or_default();
        let changes: Vec<(Snowflake, bool)> = self
            .roles_offered
            .iter()
            .enumerate()
            .filter_map(|(i, role)| items.row_data(i).map(|it| (*role, it.checked)))
            .filter(|(role, want)| has.contains(role) != *want)
            .collect();
        let api = self.api.clone();
        self.rt.spawn(async move {
            for (role, add) in changes {
                if let Err(e) = api.set_member_role(guild, user, role, add).await {
                    post(move |app| app.set_status(format!("Роль не изменена: {e}")));
                    break;
                }
            }
        });
    }

    fn ask_kick(&mut self, i: usize) {
        let Some(u) = self.members_view.get(i).cloned() else { return };
        self.pending_kick = Some(u.id);
        if let Some(ui) = self.ui() {
            ui.set_dialog_value(u.username.as_str().into());
            ui.set_dialog("kick".into());
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
        if text.is_empty() && self.pending_files.is_empty() {
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
        if self.pending_files.iter().any(|f| f.attachment.is_none()) {
            self.set_status("Дождитесь окончания загрузки файлов");
            if let Some(ui) = self.ui() {
                ui.set_composer_text(text.as_str().into()); // текст не теряем
            }
            return;
        }
        let ids: Vec<Snowflake> = self.pending_files.drain(..).filter_map(|f| f.attachment.map(|a| a.id)).collect();
        self.refresh_pending();
        let nonce = format!("{:x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        let req = SendMessageRequest { content: text, reply_to_id: self.reply_to.take(), nonce: Some(nonce), attachment_ids: ids };
        self.cancel_reply();
        self.close_emoji();
        self.mention_candidates.clear();
        self.refresh_mentions();
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.send_full(channel, &req).await;
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
            GatewayEvent::Ready { user, guilds, unread, voice_states, admin } => {
                self.me = user;
                self.admin = admin;
                self.refresh_me();
                self.guilds = guilds;
                self.voice_states = voice_states.into_iter().filter(|v| v.channel_id.is_some()).map(|v| (v.user.id, v)).collect();
                // Переподключение шлюза: сервер забыл наше голосовое состояние вместе со старой сессией
                self.send_voice_state();
                self.unread = unread.into_iter().map(|u| (u.channel_id, u.guild_id)).collect();
                if let Some(c) = self.channel {
                    self.unread.remove(&c);
                }
                self.set_status("");
                // Список личных диалогов нужен и в режиме сервера: метки и уведомления о личке
                {
                    let api = self.api.clone();
                    self.rt.spawn(async move {
                        if let Ok(dms) = api.dms().await {
                            post(move |app| {
                                for d in dms {
                                    if !app.dms.iter().any(|x| x.id == d.id) {
                                        app.dms.push(d);
                                    }
                                }
                                app.sort_dms();
                            });
                        }
                    });
                }
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
                            self.set_status("Вы пока не состоите в сообществах: создайте своё (+) или вступите по приглашению (→)");
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
                } else if !self.dms.iter().any(|d| d.id == m.channel_id)
                    && !self.channels.iter().any(|c| c.id == m.channel_id)
                    && m.author.id != self.me.id
                {
                    // Канал неизвестен: по серверам без подписки приходит UNREAD_UPDATE, а не само
                    // сообщение, значит это личка (список диалогов ещё не загружен) — добавляем её
                    self.dms.push(DmChannel { id: m.channel_id, recipients: vec![m.author.clone()], last_message_id: Some(m.id) });
                    self.on_event(Event::MessageCreate(m));
                } else if self.dms.iter().any(|d| d.id == m.channel_id) {
                    self.maybe_notify(&m, true);
                    let d = self.dms.iter_mut().find(|d| d.id == m.channel_id).expect("проверено выше");
                    // Личное сообщение в неоткрытом диалоге: метка на «Личных сообщениях», диалог наверх
                    d.last_message_id = Some(m.id);
                    if m.author.id != self.me.id {
                        self.unread.insert(m.channel_id, None);
                    }
                    self.sort_dms();
                    self.refresh_guilds();
                    self.refresh_channels();
                } else if m.author.id != self.me.id && self.channels.iter().any(|c| c.id == m.channel_id) {
                    self.maybe_notify(&m, false);
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
                    self.member_roles.insert(user.id, Vec::new());
                    self.members.push(user);
                    self.refresh_members();
                }
            }
            Event::GuildDelete { guild_id } => {
                // Меня выгнали (или забанили)
                self.guilds.retain(|g| g.id != guild_id);
                self.unread.retain(|_, g| *g != Some(guild_id));
                if self.guild == Some(guild_id) {
                    self.guild = None;
                    if self.voice.as_ref().is_some_and(|v| self.channels.iter().any(|c| c.id == v.channel)) {
                        self.leave_voice();
                    }
                    match self.guilds.first().map(|g| g.id) {
                        Some(g) => self.select_guild(g),
                        None => self.select_home(None),
                    }
                    self.set_status("Вас удалили из сообщества");
                } else {
                    self.refresh_guilds();
                }
            }
            Event::GuildMemberRemove { guild_id, user_id } => {
                if Some(guild_id) == self.guild {
                    self.members.retain(|u| u.id != user_id);
                    self.member_roles.remove(&user_id);
                    self.refresh_members();
                }
            }
            Event::GuildMemberUpdate { guild_id, user_id, roles } => {
                if Some(guild_id) == self.guild {
                    self.member_roles.insert(user_id, roles);
                    self.after_roles_changed();
                }
            }
            Event::RoleCreate(r) | Event::RoleUpdate(r) => {
                if Some(r.guild_id) == self.guild {
                    self.roles.retain(|x| x.id != r.id);
                    self.roles.push(r);
                    self.after_roles_changed();
                }
            }
            Event::RoleDelete { guild_id, role_id } => {
                if Some(guild_id) == self.guild {
                    self.roles.retain(|x| x.id != role_id);
                    for rs in self.member_roles.values_mut() {
                        rs.retain(|r| *r != role_id);
                    }
                    self.after_roles_changed();
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
                // Звук «зашёл / вышел» — только про мой голосовой канал и не про меня
                if let Some(mine) = self.voice.as_ref().filter(|_| v.user.id != self.me.id).map(|x| x.channel) {
                    let was = self.voice_states.get(&v.user.id).and_then(|s| s.channel_id) == Some(mine);
                    let now = v.channel_id == Some(mine);
                    if now && !was {
                        self.cue(Cue::Join);
                    } else if was && !now {
                        self.cue(Cue::Leave);
                    }
                }
                if v.channel_id.is_some() {
                    self.request_avatars(std::iter::once(&v.user));
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
            Event::ChannelUpdate(c) => {
                if Some(c.guild_id) == self.guild {
                    let mut all: Vec<Channel> = std::mem::take(&mut self.channels).into_iter().filter(|x| x.id != c.id).collect();
                    all.push(c);
                    self.channels = ordered(all);
                    self.refresh_channels();
                    self.rebuild_messages(); // названия веток в сообщениях
                }
            }
            Event::ChannelDelete { guild_id, channel_id } => {
                if Some(guild_id) == self.guild {
                    self.channels.retain(|c| c.id != channel_id);
                    self.unread.remove(&channel_id);
                    if self.voice.as_ref().is_some_and(|v| v.channel == channel_id) {
                        self.leave_voice();
                    }
                    if self.channel == Some(channel_id) {
                        self.channel = None;
                        self.messages.clear();
                        self.model.set_vec(Vec::new());
                        match self.channels.iter().find(|c| c.kind == Kind::Text).cloned() {
                            Some(c) => self.select_channel(&c),
                            None => self.refresh_channels(),
                        }
                        self.set_status("Канал удалён");
                    } else {
                        self.refresh_channels();
                    }
                }
            }
            Event::ChannelsInvalidated { guild_id } => {
                if Some(guild_id) == self.guild {
                    self.reload_channels();
                }
            }
            Event::GuildUpdate(g) => {
                if let Some(x) = self.guilds.iter_mut().find(|x| x.id == g.id) {
                    *x = g;
                    self.refresh_guilds();
                    self.refresh_members();
                    self.rebuild_messages();
                }
            }
            Event::UserUpdate(u) => self.on_user_update(u),
            Event::ReactionAdd { message_id, user_id, emoji, .. } => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == message_id) {
                    apply_reaction(m, &emoji, user_id == self.me.id, true);
                    self.refresh_message(message_id);
                }
            }
            Event::ReactionRemove { message_id, user_id, emoji, .. } => {
                if let Some(m) = self.messages.iter_mut().find(|m| m.id == message_id) {
                    apply_reaction(m, &emoji, user_id == self.me.id, false);
                    self.refresh_message(message_id);
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
            stats: Default::default(),
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
                        app.cue(Cue::Join);
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
            VoiceEvent::Stats(st) => {
                // Только строка пинга: список каналов не перерисовываем
                if st != v.stats {
                    v.stats = st;
                    self.refresh_ping();
                }
                return;
            }
            VoiceEvent::Screen { identity, live } => {
                self.on_screen(identity, live);
                return;
            }
            VoiceEvent::ScreenEnded(reason) => {
                self.set_status(reason);
                return;
            }
            VoiceEvent::Disconnected(reason) => {
                self.close_screen();
                self.screen_live.clear();
                self.sharing = false;
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
        let muted = v.muted;
        self.apply_mic();
        self.refresh_voice();
        self.send_voice_state();
        self.cue(if muted { Cue::Mute } else { Cue::Unmute });
    }

    fn toggle_deafen(&mut self) {
        let Some(v) = self.voice.as_mut() else { return };
        v.deafened = !v.deafened;
        if let Some(s) = &v.session {
            // Звук до выключения: после него пользователь его уже не услышит
            if v.deafened && self.settings.voice_sounds {
                s.play_cue(Cue::Mute);
            }
            s.set_deafened(v.deafened);
            if !v.deafened && self.settings.voice_sounds {
                s.play_cue(Cue::Unmute);
            }
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
            self.hotkey = Some(feather_core::hotkey::register(APP_ID, self.settings.ptt_key.clone(), |ev| post(move |app| app.on_hotkey(ev))));
        }
    }

    fn on_hotkey(&mut self, ev: HotkeyEvent) {
        match ev {
            HotkeyEvent::Bound(key) => {
                if let Some(ui) = self.ui() {
                    ui.set_ptt_key(key.as_str().into());
                }
                // Windows: клавишу назначают в Feather — запоминаем её в настройках
                if !key.is_empty() && self.settings.ptt_key.as_deref() != Some(key.as_str()) {
                    self.settings.ptt_key = Some(key.clone());
                    settings::save(&self.settings);
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
        ui.set_set_notify(self.settings.notifications);
        ui.set_set_sounds(self.settings.voice_sounds);
        ui.set_set_updates(self.settings.check_updates);
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
            notifications: ui.get_set_notify(),
            ptt_key: self.settings.ptt_key.clone(),
            voice_sounds: ui.get_set_sounds(),
            check_updates: ui.get_set_updates(),
            volumes: self.settings.volumes.clone(),
            screen_quality: self.settings.screen_quality,
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
        self.close_screen();
        self.screen_live.clear();
        self.sharing = false;
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
        if kind == "ban" {
            ui.set_dialog(SharedString::new());
            self.ban_confirmed();
            return;
        }
        if kind == "thread" {
            if value.is_empty() {
                return;
            }
            ui.set_dialog(SharedString::new());
            self.create_thread(value);
            return;
        }
        if kind == "confirm-user" {
            self.admin_delete_confirmed();
            return;
        }
        if kind == "kick" {
            ui.set_dialog(SharedString::new());
            let (Some(guild), Some(user)) = (self.guild, self.pending_kick.take()) else { return };
            let api = self.api.clone();
            self.rt.spawn(async move {
                if let Err(e) = api.kick(guild, user).await {
                    post(move |app| app.set_status(format!("Не удалось выгнать: {e}")));
                }
            });
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
            ui.set_is_admin(false);
            ui.set_emoji_for(-2);
            ui.set_update_version(SharedString::new());
        }
        // Состояние удаляем после выхода из текущего заимствования APP
        let _ = slint::invoke_from_event_loop(|| APP.with_borrow_mut(|a| *a = None));
    }
}

#[cfg(test)]
mod tests {
    use super::mentions;

    #[test]
    fn mention_detection() {
        assert!(mentions("@Kerrigan го в рейд", "kerrigan"));
        assert!(mentions("эй, @kerrigan!", "Kerrigan"));
        assert!(!mentions("@kerrigan2 не ты", "kerrigan"), "только целое имя");
        assert!(!mentions("kerrigan без собаки", "kerrigan"));
        assert!(mentions("привет @Шторм", "шторм"));
    }
}

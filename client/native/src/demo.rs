//! Демо-режим (FEATHER_DEMO=1): 10 000 сгенерированных сообщений и «живой» чат без сервера.
//! Нужен для повторяемых замеров производительности (bench/README.md, замеры 2–3).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::mock;
use crate::util::{color, initial};
use crate::{AppWindow, ChannelItem, ChannelKind, GuildItem, Member, Msg};

const MESSAGE_COUNT: usize = 10_000;
// Входящее сообщение раз в N мс (как в прототипе на Tauri)
const LIVE_INTERVAL: Duration = Duration::from_millis(1500);
const GROUP_GAP_MS: i64 = 5 * 60_000;
// Демо: время по МСК без учёта часового пояса системы
const TZ_OFFSET_MS: i64 = 3 * 3_600_000;

fn hhmm(ts_ms: i64) -> SharedString {
    let minutes = (ts_ms + TZ_OFFSET_MS).div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60).into()
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Хранит последнего автора и время, чтобы решать, нужна ли строке шапка
struct Feed {
    users: Vec<mock::User>,
    model: Rc<VecModel<Msg>>,
    last: Option<(usize, i64)>,
}

impl Feed {
    fn row(&mut self, m: &mock::Message) -> Msg {
        let head = match self.last {
            Some((author, ts)) => author != m.author || m.ts_ms - ts > GROUP_GAP_MS,
            None => true,
        };
        self.last = Some((m.author, m.ts_ms));
        let u = &self.users[m.author];
        let (img_w, img_h) = match &m.image {
            Some(i) if i.w > i.h => (400.0, 400.0 * i.h as f32 / i.w as f32),
            Some(i) => (200.0, 200.0 * i.h as f32 / i.w as f32),
            None => (0.0, 0.0),
        };
        Msg {
            head,
            author: u.name.into(),
            initial: initial(u.name),
            tint: color(u.color),
            time: hhmm(m.ts_ms),
            text: m.text.as_str().into(),
            edited: false,
            has_image: m.image.is_some(),
            img_w,
            img_h,
            img_label: m.image.as_ref().map(|i| format!("{}×{}", i.w, i.h)).unwrap_or_default().into(),
            img_tint: color(m.image.as_ref().map_or(0, |i| i.color)),
        }
    }
}

/// Заполняет окно демо-данными. Возвращённые таймеры должны жить до конца работы.
pub fn run(ui: &AppWindow) -> Vec<Timer> {
    let mut rng = mock::Rng::new(42);
    let messages = mock::generate(MESSAGE_COUNT, now_ms(), &mut rng);
    let users = mock::users();

    ui.set_logged_in(true);
    ui.set_guild_name("Феникс · гильдия".into());
    ui.set_channel_name("общий".into());
    ui.set_can_write(true);
    let guild = |name: &str, active| GuildItem { name: name.into(), initial: name.into(), active, unread: false };
    ui.set_guilds(ModelRc::new(VecModel::from(vec![
        guild("Ф", true),
        guild("R", false),
        guild("Кл", false),
        guild("WoW", false),
        guild("CS", false),
    ])));
    let ch = |name: &str, kind, active| ChannelItem { name: name.into(), kind, active, unread: false };
    ui.set_channels(ModelRc::new(VecModel::from(vec![
        ch("ТЕКСТОВЫЕ", ChannelKind::Category, false),
        ch("общий", ChannelKind::Text, true),
        ch("рейды", ChannelKind::Text, false),
        ch("мемы", ChannelKind::Text, false),
        ch("билды", ChannelKind::Text, false),
        ch("ГОЛОСОВЫЕ", ChannelKind::Category, false),
        ch("Лобби", ChannelKind::Voice, false),
        ch("Рейд", ChannelKind::Voice, false),
        ch("AFK", ChannelKind::Voice, false),
    ])));
    ui.set_members(ModelRc::new(VecModel::from(
        users.iter().map(|u| Member { name: u.name.into(), initial: initial(u.name), tint: color(u.color) }).collect::<Vec<_>>(),
    )));
    ui.set_me_name(users[0].name.into());
    ui.set_me_initial(initial(users[0].name));
    ui.set_me_tint(color(users[0].color));

    let feed = Rc::new(RefCell::new(Feed { users, model: Rc::new(VecModel::default()), last: None }));
    {
        let mut f = feed.borrow_mut();
        let rows: Vec<Msg> = messages.iter().map(|m| f.row(m)).collect();
        f.model.set_vec(rows);
        ui.set_messages(ModelRc::from(f.model.clone()));
    }

    // Добавить сообщение и, если пользователь был внизу, прокрутить к нему
    let append = {
        let feed = feed.clone();
        let weak = ui.as_weak();
        move |m: mock::Message| {
            let Some(ui) = weak.upgrade() else { return };
            let stick = ui.invoke_near_bottom();
            let row = feed.borrow_mut().row(&m);
            feed.borrow().model.push(row);
            if stick {
                ui.invoke_scroll_to_bottom();
            }
        }
    };

    let live = Timer::default();
    {
        let append = append.clone();
        let rng = RefCell::new(rng);
        live.start(TimerMode::Repeated, LIVE_INTERVAL, move || {
            let mut r = rng.borrow_mut();
            let author = r.pick(mock::NAMES.len());
            let text = mock::PHRASES[r.pick(mock::PHRASES.len())].to_string();
            append(mock::Message { author, ts_ms: now_ms(), text, image: None });
        });
    }
    ui.on_send(move |text| {
        append(mock::Message { author: 0, ts_ms: now_ms(), text: text.to_string(), image: None });
    });

    // Открываемся внизу ленты, как любой чат
    let weak = ui.as_weak();
    Timer::single_shot(Duration::from_millis(30), move || {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_scroll_to_bottom();
        }
    });
    vec![live]
}

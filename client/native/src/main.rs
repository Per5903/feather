// Не открывать консольное окно на Windows в релизе
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod mock;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use slint::{Color, ModelRc, SharedString, Timer, TimerMode, VecModel};

slint::include_modules!();

const MESSAGE_COUNT: usize = 10_000;
// Имитация живого чата: входящее сообщение раз в N мс (как в прототипе на Tauri)
const LIVE_INTERVAL: Duration = Duration::from_millis(1500);
// Новое сообщение от того же автора в пределах 5 минут рисуется без шапки
const GROUP_GAP_MS: i64 = 5 * 60_000;
// Прототип: время показываем по МСК без учёта часового пояса системы
const TZ_OFFSET_MS: i64 = 3 * 3_600_000;

fn color(rgb: u32) -> Color {
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

fn hhmm(ts_ms: i64) -> SharedString {
    let minutes = (ts_ms + TZ_OFFSET_MS).div_euclid(60_000).rem_euclid(24 * 60);
    format!("{:02}:{:02}", minutes / 60, minutes % 60).into()
}

fn initial(name: &str) -> SharedString {
    name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default().into()
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
            has_image: m.image.is_some(),
            img_w,
            img_h,
            img_label: m.image.as_ref().map(|i| format!("{}×{}", i.w, i.h)).unwrap_or_default().into(),
            img_tint: color(m.image.as_ref().map_or(0, |i| i.color)),
        }
    }

    fn push(&mut self, m: &mock::Message) {
        let row = self.row(m);
        self.model.push(row);
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;

    let mut rng = mock::Rng::new(42);
    let messages = mock::generate(MESSAGE_COUNT, now_ms(), &mut rng);
    let users = mock::users();

    ui.set_members(ModelRc::new(VecModel::from(
        users
            .iter()
            .map(|u| Member { name: u.name.into(), initial: initial(u.name), tint: color(u.color) })
            .collect::<Vec<_>>(),
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
            feed.borrow_mut().push(&m);
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

    // Режим бенчмарка: FEATHER_WINDOW=2560x1440 задаёт размер окна в пикселях
    if let Some((w, h)) = std::env::var("FEATHER_WINDOW").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }) {
        ui.window().set_size(slint::PhysicalSize::new(w, h));
    }

    // Режим бенчмарка: FEATHER_AUTOSCROLL=1 непрерывно листает ленту (60 шагов в секунду)
    let autoscroll = Timer::default();
    if std::env::var_os("FEATHER_AUTOSCROLL").is_some() {
        let weak = ui.as_weak();
        let dir = std::cell::Cell::new(1.0f32);
        autoscroll.start(TimerMode::Repeated, Duration::from_millis(16), move || {
            if let Some(ui) = weak.upgrade() {
                if ui.invoke_scroll_by(40.0 * dir.get()) {
                    dir.set(-dir.get());
                }
            }
        });
    }

    // Режим проверки: FEATHER_SNAPSHOT=путь.png сохраняет снимок окна и закрывает его
    if let Some(path) = std::env::var_os("FEATHER_SNAPSHOT") {
        let weak = ui.as_weak();
        Timer::single_shot(Duration::from_secs(3), move || {
            if let Some(ui) = weak.upgrade() {
                if let Err(e) = save_snapshot(&ui, path.as_ref()) {
                    eprintln!("снимок не сохранён: {e}");
                }
                let _ = slint::quit_event_loop();
            }
        });
    }

    ui.run()
}

fn save_snapshot(ui: &AppWindow, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use slint::ComponentHandle;
    let shot = ui.window().take_snapshot()?;
    let mut enc = png::Encoder::new(std::fs::File::create(path)?, shot.width(), shot.height());
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(shot.as_bytes())?;
    Ok(())
}

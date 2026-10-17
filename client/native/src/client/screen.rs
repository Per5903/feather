//! Демонстрация экрана: начать/остановить свой показ, смотреть чужой.

use feather_core::screen::{QUALITIES, Source};

use super::*;

impl App {
    fn session(&self) -> Option<&VoiceSession> {
        self.voice.as_ref().and_then(|v| v.session.as_ref())
    }

    /// Кнопка в голосовой панели: остановить показ или открыть окно выбора
    pub(super) fn toggle_screen(&mut self) {
        let Some(session) = self.session() else {
            self.set_status("Сначала зайдите в голосовой канал");
            return;
        };
        if session.is_sharing() {
            self.rt.spawn(session.stop_screen());
            self.sharing = false;
            if self.watching == Some(self.me.id) {
                self.close_screen();
            }
            self.refresh_voice();
            return;
        }
        self.screen_sources = feather_core::screen::sources();
        let Some(ui) = self.ui() else { return };
        let q: Vec<SharedString> = QUALITIES.iter().map(|(n, _)| (*n).into()).collect();
        ui.set_screen_qualities(ModelRc::new(VecModel::from(q)));
        ui.set_screen_quality(self.settings.screen_quality.min(QUALITIES.len() - 1) as i32);
        let s: Vec<SharedString> = self.screen_sources.iter().map(|s| s.title.as_str().into()).collect();
        ui.set_screen_sources(ModelRc::new(VecModel::from(s)));
        ui.set_screen_source(0);
        ui.set_dialog("screen".into());
    }

    pub(super) fn start_screen(&mut self) {
        let Some(ui) = self.ui() else { return };
        ui.set_dialog(SharedString::new());
        let qi = (ui.get_screen_quality().max(0) as usize).min(QUALITIES.len() - 1);
        if self.settings.screen_quality != qi {
            self.settings.screen_quality = qi;
            settings::save(&self.settings);
        }
        let source: Option<Source> = self.screen_sources.get(ui.get_screen_source().max(0) as usize).cloned();
        let Some(session) = self.session() else { return };
        let id = self.voice.as_ref().map(|v| v.id).unwrap_or(0);
        let task = session.start_screen(source, QUALITIES[qi].1, move |reason| {
            post(move |app| {
                if app.voice.as_ref().is_some_and(|v| v.id == id) {
                    // Захват кончился сам: дорожку тоже снимаем
                    if let Some(s) = app.session() {
                        app.rt.spawn(s.stop_screen());
                    }
                    app.sharing = false;
                    if app.watching == Some(app.me.id) {
                        app.close_screen();
                    }
                    app.refresh_voice();
                    app.set_status(reason);
                }
            })
        });
        self.sharing = true;
        self.refresh_voice();
        self.rt.spawn(async move {
            if let Err(e) = task.await {
                post(move |app| {
                    app.sharing = false;
                    app.refresh_voice();
                    app.set_status(e);
                });
            }
        });
    }

    /// Участник начал или закончил показ
    pub(super) fn on_screen(&mut self, identity: String, live: bool) {
        let Ok(uid) = identity.parse::<u64>().map(Snowflake) else { return };
        if live {
            self.screen_live.insert(uid);
            // Для проверок: сразу смотреть первую трансляцию
            if std::env::var_os("FEATHER_AUTOWATCH").is_some() && self.watching.is_none() {
                // Событие могло прийти, пока сессия ещё подключается — смотрим чуть позже
                Timer::single_shot(Duration::from_millis(800), move || post(move |app| app.watch_screen(identity)));
            }
        } else {
            self.screen_live.remove(&uid);
            if self.watching == Some(uid) {
                self.close_screen();
                self.set_status("Трансляция закончилась");
            }
        }
        self.refresh_channels();
    }

    pub(super) fn watch_screen(&mut self, id: String) {
        let Ok(uid) = id.parse::<u64>().map(Snowflake) else { return };
        let me = uid == self.me.id;
        if (me && !self.sharing) || (!me && !self.screen_live.contains(&uid)) {
            return;
        }
        let Some(session) = self.session() else { return };
        // Подписка LiveKit запускает задачи tokio — нужен контекст рантайма
        let _rt = self.rt.enter();
        if me {
            // Свой экран: предпросмотр прямо с захвата
            session.watch_self();
        } else {
            session.watch(Some(&id));
        }
        self.watching = Some(uid);
        let name = if me {
            "Ваш экран — так его видят другие".to_owned()
        } else {
            self.voice_states.get(&uid).map(|v| v.user.shown_name().to_owned()).unwrap_or_else(|| "участник".into())
        };
        if let Some(ui) = self.ui() {
            ui.set_screen_name(name.into());
            ui.set_screen_has_frame(false);
        }
        // Забираем кадры с частотой экрана; новый кадр в RGBA готовит ядро, только когда прошлый показан
        self.screen_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), || {
            post(|app| {
                let Some(session) = app.session() else { return };
                // Размер окна просмотра: в обычном режиме — чат (без двух боковых колонок)
                if let Some(ui) = app.ui() {
                    let size = ui.window().size();
                    let side = if ui.get_screen_full() { 0 } else { (516.0 * ui.window().scale_factor()) as u32 };
                    session.set_view_size(size.width.saturating_sub(side), size.height);
                }
                let Some(frame) = session.take_screen_frame() else { return };
                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&frame.rgba, frame.width, frame.height);
                if let Some(ui) = app.ui() {
                    ui.set_screen_image(slint::Image::from_rgba8(buf));
                    ui.set_screen_has_frame(true);
                }
            })
        });
    }

    pub(super) fn close_screen(&mut self) {
        self.screen_timer.stop();
        if self.watching.take().is_some()
            && let Some(s) = self.session()
        {
            let _rt = self.rt.enter();
            s.watch(None);
        }
        if let Some(ui) = self.ui() {
            if ui.get_screen_full() {
                ui.set_screen_full(false);
                ui.window().set_fullscreen(false);
            }
            ui.set_screen_name(SharedString::new());
            ui.set_screen_image(slint::Image::default());
            ui.set_screen_has_frame(false);
        }
    }

    pub(super) fn toggle_fullscreen(&self) {
        let Some(ui) = self.ui() else { return };
        let full = !ui.get_screen_full();
        ui.set_screen_full(full);
        ui.window().set_fullscreen(full);
    }
}

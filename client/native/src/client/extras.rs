//! Сообщения и голос сверх основного: аватары, ответы, реакции, эмодзи, закреплённые, поиск,
//! @упоминания, вставка картинок, ветки, громкость собеседников, звуки, проверка обновлений.

use super::*;

/// Что показано в окне-списке
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    None,
    Search,
    Pins,
}

/// Сколько аватаров держать в памяти (каждый 128×128 RGBA = 64 КБ)
const AVATAR_CACHE: usize = 300;
const MAX_MENTION_SUGGESTIONS: usize = 6;

impl App {
    // ------------------------------------------------------------ аватары

    /// Картинка аватара, если уже загружена нужная версия
    pub(super) fn avatar_of(&self, u: &User) -> (slint::Image, bool) {
        match (u.avatar, self.avatars.get(&u.id)) {
            (Some(v), Some((have, img))) if *have == v => (img.clone(), true),
            _ => (slint::Image::default(), false),
        }
    }

    /// Догрузить аватары, которых нет в кэше (или устарели)
    pub(super) fn request_avatars<'a>(&mut self, users: impl Iterator<Item = &'a User>) {
        let mut wanted: Vec<(Snowflake, Snowflake)> = Vec::new();
        for u in users {
            let Some(v) = u.avatar else { continue };
            let fresh = self.avatars.get(&u.id).is_some_and(|(have, _)| *have == v);
            if !fresh && !self.avatars_loading.contains(&(u.id, v)) && !wanted.contains(&(u.id, v)) {
                wanted.push((u.id, v));
            }
        }
        for (user, version) in wanted {
            self.avatars_loading.insert((user, version));
            let api = self.api.clone();
            self.rt.spawn(async move {
                let decoded = api.avatar(user, version).await.ok().and_then(|bytes| {
                    let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).ok()?.to_rgba8();
                    Some(slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(img.as_raw(), img.width(), img.height()))
                });
                post(move |app| app.avatar_loaded(user, version, decoded));
            });
        }
    }

    fn avatar_loaded(&mut self, user: Snowflake, version: Snowflake, buffer: Option<slint::SharedPixelBuffer<slint::Rgba8Pixel>>) {
        self.avatars_loading.remove(&(user, version));
        let Some(buffer) = buffer else { return };
        if self.avatars.len() >= AVATAR_CACHE && !self.avatars.contains_key(&user) {
            // Простое вытеснение: убираем тех, кого сейчас нет на экране
            let visible: HashSet<Snowflake> = self.members.iter().map(|u| u.id).chain(self.messages.iter().map(|m| m.author.id)).collect();
            self.avatars.retain(|id, _| visible.contains(id));
        }
        self.avatars.insert(user, (version, slint::Image::from_rgba8(buffer)));
        // Перерисовываем только то, где есть этот человек
        if user == self.me.id {
            self.refresh_me();
        }
        if self.members.iter().any(|u| u.id == user) {
            self.refresh_members();
        }
        for i in 0..self.messages.len() {
            if self.messages[i].author.id == user {
                self.model.set_row_data(i, self.row_at(i));
            }
        }
        if self.voice_states.contains_key(&user) {
            self.refresh_channels();
        }
    }

    /// Нижняя панель: моё имя и аватар
    pub(super) fn refresh_me(&mut self) {
        let me = self.me.clone();
        self.request_avatars(std::iter::once(&me));
        let Some(ui) = self.ui() else { return };
        let (avatar, has) = self.avatar_of(&me);
        ui.set_me_name(me.shown_name().into());
        ui.set_me_initial(initial(me.shown_name()));
        ui.set_me_avatar(avatar);
        ui.set_me_has_avatar(has);
        ui.set_is_admin(self.admin);
    }

    /// Кто-то сменил имя или аватар: обновить везде, где он показан
    pub(super) fn on_user_update(&mut self, u: User) {
        if u.id == self.me.id {
            self.me = u.clone();
            self.refresh_me();
        }
        for m in self.members.iter_mut().filter(|m| m.id == u.id) {
            *m = u.clone();
        }
        for d in &mut self.dms {
            for r in d.recipients.iter_mut().filter(|r| r.id == u.id) {
                *r = u.clone();
            }
        }
        for v in self.voice_states.values_mut().filter(|v| v.user.id == u.id) {
            v.user = u.clone();
        }
        let mut touched = false;
        for m in self.messages.iter_mut().filter(|m| m.author.id == u.id) {
            m.author = u.clone();
            touched = true;
        }
        self.request_avatars(std::iter::once(&u));
        if self.dm_mode {
            self.sort_dms();
        }
        self.refresh_members();
        self.refresh_channels();
        if touched {
            self.rebuild_messages();
        }
    }

    // ------------------------------------------------------------ ответы

    pub(super) fn start_reply(&mut self, i: usize) {
        let Some(m) = self.messages.get(i).cloned() else { return };
        self.cancel_edit();
        self.reply_to = Some(m.id);
        if let Some(ui) = self.ui() {
            ui.set_reply_to(format!("Ответ для {} · {}", m.author.shown_name(), render::snippet(&m.content, 60)).into());
            ui.invoke_focus_composer();
        }
    }

    pub(super) fn cancel_reply(&mut self) {
        self.reply_to = None;
        if let Some(ui) = self.ui() {
            ui.set_reply_to(SharedString::new());
        }
    }

    // ------------------------------------------------------------ реакции и эмодзи

    pub(super) fn open_reaction_picker(&mut self, i: usize) {
        let Some(m) = self.messages.get(i) else { return };
        self.reacting_to = Some(m.id);
        if let Some(ui) = self.ui() {
            ui.set_emoji_for(i as i32);
        }
    }

    pub(super) fn close_emoji(&mut self) {
        self.reacting_to = None;
        if let Some(ui) = self.ui() {
            ui.set_emoji_for(-2);
        }
    }

    pub(super) fn emoji_picked(&mut self, emoji: String) {
        let Some(ui) = self.ui() else { return };
        if ui.get_emoji_for() == -1 {
            // В поле ввода: добавляем в конец (курсор Slint из Rust не узнать)
            let mut text = ui.get_composer_text().to_string();
            if !text.is_empty() && !text.ends_with(' ') {
                text.push(' ');
            }
            text.push_str(&emoji);
            ui.set_composer_text(text.into());
            ui.set_emoji_for(-2);
            ui.invoke_focus_composer();
            return;
        }
        let target = self.reacting_to.take();
        ui.set_emoji_for(-2);
        if let Some(id) = target {
            let mine = self.messages.iter().find(|m| m.id == id).is_some_and(|m| m.reactions.iter().any(|r| r.emoji == emoji && r.me));
            if !mine {
                self.send_reaction(id, emoji, true);
            }
        }
    }

    /// Клик по реакции под сообщением: поставить такую же или убрать свою
    pub(super) fn toggle_reaction(&mut self, i: usize, emoji: String) {
        let Some(m) = self.messages.get(i) else { return };
        let mine = m.reactions.iter().any(|r| r.emoji == emoji && r.me);
        self.send_reaction(m.id, emoji, !mine);
    }

    fn send_reaction(&mut self, message: Snowflake, emoji: String, add: bool) {
        let Some(channel) = self.channel else { return };
        // Сразу показываем: событие шлюза потом совпадёт и ничего не изменит
        if let Some(m) = self.messages.iter_mut().find(|m| m.id == message) {
            apply_reaction(m, &emoji, true, add);
            self.refresh_message(message);
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.react(channel, message, &emoji, add).await {
                post(move |app| {
                    // Не вышло — откатываем
                    if let Some(m) = app.messages.iter_mut().find(|m| m.id == message) {
                        apply_reaction(m, &emoji, true, !add);
                        app.refresh_message(message);
                    }
                    app.set_status(format!("Реакция не поставлена: {e}"));
                });
            }
        });
    }

    // ------------------------------------------------------------ закреплённые и поиск

    pub(super) fn toggle_pin(&mut self, i: usize) {
        let (Some(channel), Some(m)) = (self.channel, self.messages.get(i)) else { return };
        let (id, on) = (m.id, !m.pinned);
        let api = self.api.clone();
        self.rt.spawn(async move {
            match api.pin(channel, id, on).await {
                Ok(()) => post(move |app| app.set_status(if on { "Сообщение закреплено" } else { "Сообщение откреплено" })),
                Err(e) => post(move |app| app.set_status(format!("Не удалось: {e}"))),
            }
        });
    }

    fn message_row(&self, m: &Message, with_channel: bool) -> ListRow {
        let (avatar, has_avatar) = self.avatar_of(&m.author);
        let channel = if with_channel {
            self.channels.iter().find(|c| c.id == m.channel_id).map(|c| format!(" · #{}", c.name)).unwrap_or_default()
        } else {
            String::new()
        };
        let attachments = if m.attachments.is_empty() { String::new() } else { format!(" 📎{}", m.attachments.len()) };
        ListRow {
            title: m.author.shown_name().into(),
            subtitle: format!("{}{channel}", time_label(m.id.timestamp_ms() as i64)).into(),
            text: format!("{}{attachments}", render::snippet(&m.content, 300)).into(),
            initial: initial(m.author.shown_name()),
            tint: self.user_tint(m.author.id),
            avatar,
            has_avatar,
            action: if self.list_kind == ListKind::Pins && (self.dm_mode || self.moderator()) { "Открепить".into() } else { SharedString::new() },
            action2: SharedString::new(),
        }
    }

    fn refresh_list(&mut self) {
        let Some(ui) = self.ui() else { return };
        let with_channel = self.list_kind == ListKind::Search && !self.dm_mode;
        let rows: Vec<ListRow> = self.list_messages.iter().map(|m| self.message_row(m, with_channel)).collect();
        ui.set_list_rows(ModelRc::new(VecModel::from(rows)));
        let authors: Vec<User> = self.list_messages.iter().map(|m| m.author.clone()).collect();
        self.request_avatars(authors.iter());
    }

    pub(super) fn open_pins(&mut self) {
        let Some(channel) = self.channel else { return };
        self.list_kind = ListKind::Pins;
        self.list_messages.clear();
        if let Some(ui) = self.ui() {
            ui.set_list_title("Закреплённые сообщения".into());
            ui.set_list_empty("Загрузка…".into());
            ui.set_list_searchable(false);
            ui.set_list_rows(ModelRc::default());
            ui.set_dialog("list".into());
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.pins(channel).await;
            post(move |app| {
                if app.list_kind != ListKind::Pins {
                    return;
                }
                match res {
                    Ok(list) => {
                        app.list_messages = list;
                        if let Some(ui) = app.ui() {
                            ui.set_list_empty(
                                if app.dm_mode || app.moderator() {
                                    "Закреплённых нет. Закрепить: меню «⋯» у сообщения."
                                } else {
                                    "Закреплённых нет."
                                }
                                .into(),
                            );
                        }
                        app.refresh_list();
                    }
                    Err(e) => app.set_status(format!("Не удалось загрузить закреплённые: {e}")),
                }
            });
        });
    }

    pub(super) fn open_search(&mut self) {
        self.list_kind = ListKind::Search;
        self.list_messages.clear();
        if let Some(ui) = self.ui() {
            ui.set_list_title(if self.dm_mode { "Поиск в диалоге".into() } else { "Поиск по сообществу".into() });
            ui.set_list_empty("Введите слово или фразу (от 2 символов).".into());
            ui.set_list_searchable(true);
            ui.set_list_rows(ModelRc::default());
            ui.set_dialog("list".into());
        }
    }

    pub(super) fn run_search(&mut self) {
        let (Some(channel), Some(ui)) = (self.channel, self.ui()) else { return };
        let query = ui.get_search_query().trim().to_owned();
        if query.chars().count() < 2 {
            ui.set_list_empty("Нужно хотя бы 2 символа.".into());
            return;
        }
        ui.set_list_empty("Ищу…".into());
        let guild = if self.dm_mode { None } else { self.guild };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.search(guild, channel, &query).await;
            post(move |app| {
                if app.list_kind != ListKind::Search {
                    return;
                }
                match res {
                    Ok(found) => {
                        app.list_messages = found;
                        if let Some(ui) = app.ui() {
                            ui.set_list_empty("Ничего не найдено.".into());
                        }
                        app.refresh_list();
                    }
                    Err(e) => {
                        if let Some(ui) = app.ui() {
                            ui.set_list_empty(e.to_string().into());
                        }
                    }
                }
            });
        });
    }

    /// Клик по найденному сообщению: открыть его канал
    pub(super) fn list_open(&mut self, i: usize) {
        let Some(m) = self.list_messages.get(i).cloned() else { return };
        if let Some(ui) = self.ui() {
            ui.set_dialog(SharedString::new());
        }
        if self.channel == Some(m.channel_id) {
            self.scroll_to_message(m.id);
            return;
        }
        if let Some(c) = self.channels.iter().find(|c| c.id == m.channel_id).cloned() {
            self.select_channel(&c);
        }
    }

    /// Прокрутить к сообщению, если оно уже загружено (иначе — просто открыт канал)
    fn scroll_to_message(&self, id: Snowflake) {
        let Some(i) = self.messages.iter().position(|m| m.id == id) else { return };
        if let Some(ui) = self.ui() {
            // Высоты строк разные: прокручиваем пропорционально позиции — точности хватает
            let frac = i as f32 / self.messages.len().max(1) as f32;
            let total = ui.invoke_content_height();
            ui.invoke_scroll_to_bottom();
            let delta = (1.0 - frac) * total;
            ui.invoke_shift_scroll(-delta);
        }
    }

    pub(super) fn list_action(&mut self, i: usize) {
        if self.list_kind != ListKind::Pins {
            return;
        }
        let (Some(channel), Some(m)) = (self.channel, self.list_messages.get(i).cloned()) else { return };
        self.list_messages.remove(i);
        self.refresh_list();
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.pin(channel, m.id, false).await {
                post(move |app| app.set_status(format!("Не удалось открепить: {e}")));
            }
        });
    }

    // ------------------------------------------------------------ @упоминания

    /// Имена для подсветки @упоминаний: участники (логины) и цвет
    pub(super) fn mention_palette(&self) -> Vec<(String, String)> {
        let hex = |c: slint::Color| format!("#{:02x}{:02x}{:02x}", c.red(), c.green(), c.blue());
        let mut out: Vec<(String, String)> = self.members.iter().map(|u| (u.username.clone(), hex(self.user_tint(u.id)))).collect();
        if !out.iter().any(|(n, _)| *n == self.me.username) {
            out.push((self.me.username.clone(), hex(self.user_tint(self.me.id))));
        }
        out
    }

    pub(super) fn composer_changed(&mut self, text: String) {
        let prefix = render::mention_prefix(&text).map(str::to_lowercase);
        self.mention_candidates = match prefix {
            Some(p) => {
                let mut list: Vec<User> = self
                    .members
                    .iter()
                    .filter(|u| u.id != self.me.id)
                    .filter(|u| u.username.to_lowercase().starts_with(&p) || u.shown_name().to_lowercase().starts_with(&p))
                    .cloned()
                    .collect();
                list.sort_by_key(|u| u.username.to_lowercase());
                list.truncate(MAX_MENTION_SUGGESTIONS);
                list
            }
            None => Vec::new(),
        };
        self.refresh_mentions();
    }

    pub(super) fn refresh_mentions(&mut self) {
        let Some(ui) = self.ui() else { return };
        let items: Vec<MentionItem> = self
            .mention_candidates
            .iter()
            .map(|u| {
                let (avatar, has_avatar) = self.avatar_of(u);
                let name = if u.shown_name() == u.username { u.username.clone() } else { format!("{} (@{})", u.shown_name(), u.username) };
                MentionItem { name: name.into(), initial: initial(u.shown_name()), tint: self.user_tint(u.id), avatar, has_avatar }
            })
            .collect();
        ui.set_mentions(ModelRc::new(VecModel::from(items)));
    }

    pub(super) fn pick_mention(&mut self, i: usize) {
        let (Some(u), Some(ui)) = (self.mention_candidates.get(i).cloned(), self.ui()) else { return };
        let text = ui.get_composer_text().to_string();
        let Some(at) = text.rfind('@') else { return };
        let new = format!("{}@{} ", &text[..at], u.username);
        ui.set_composer_text(new.into());
        self.mention_candidates.clear();
        self.refresh_mentions();
        ui.invoke_focus_composer();
    }

    // ------------------------------------------------------------ вставка, копирование, ссылки, ветки

    /// Прикрепить файл из памяти (скриншот из буфера обмена)
    pub(super) fn attach_bytes(&mut self, name: String, bytes: Vec<u8>, content_type: &'static str) {
        let Some(channel) = self.channel else { return };
        let size = bytes.len() as u64;
        if size > MAX_UPLOAD {
            self.set_status("Картинка больше 25 МБ");
            return;
        }
        self.pending_files.push(PendingFile { name: name.clone(), size, attachment: None });
        self.refresh_pending();
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.upload(channel, &name, content_type, bytes).await;
            post(move |app| {
                let slot = app.pending_files.iter().position(|f| f.name == name && f.attachment.is_none());
                match (res, slot) {
                    (Ok(a), Some(i)) => app.pending_files[i].attachment = Some(a),
                    (Err(e), Some(i)) => {
                        app.pending_files.remove(i);
                        app.set_status(format!("Картинка не загружена: {e}"));
                    }
                    _ => {}
                }
                app.refresh_pending();
            });
        });
    }

    pub(super) fn copy_message(&self, i: usize) {
        if let Some(m) = self.messages.get(i) {
            feather_core::files::copy_text(&m.content);
            self.set_status("Текст скопирован");
        }
    }

    pub(super) fn open_embed(&self, i: usize) {
        if let Some(e) = self.messages.get(i).and_then(|m| m.embeds.first()) {
            feather_core::files::open_url(&e.url);
        }
    }

    pub(super) fn ask_thread(&mut self, i: usize) {
        let Some(m) = self.messages.get(i) else { return };
        self.pending_thread = Some(m.id);
        if let Some(ui) = self.ui() {
            ui.set_dialog_value(render::snippet(&m.content, 40).into());
            ui.set_dialog("thread".into());
        }
    }

    pub(super) fn create_thread(&mut self, name: String) {
        let (Some(channel), Some(message)) = (self.channel, self.pending_thread.take()) else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.create_thread(channel, message, &name).await;
            post(move |app| match res {
                Ok(thread) => {
                    if !app.channels.iter().any(|c| c.id == thread.id) {
                        let mut all = std::mem::take(&mut app.channels);
                        all.push(thread.clone());
                        app.channels = ordered(all);
                    }
                    app.select_channel(&thread);
                }
                Err(e) => app.set_status(format!("Ветка не создана: {e}")),
            });
        });
    }

    pub(super) fn open_thread(&mut self, i: usize) {
        let Some(t) = self.messages.get(i).and_then(|m| m.thread_id) else { return };
        if let Some(c) = self.channels.iter().find(|c| c.id == t).cloned() {
            self.select_channel(&c);
        }
    }

    /// Перезапросить каналы открытого сообщества (сменились права — видимые каналы другие)
    pub(super) fn reload_channels(&mut self) {
        let Some(guild) = self.guild else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Ok(channels) = api.channels(guild).await {
                post(move |app| {
                    if app.guild != Some(guild) {
                        return;
                    }
                    app.channels = ordered(channels);
                    // Открытый канал стал закрытым для меня
                    if let Some(c) = app.channel
                        && !app.channels.iter().any(|x| x.id == c)
                    {
                        app.channel = None;
                        app.messages.clear();
                        app.model.set_vec(Vec::new());
                        app.set_status("Канал стал недоступен");
                    }
                    app.refresh_channels();
                });
            }
        });
    }

    // ------------------------------------------------------------ бан

    pub(super) fn ask_ban(&mut self, i: usize) {
        let Some(u) = self.members_view.get(i).cloned() else { return };
        self.pending_ban = Some(u.id);
        if let Some(ui) = self.ui() {
            ui.set_dialog_value(u.shown_name().into());
            ui.set_dialog("ban".into());
        }
    }

    pub(super) fn ban_confirmed(&mut self) {
        let (Some(guild), Some(user)) = (self.guild, self.pending_ban.take()) else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Err(e) = api.ban(guild, user).await {
                post(move |app| app.set_status(format!("Не удалось забанить: {e}")));
            }
        });
    }

    // ------------------------------------------------------------ громкость и звуки

    pub(super) fn open_volume(&mut self, id: String) {
        let Ok(uid) = id.parse::<u64>().map(Snowflake) else { return };
        let Some(v) = self.voice_states.get(&uid) else { return };
        self.volume_target = Some(uid);
        let gain = self.settings.volumes.get(&id).copied().unwrap_or(1.0);
        if let Some(ui) = self.ui() {
            ui.set_volume_name(v.user.shown_name().into());
            ui.set_volume_value(gain * 100.0);
            ui.set_dialog("volume".into());
        }
    }

    pub(super) fn volume_changed(&mut self, percent: f32) {
        let Some(uid) = self.volume_target else { return };
        let gain = (percent / 100.0).clamp(0.0, 2.0);
        let key = uid.to_string();
        if (gain - 1.0).abs() < 0.01 {
            self.settings.volumes.remove(&key);
        } else {
            self.settings.volumes.insert(key.clone(), gain);
        }
        settings::save(&self.settings);
        if let Some(s) = self.voice.as_ref().and_then(|v| v.session.as_ref()) {
            s.set_volume(&key, gain);
        }
        self.refresh_channels();
    }

    /// Звук интерфейса голоса, если включён в настройках
    pub(super) fn cue(&self, cue: Cue) {
        if !self.settings.voice_sounds {
            return;
        }
        if let Some(s) = self.voice.as_ref().and_then(|v| v.session.as_ref()) {
            s.play_cue(cue);
        }
    }

    // ------------------------------------------------------------ обновления

    pub(super) fn check_update(&self) {
        if !self.settings.check_updates || std::env::var_os("FEATHER_NO_UPDATE_CHECK").is_some() {
            return;
        }
        self.rt.spawn(async move {
            if let Some((version, url)) = feather_core::update::check(VERSION).await {
                post(move |app| {
                    app.update_url = Some(url);
                    if let Some(ui) = app.ui() {
                        ui.set_update_version(version.into());
                    }
                });
            }
        });
    }

    pub(super) fn open_update(&self) {
        if let Some(url) = &self.update_url {
            feather_core::files::open_url(url);
        }
    }
}

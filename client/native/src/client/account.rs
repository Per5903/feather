//! Мой аккаунт (имя, аватар, пароль, двухфакторная защита, удаление) и панель администратора.

use feather_protocol::RegistrationMode;

use super::*;

impl App {
    fn account_message(&self, text: impl Into<SharedString>, ok: bool) {
        if let Some(ui) = self.ui() {
            ui.set_account_message(text.into());
            ui.set_account_message_ok(ok);
        }
    }

    pub(super) fn open_account(&mut self) {
        let Some(ui) = self.ui() else { return };
        ui.set_account_tab(0);
        ui.set_account_username(self.me.username.as_str().into());
        ui.set_account_display_name(self.me.display_name.clone().unwrap_or_default().into());
        for clear in [AppWindow::set_account_pw_current, AppWindow::set_account_pw_new, AppWindow::set_account_pw_new2] {
            clear(&ui, SharedString::new());
        }
        ui.set_account_totp_code(SharedString::new());
        ui.set_account_confirm_password(SharedString::new());
        ui.set_account_totp_setup(false);
        ui.set_account_message(SharedString::new());
        ui.set_dialog("account".into());
        let api = self.api.clone();
        self.rt.spawn(async move {
            if let Ok(on) = api.totp_status().await {
                post(move |app| {
                    if let Some(ui) = app.ui() {
                        ui.set_account_totp_enabled(on);
                    }
                });
            }
        });
    }

    pub(super) fn account_save_profile(&mut self) {
        let Some(ui) = self.ui() else { return };
        let name = ui.get_account_display_name().trim().to_owned();
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.update_me(&name).await;
            post(move |app| match res {
                Ok(me) => {
                    app.on_user_update(me);
                    app.account_message("Имя сохранено", true);
                }
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn account_pick_avatar(&mut self) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            let Some(path) = feather_core::files::pick_files().await.into_iter().next() else { return };
            let res = match tokio::fs::read(&path).await {
                Ok(bytes) if bytes.len() > 8 * 1024 * 1024 => Err("картинка больше 8 МБ".to_owned()),
                Ok(bytes) => api.set_avatar(bytes).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            post(move |app| match res {
                Ok(me) => {
                    app.on_user_update(me);
                    app.account_message("Аватар обновлён", true);
                }
                Err(e) => app.account_message(e, false),
            });
        });
    }

    pub(super) fn account_remove_avatar(&mut self) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.remove_avatar().await;
            post(move |app| match res {
                Ok(me) => app.on_user_update(me),
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn account_change_password(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (current, new, new2) = (ui.get_account_pw_current().to_string(), ui.get_account_pw_new().to_string(), ui.get_account_pw_new2().to_string());
        if new != new2 {
            return self.account_message("Новые пароли не совпадают", false);
        }
        if new.chars().count() < 8 {
            return self.account_message("Пароль — от 8 символов", false);
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.change_password(&current, &new).await;
            post(move |app| match res {
                Ok(()) => {
                    if let Some(ui) = app.ui() {
                        ui.set_account_pw_current(SharedString::new());
                        ui.set_account_pw_new(SharedString::new());
                        ui.set_account_pw_new2(SharedString::new());
                    }
                    app.account_message("Пароль сменён. Другие устройства вышли из аккаунта", true);
                }
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn account_start_totp(&mut self) {
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.totp_setup().await;
            post(move |app| match res {
                Ok(setup) => {
                    let Some(ui) = app.ui() else { return };
                    if let Some((side, px)) = render::qr_pixels(&setup.uri, 4) {
                        let buf = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(&px, side, side);
                        ui.set_account_totp_qr(slint::Image::from_rgb8(buf));
                    }
                    // Ключ группами по 4 — так его проще перепечатать
                    let grouped = setup.secret.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ");
                    ui.set_account_totp_secret(grouped.into());
                    ui.set_account_totp_code(SharedString::new());
                    ui.set_account_totp_setup(true);
                    app.account_message("", true);
                }
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn account_enable_totp(&mut self) {
        let Some(ui) = self.ui() else { return };
        let code = ui.get_account_totp_code().trim().to_owned();
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.totp_enable(&code).await;
            post(move |app| match res {
                Ok(()) => {
                    if let Some(ui) = app.ui() {
                        ui.set_account_totp_setup(false);
                        ui.set_account_totp_enabled(true);
                        ui.set_account_totp_code(SharedString::new());
                    }
                    app.account_message("Двухфакторная защита включена. При входе понадобится код из приложения", true);
                }
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn account_disable_totp(&mut self) {
        let Some(ui) = self.ui() else { return };
        let (password, code) = (ui.get_account_confirm_password().to_string(), ui.get_account_totp_code().trim().to_owned());
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.totp_disable(&password, &code).await;
            post(move |app| match res {
                Ok(()) => {
                    if let Some(ui) = app.ui() {
                        ui.set_account_totp_enabled(false);
                        ui.set_account_confirm_password(SharedString::new());
                        ui.set_account_totp_code(SharedString::new());
                    }
                    app.account_message("Двухфакторная защита выключена", true);
                }
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn account_delete(&mut self) {
        let Some(ui) = self.ui() else { return };
        let password = ui.get_account_confirm_password().to_string();
        let code = ui.get_account_totp_code().trim().to_owned();
        if password.is_empty() {
            return self.account_message("Введите пароль", false);
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.delete_account(&password, (!code.is_empty()).then_some(code.as_str())).await;
            post(move |app| match res {
                Ok(()) => {
                    app.logout();
                    if let Some(ui) = app.ui() {
                        ui.set_login_error("Аккаунт удалён".into());
                    }
                }
                Err(e) => app.account_message(e.to_string(), false),
            });
        });
    }

    // ------------------------------------------------------------ администратор

    fn admin_message(&self, text: impl Into<SharedString>, ok: bool) {
        if let Some(ui) = self.ui() {
            ui.set_admin_message(text.into());
            ui.set_admin_message_ok(ok);
        }
    }

    pub(super) fn open_admin(&mut self) {
        if !self.admin {
            return;
        }
        if let Some(ui) = self.ui() {
            ui.set_admin_message(SharedString::new());
            ui.set_dialog("admin".into());
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = tokio::try_join!(api.admin_users(), api.admin_settings());
            post(move |app| match res {
                Ok((users, settings)) => {
                    app.admin_users = users;
                    if let Some(ui) = app.ui() {
                        ui.set_admin_registration(match settings.registration {
                            RegistrationMode::Open => 0,
                            RegistrationMode::Invite => 1,
                            RegistrationMode::Closed => 2,
                        });
                    }
                    app.refresh_admin_users();
                }
                Err(e) => app.admin_message(e.to_string(), false),
            });
        });
    }

    fn refresh_admin_users(&mut self) {
        let Some(ui) = self.ui() else { return };
        let rows: Vec<ListRow> = self
            .admin_users
            .iter()
            .map(|a| {
                let (avatar, has_avatar) = self.avatar_of(&a.user);
                let mut tags = vec![format!("@{}", a.user.username), format!("с {}", time_label(a.created_at))];
                if a.is_admin {
                    tags.push("администратор".into());
                }
                if a.totp_enabled {
                    tags.push("2FA".into());
                }
                let me = a.user.id == self.me.id;
                ListRow {
                    title: a.user.shown_name().into(),
                    subtitle: tags.join(" · ").into(),
                    text: format!("сообществ: {}", a.guilds).into(),
                    initial: initial(a.user.shown_name()),
                    tint: user_color(a.user.id),
                    avatar,
                    has_avatar,
                    action: if me { SharedString::new() } else { "Сбросить пароль".into() },
                    action2: if me { SharedString::new() } else { "Удалить".into() },
                }
            })
            .collect();
        ui.set_admin_users(ModelRc::new(VecModel::from(rows)));
        let users: Vec<User> = self.admin_users.iter().map(|a| a.user.clone()).collect();
        self.request_avatars(users.iter());
    }

    pub(super) fn admin_set_registration(&mut self, i: i32) {
        let mode = match i {
            0 => RegistrationMode::Open,
            1 => RegistrationMode::Invite,
            _ => RegistrationMode::Closed,
        };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.admin_set_registration(mode).await;
            post(move |app| match res {
                Ok(_) => app.admin_message("Режим регистрации сохранён", true),
                Err(e) => app.admin_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn admin_reset_password(&mut self, i: usize) {
        let Some(u) = self.admin_users.get(i).map(|a| a.user.clone()) else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.admin_reset_password(u.id).await;
            post(move |app| match res {
                Ok(temp) => {
                    if let Some(ui) = app.ui() {
                        ui.set_dialog_value(temp.password.into());
                        ui.set_dialog_switch(true);
                        ui.set_dialog("info".into());
                    }
                }
                Err(e) => app.admin_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn admin_ask_delete(&mut self, i: usize) {
        let Some(u) = self.admin_users.get(i).map(|a| a.user.clone()) else { return };
        self.pending_user_delete = Some(u.id);
        if let Some(ui) = self.ui() {
            ui.set_dialog_value(format!("{} (@{})", u.shown_name(), u.username).into());
            ui.set_dialog("confirm-user".into());
        }
    }

    pub(super) fn admin_delete_confirmed(&mut self) {
        let Some(user) = self.pending_user_delete.take() else { return };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.admin_delete_user(user).await;
            post(move |app| {
                app.open_admin();
                match res {
                    Ok(()) => app.admin_message("Пользователь удалён", true),
                    Err(e) => app.admin_message(e.to_string(), false),
                }
            });
        });
    }
}

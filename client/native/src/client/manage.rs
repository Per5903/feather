//! Управление сообществом и каналами: настройки сообщества (название, передача, удаление,
//! выход), баны, журнал модерации, настройки канала (закрытый, только чтение).

use feather_protocol::{AuditEntry, OverwriteRequest, UpdateChannelRequest, UpdateGuildRequest};

use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GuildTab {
    General,
    Bans,
    Audit,
}

/// Действие журнала модерации по-человечески
fn audit_text(e: &AuditEntry) -> String {
    let what = match e.action.as_str() {
        "channel_create" => "создал(а) канал",
        "channel_update" => "изменил(а) канал",
        "channel_delete" => "удалил(а) канал",
        "guild_update" => "переименовал(а) сообщество",
        "role_create" => "создал(а) роль",
        "role_update" => "изменил(а) роль",
        "role_delete" => "удалил(а) роль",
        "member_role_add" => "выдал(а) роль",
        "member_role_remove" => "снял(а) роль",
        "kick" => "выгнал(а)",
        "ban" => "забанил(а)",
        "unban" => "разбанил(а)",
        "message_delete" => "удалил(а) сообщение",
        "message_pin" => "закрепил(а) сообщение",
        "message_unpin" => "открепил(а) сообщение",
        "overwrite_update" => "изменил(а) права канала",
        "overwrite_delete" => "сбросил(а) права канала",
        "invite_create" => "создал(а) приглашение",
        "owner_transfer" => "передал(а) сообщество",
        other => other,
    };
    if e.details.is_empty() { what.to_owned() } else { format!("{what}: {}", render::snippet(&e.details, 200)) }
}

impl App {
    fn current_guild(&self) -> Option<&Guild> {
        self.guild.and_then(|g| self.guilds.iter().find(|x| x.id == g))
    }

    fn guild_message(&self, text: impl Into<SharedString>, ok: bool) {
        if let Some(ui) = self.ui() {
            ui.set_guild_message(text.into());
            ui.set_guild_message_ok(ok);
        }
    }

    pub(super) fn open_guild_settings(&mut self) {
        let Some(g) = self.current_guild().cloned() else { return };
        let owner = g.owner_id == self.me.id;
        self.guild_tabs = vec![GuildTab::General];
        if self.can(perms::BAN_MEMBERS) {
            self.guild_tabs.push(GuildTab::Bans);
        }
        if self.can(perms::MANAGE_GUILD) {
            self.guild_tabs.push(GuildTab::Audit);
        }
        self.owner_candidates = self.members.iter().filter(|u| u.id != self.me.id).cloned().collect();
        self.owner_candidates.sort_by_key(|u| u.username.to_lowercase());
        let Some(ui) = self.ui() else { return };
        let labels: Vec<SharedString> = self
            .guild_tabs
            .iter()
            .map(|t| match t {
                GuildTab::General => "Общие",
                GuildTab::Bans => "Баны",
                GuildTab::Audit => "Журнал",
            })
            .map(Into::into)
            .collect();
        ui.set_guild_tab_labels(ModelRc::new(VecModel::from(labels)));
        ui.set_guild_tab(0);
        ui.set_guild_edit_name(g.name.as_str().into());
        ui.set_guild_can_manage(self.can(perms::MANAGE_GUILD));
        ui.set_guild_is_owner(owner);
        let names: Vec<SharedString> = self.owner_candidates.iter().map(|u| format!("{} (@{})", u.shown_name(), u.username).into()).collect();
        ui.set_guild_member_names(ModelRc::new(VecModel::from(names)));
        ui.set_guild_new_owner(0);
        ui.set_guild_confirm_name(SharedString::new());
        ui.set_guild_bans(ModelRc::default());
        ui.set_guild_audit(ModelRc::default());
        ui.set_guild_message(SharedString::new());
        ui.set_dialog("guild".into());
    }

    pub(super) fn guild_tab_changed(&mut self, i: usize) {
        let Some(guild) = self.guild else { return };
        self.guild_message("", true);
        let api = self.api.clone();
        match self.guild_tabs.get(i) {
            Some(GuildTab::Bans) => {
                self.rt.spawn(async move {
                    let res = api.bans(guild).await;
                    post(move |app| match res {
                        Ok(bans) => {
                            app.bans = bans.into_iter().map(|b| b.user).collect();
                            app.refresh_bans();
                        }
                        Err(e) => app.guild_message(e.to_string(), false),
                    });
                });
            }
            Some(GuildTab::Audit) => {
                self.rt.spawn(async move {
                    let res = api.audit_log(guild).await;
                    post(move |app| match res {
                        Ok(entries) => {
                            let rows: Vec<ListRow> = entries
                                .iter()
                                .map(|e| {
                                    let (avatar, has_avatar) = app.avatar_of(&e.actor);
                                    ListRow {
                                        title: e.actor.shown_name().into(),
                                        subtitle: time_label(e.id.timestamp_ms() as i64),
                                        text: audit_text(e).into(),
                                        initial: initial(e.actor.shown_name()),
                                        tint: app.user_tint(e.actor.id),
                                        avatar,
                                        has_avatar,
                                        action: SharedString::new(),
                                        action2: SharedString::new(),
                                    }
                                })
                                .collect();
                            if let Some(ui) = app.ui() {
                                ui.set_guild_audit(ModelRc::new(VecModel::from(rows)));
                            }
                        }
                        Err(e) => app.guild_message(e.to_string(), false),
                    });
                });
            }
            _ => {}
        }
    }

    fn refresh_bans(&self) {
        let Some(ui) = self.ui() else { return };
        let rows: Vec<ListRow> = self
            .bans
            .iter()
            .map(|u| {
                let (avatar, has_avatar) = self.avatar_of(u);
                ListRow {
                    title: u.shown_name().into(),
                    subtitle: format!("@{}", u.username).into(),
                    text: SharedString::new(),
                    initial: initial(u.shown_name()),
                    tint: user_color(u.id),
                    avatar,
                    has_avatar,
                    action: "Разбанить".into(),
                    action2: SharedString::new(),
                }
            })
            .collect();
        ui.set_guild_bans(ModelRc::new(VecModel::from(rows)));
    }

    pub(super) fn guild_unban(&mut self, i: usize) {
        let (Some(guild), Some(u)) = (self.guild, self.bans.get(i).cloned()) else { return };
        self.bans.remove(i);
        self.refresh_bans();
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.unban(guild, u.id).await;
            post(move |app| match res {
                Ok(()) => app.guild_message(format!("{} разбанен(а): может вернуться по приглашению", u.shown_name()), true),
                Err(e) => app.guild_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn guild_save_name(&mut self) {
        let (Some(guild), Some(ui)) = (self.guild, self.ui()) else { return };
        let name = ui.get_guild_edit_name().trim().to_owned();
        if name.is_empty() {
            return;
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.update_guild(guild, &UpdateGuildRequest { name: Some(name), owner_id: None }).await;
            post(move |app| match res {
                Ok(_) => app.guild_message("Название сохранено", true),
                Err(e) => app.guild_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn guild_transfer(&mut self) {
        let (Some(guild), Some(ui)) = (self.guild, self.ui()) else { return };
        let Some(target) = self.owner_candidates.get(ui.get_guild_new_owner().max(0) as usize).cloned() else {
            self.guild_message("В сообществе больше никого нет", false);
            return;
        };
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.update_guild(guild, &UpdateGuildRequest { name: None, owner_id: Some(target.id) }).await;
            post(move |app| match res {
                Ok(_) => {
                    app.guild_message(format!("Сообщество передано: {}", target.shown_name()), true);
                    if let Some(ui) = app.ui() {
                        ui.set_guild_is_owner(false);
                    }
                }
                Err(e) => app.guild_message(e.to_string(), false),
            });
        });
    }

    pub(super) fn guild_delete(&mut self) {
        let (Some(g), Some(ui)) = (self.current_guild().cloned(), self.ui()) else { return };
        if ui.get_guild_confirm_name().trim() != g.name.trim() {
            self.guild_message("Название не совпадает — сообщество не удалено", false);
            return;
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            match api.delete_guild(g.id).await {
                // Само сообщество уйдёт из списка событием GUILD_DELETE
                Ok(()) => post(|app| {
                    if let Some(ui) = app.ui() {
                        ui.set_dialog(SharedString::new());
                    }
                }),
                Err(e) => post(move |app| app.guild_message(e.to_string(), false)),
            }
        });
    }

    pub(super) fn guild_leave(&mut self) {
        let Some(guild) = self.guild else { return };
        let (api, me) = (self.api.clone(), self.me.id);
        self.rt.spawn(async move {
            match api.kick(guild, me).await {
                Ok(()) => post(|app| {
                    if let Some(ui) = app.ui() {
                        ui.set_dialog(SharedString::new());
                    }
                }),
                Err(e) => post(move |app| app.guild_message(e.to_string(), false)),
            }
        });
    }

    // ------------------------------------------------------------ настройки канала

    pub(super) fn open_channel_settings(&mut self, i: usize) {
        let Some(c) = self.channels.get(i).cloned() else { return };
        let Some(guild) = self.guild else { return };
        self.settings_channel = Some(c.id);
        let everyone = c.overwrites.iter().find(|o| o.kind == OverwriteKind::Role && o.id == guild);
        let private = everyone.is_some_and(|o| o.deny & perms::VIEW_CHANNEL != 0);
        let read_only = everyone.is_some_and(|o| o.deny & perms::SEND_MESSAGES != 0);
        // Роли ниже моей (их права в канале я могу менять); @everyone — отдельными переключателями
        let my_top = self.top_position(self.me.id);
        let roles: Vec<Role> = self.roles_sorted().into_iter().filter(|r| r.id != guild && r.position < my_top).collect();
        self.settings_roles = roles.iter().map(|r| r.id).collect();
        let toggles: Vec<RoleToggle> = roles
            .iter()
            .map(|r| {
                let o = c.overwrites.iter().find(|o| o.kind == OverwriteKind::Role && o.id == r.id);
                RoleToggle {
                    name: r.name.as_str().into(),
                    tint: if r.color == 0 { slint::Color::from_rgb_u8(0x99, 0xaa, 0xb5) } else { color(r.color) },
                    view: o.is_some_and(|o| o.allow & perms::VIEW_CHANNEL != 0),
                    send: o.is_some_and(|o| o.allow & perms::SEND_MESSAGES != 0),
                }
            })
            .collect();
        let Some(ui) = self.ui() else { return };
        ui.set_ch_name(c.name.as_str().into());
        ui.set_ch_is_voice(c.kind == Kind::Voice || c.kind == Kind::Thread);
        ui.set_ch_is_category(c.kind == Kind::Category);
        ui.set_ch_private(private);
        ui.set_ch_read_only(read_only);
        ui.set_ch_roles(ModelRc::new(VecModel::from(toggles)));
        ui.set_ch_message(SharedString::new());
        ui.set_dialog("channel-settings".into());
    }

    pub(super) fn save_channel_settings(&mut self) {
        let (Some(channel_id), Some(guild), Some(ui)) = (self.settings_channel, self.guild, self.ui()) else { return };
        let Some(c) = self.channels.iter().find(|c| c.id == channel_id).cloned() else { return };
        let name = ui.get_ch_name().trim().to_owned();
        let (private, read_only) = (ui.get_ch_private(), ui.get_ch_read_only() && matches!(c.kind, Kind::Text));
        let toggles = ui.get_ch_roles();
        // Что отправить: права @everyone и ролей. Пустое (0/0) — убрать настройку
        let mut ops: Vec<(Snowflake, u64, u64)> = Vec::new();
        if c.kind != Kind::Thread {
            let deny = if private { perms::VIEW_CHANNEL } else { 0 } | if read_only { perms::SEND_MESSAGES } else { 0 };
            ops.push((guild, 0, deny));
            for (i, role) in self.settings_roles.iter().enumerate() {
                let Some(t) = toggles.row_data(i) else { continue };
                let allow = if private && t.view { perms::VIEW_CHANNEL } else { 0 } | if read_only && t.send { perms::SEND_MESSAGES } else { 0 };
                ops.push((*role, allow, 0));
            }
        }
        // Не трогаем то, что и так совпадает
        let current = |id: Snowflake| c.overwrites.iter().find(|o| o.id == id).map(|o| (o.allow, o.deny)).unwrap_or((0, 0));
        ops.retain(|(id, allow, deny)| current(*id) != (*allow, *deny));
        let rename = (!name.is_empty() && name != c.name).then_some(name);
        let api = self.api.clone();
        self.rt.spawn(async move {
            let mut error = None;
            if let Some(name) = rename
                && let Err(e) = api.update_channel(channel_id, &UpdateChannelRequest { name: Some(name), position: None }).await
            {
                error = Some(e.to_string());
            }
            for (target, allow, deny) in ops {
                let res = if allow == 0 && deny == 0 {
                    api.delete_overwrite(channel_id, target).await
                } else {
                    api.put_overwrite(channel_id, target, &OverwriteRequest { kind: OverwriteKind::Role, allow, deny }).await
                };
                if let Err(e) = res {
                    error = Some(e.to_string());
                    break;
                }
            }
            post(move |app| match error {
                None => {
                    if let Some(ui) = app.ui() {
                        ui.set_dialog(SharedString::new());
                    }
                    // Права изменились — каналы перезапросим (придёт и событие, но так быстрее)
                    app.reload_channels();
                }
                Some(e) => {
                    if let Some(ui) = app.ui() {
                        ui.set_ch_message(e.into());
                    }
                }
            });
        });
    }

    pub(super) fn delete_channel(&mut self) {
        let Some(channel_id) = self.settings_channel else { return };
        // Второе нажатие подтверждает: первое — предупреждение
        let Some(ui) = self.ui() else { return };
        if ui.get_ch_message() != "Нажмите «Удалить» ещё раз: канал и все его сообщения исчезнут навсегда" {
            ui.set_ch_message("Нажмите «Удалить» ещё раз: канал и все его сообщения исчезнут навсегда".into());
            return;
        }
        let api = self.api.clone();
        self.rt.spawn(async move {
            let res = api.delete_channel(channel_id).await;
            post(move |app| match res {
                Ok(()) => {
                    if let Some(ui) = app.ui() {
                        ui.set_dialog(SharedString::new());
                    }
                }
                Err(e) => {
                    if let Some(ui) = app.ui() {
                        ui.set_ch_message(e.to_string().into());
                    }
                }
            });
        });
    }
}

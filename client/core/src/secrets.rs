//! Токен входа в системном хранилище ключей: Secret Service (KWallet, GNOME Keyring) на Linux,
//! диспетчер учётных данных на Windows. Нет хранилища — токен остаётся в файле сессии (права 600).

const SERVICE: &str = "app.feather.Feather";

fn entry(account: &str) -> Option<keyring::Entry> {
    keyring::Entry::new(SERVICE, account).ok()
}

/// Сохранить; false — хранилище недоступно (тогда токен хранится в файле)
pub fn store(account: &str, token: &str) -> bool {
    entry(account).is_some_and(|e| e.set_password(token).is_ok())
}

pub fn load(account: &str) -> Option<String> {
    entry(account)?.get_password().ok()
}

pub fn forget(account: &str) {
    if let Some(e) = entry(account) {
        let _ = e.delete_credential();
    }
}

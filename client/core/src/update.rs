//! Проверка новой версии: последний релиз на GitHub. Ничего не скачивает сам —
//! только сообщает версию и ссылку на страницу релиза.

const RELEASES: &str = "https://api.github.com/repos/Per5903/feather/releases?per_page=5";

#[derive(serde::Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    draft: bool,
}

/// Версия «0.2.0» из тега «v0.2.0» в числа для сравнения
fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim_start_matches('v').split(['.', '-']).map(|p| p.parse::<u32>());
    Some((it.next()?.ok()?, it.next()?.ok()?, it.next()?.ok()?))
}

/// Новее ли `latest`, чем `current`
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// Последний релиз новее текущей версии: (версия без «v», адрес страницы)
pub async fn check(current: &str) -> Option<(String, String)> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(format!("Feather/{current}"))
        .build()
        .ok()?;
    let releases: Vec<Release> = http.get(RELEASES).send().await.ok()?.json().await.ok()?;
    let latest = releases.into_iter().find(|r| !r.draft)?;
    is_newer(&latest.tag_name, current).then(|| (latest.tag_name.trim_start_matches('v').to_owned(), latest.html_url))
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("nightly", "0.1.0"));
    }
}

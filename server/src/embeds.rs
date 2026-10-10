//! Превью ссылок: после отправки сообщения сервер скачивает начало страницы по первой ссылке,
//! достаёт заголовок и описание (Open Graph) и рассылает MESSAGE_UPDATE.
//!
//! Безопасность (SSRF): ходим только на публичные адреса — имя резолвим сами, проверяем IP
//! и подключаемся именно к нему; редиректы проверяем так же, не больше трёх. Отключается
//! переменной FEATHER_LINK_PREVIEWS=0 (тогда сервер не делает никаких внешних запросов).

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use feather_protocol::{Embed, Event, Message};

use crate::Shared;
use crate::access::Access;

const MAX_BODY: usize = 512 * 1024;
const TIMEOUT: Duration = Duration::from_secs(6);

pub fn spawn(st: Shared, access: Access, message: Message) {
    if !st.cfg.link_previews {
        return;
    }
    let Some(url) = first_url(&message.content) else { return };
    tokio::spawn(async move {
        let Some(embed) = fetch(&url).await else { return };
        let json = serde_json::to_string(&[&embed]).unwrap_or_default();
        // Сообщение могли удалить или изменить, пока мы ходили по ссылке
        let updated = sqlx::query("UPDATE messages SET embeds = $1 WHERE id = $2 AND content = $3")
            .bind(&json)
            .bind(message.id.0 as i64)
            .bind(&message.content)
            .execute(&st.db)
            .await
            .map(|r| r.rows_affected() > 0)
            .unwrap_or(false);
        if updated && let Ok(m) = crate::messages::load_public(&st.db, message.channel_id, message.id).await {
            access.publish(&st.bus, Event::MessageUpdate(m));
        }
    });
}

/// Первая ссылка http(s) в тексте; ссылки в `<угловых скобках>` не разворачиваем (как в Discord)
pub fn first_url(text: &str) -> Option<String> {
    for (i, _) in text.match_indices("http") {
        let rest = &text[i..];
        if !(rest.starts_with("http://") || rest.starts_with("https://")) {
            continue;
        }
        if text[..i].ends_with('<') {
            continue;
        }
        let end = rest.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`')).unwrap_or(rest.len());
        let url = rest[..end].trim_end_matches(['.', ',', ')', '!', '?', ';', ':']);
        if url.len() > 10 && url.len() < 2000 {
            return Some(url.to_owned());
        }
    }
    None
}

/// Публичный ли адрес: не локальный, не частная сеть, не служебный
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (64..128).contains(&o[1])) // CGNAT
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
                || o[0] >= 240)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // уникальные локальные
                || (s[0] & 0xffc0) == 0xfe80) // link-local
        }
    }
}

async fn resolve_public(host: &str, port: u16) -> Option<SocketAddr> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.ok()?.collect();
    // Если хоть один адрес внутренний — не идём вовсе (защита от подмены DNS)
    if addrs.is_empty() || addrs.iter().any(|a| !is_public(a.ip())) {
        return None;
    }
    addrs.into_iter().next()
}

async fn fetch(url: &str) -> Option<Embed> {
    let mut current = reqwest::Url::parse(url).ok()?;
    for _ in 0..4 {
        if !matches!(current.scheme(), "http" | "https") {
            return None;
        }
        let host = current.host_str()?.to_owned();
        let port = current.port_or_known_default()?;
        let addr = resolve_public(&host, port).await?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .resolve(&host, addr)
            .user_agent("Mozilla/5.0 (compatible; FeatherBot/0.1; +https://github.com/Per5903/feather)")
            .build()
            .ok()?;
        let mut resp = client.get(current.clone()).header("accept", "text/html").send().await.ok()?;
        if resp.status().is_redirection() {
            let loc = resp.headers().get("location")?.to_str().ok()?;
            current = current.join(loc).ok()?;
            continue;
        }
        if !resp.status().is_success() {
            return None;
        }
        let html = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|t| t.contains("html"));
        if !html {
            return None;
        }
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_BODY {
                break;
            }
        }
        let page = String::from_utf8_lossy(&body);
        let embed = parse(url, &page);
        return (embed.title.is_some() || embed.description.is_some()).then_some(embed);
    }
    None
}

/// Заголовок и описание из <meta property="og:…"> или <title>
pub fn parse(url: &str, html: &str) -> Embed {
    let head = match html.find("</head>") {
        Some(end) => &html[..end],
        None => html,
    };
    let meta = |names: &[&str]| -> Option<String> {
        let lower = head.to_ascii_lowercase();
        for name in names {
            let mut from = 0;
            while let Some(pos) = lower[from..].find("<meta") {
                let start = from + pos;
                let end = lower[start..].find('>').map(|e| start + e).unwrap_or(lower.len());
                let tag = &head[start..end];
                let tag_lower = &lower[start..end];
                from = end;
                let named = [format!("property=\"{name}\""), format!("name=\"{name}\""), format!("property='{name}'"), format!("name='{name}'")]
                    .iter()
                    .any(|n| tag_lower.contains(n.as_str()));
                if named && let Some(v) = attr(tag, tag_lower, "content") {
                    let v = clean(&v, 300);
                    if !v.is_empty() {
                        return Some(v);
                    }
                }
            }
        }
        None
    };
    let title = meta(&["og:title", "twitter:title"]).or_else(|| {
        let lower = head.to_ascii_lowercase();
        let s = lower.find("<title")?;
        let s = s + lower[s..].find('>')? + 1;
        let e = s + lower[s..].find("</title>")?;
        Some(clean(&head[s..e], 300)).filter(|t| !t.is_empty())
    });
    Embed {
        url: url.to_owned(),
        site_name: meta(&["og:site_name"]),
        title,
        description: meta(&["og:description", "twitter:description", "description"]),
    }
}

fn attr(tag: &str, tag_lower: &str, name: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let key = format!("{name}={quote}");
        if let Some(p) = tag_lower.find(&key) {
            let s = p + key.len();
            let e = s + tag[s..].find(quote)?;
            return Some(tag[s..e].to_owned());
        }
    }
    None
}

/// HTML-сущности, лишние пробелы, ограничение длины
fn clean(s: &str, max: usize) -> String {
    let s = s
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ");
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls() {
        assert_eq!(first_url("смотри https://example.com/a?b=1, круто").as_deref(), Some("https://example.com/a?b=1"));
        assert_eq!(first_url("без превью <https://example.com/x>"), None);
        assert_eq!(first_url("нет ссылок"), None);
    }

    #[test]
    fn blocks_private_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.5.4", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "::ffff:127.0.0.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} должен быть закрыт");
        }
        assert!(is_public("1.1.1.1".parse().unwrap()));
        assert!(is_public("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn parses_open_graph() {
        let html = r#"<html><head><title>Запасной</title>
            <meta property="og:title" content="Dota 2 &amp; патч">
            <meta name='description' content='Описание   страницы'>
            <meta property="og:site_name" content="Valve"></head><body>…</body></html>"#;
        let e = parse("https://x.test", html);
        assert_eq!(e.title.as_deref(), Some("Dota 2 & патч"));
        assert_eq!(e.description.as_deref(), Some("Описание страницы"));
        assert_eq!(e.site_name.as_deref(), Some("Valve"));
    }
}

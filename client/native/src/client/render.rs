//! Текст сообщения → разметка для StyledText: **жирный**, *курсив*, ~~зачёркнутый~~, `код`,
//! ссылки и подсветка @упоминаний. Опасного в разметке Slint нет (картинок и скриптов она не умеет),
//! но HTML-теги пользователя экранируем: иначе `<font>` из чужого сообщения перекрасит текст.

/// Ссылка http(s) с этой позиции: длина в байтах
fn url_len(s: &str) -> Option<usize> {
    if !(s.starts_with("https://") || s.starts_with("http://")) {
        return None;
    }
    let end = s.find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '`')).unwrap_or(s.len());
    let url = s[..end].trim_end_matches(['.', ',', ')', '!', '?', ';', ':', '*', '_', '~']);
    (url.len() > "https://".len()).then_some(url.len())
}

/// Есть ли смысл рисовать текст разметкой (иначе — обычный Text: он лучше переносит строки)
pub fn needs_markup(text: &str, mention_names: &[String]) -> bool {
    text.contains(['*', '_', '`', '~'])
        || text.contains("http://")
        || text.contains("https://")
        || mention_names.iter().any(|n| contains_mention(text, n))
}

fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// «@имя» целым словом, без учёта регистра
pub fn contains_mention(text: &str, username: &str) -> bool {
    let (text, needle) = (text.to_lowercase(), format!("@{}", username.to_lowercase()));
    text.match_indices(&needle).any(|(i, _)| text[i + needle.len()..].chars().next().is_none_or(|c| !is_name_char(c)))
}

/// Экранировать символы, которые в CommonMark начинают блоки и HTML, но оставить
/// встроенную разметку (*, _, `, ~) — её пользователь пишет намеренно
fn escape_text(s: &str, at_line_start: &mut bool) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '<' | '>' | '&' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            // В начале строки: заголовки, цитаты, списки «- » и «+ » — показываем как есть
            '#' | '-' | '+' | '=' | '|' if *at_line_start => {
                out.push('\\');
                out.push(c);
            }
            '\n' => {
                out.push_str("  \n");
                *at_line_start = true;
                continue;
            }
            _ => out.push(c),
        }
        if !c.is_whitespace() {
            *at_line_start = false;
        }
    }
    out
}

/// Разметка сообщения. `mentions` — имена (логины) участников, `tint_of` — цвет упоминания для имени
pub fn markup(text: &str, mentions: &[(String, String)], edited: bool) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    let mut i = 0;
    let mut at_line_start = true;
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut String, at_line_start: &mut bool| {
        if !plain.is_empty() {
            out.push_str(&escape_text(plain, at_line_start));
            plain.clear();
        }
    };
    while i < text.len() {
        let rest = &text[i..];
        if let Some(n) = url_len(rest) {
            flush(&mut plain, &mut out, &mut at_line_start);
            out.push('<');
            out.push_str(&rest[..n]);
            out.push('>');
            at_line_start = false;
            i += n;
            continue;
        }
        if rest.starts_with('@') && plain.chars().last().is_none_or(|c| !is_name_char(c)) {
            let candidate: String = rest[1..].chars().take_while(|c| is_name_char(*c)).collect();
            if let Some((_, color)) = mentions.iter().find(|(n, _)| !candidate.is_empty() && n.to_lowercase() == candidate.to_lowercase()) {
                flush(&mut plain, &mut out, &mut at_line_start);
                out.push_str(&format!("<font color=\"{color}\">**@{}**</font>", escape_text(&candidate, &mut false)));
                at_line_start = false;
                i += 1 + candidate.len();
                continue;
            }
        }
        let c = rest.chars().next().unwrap_or(' ');
        plain.push(c);
        i += c.len_utf8();
    }
    flush(&mut plain, &mut out, &mut at_line_start);
    if edited {
        out.push_str(" <font color=\"#8e9297\">(изменено)</font>");
    }
    out
}

/// Первая строка, не длиннее `max` символов — для цитаты ответа и превью
pub fn snippet(text: &str, max: usize) -> String {
    // Без символов разметки: в цитате и списках «**жирно**» выглядит мусором
    let clean: String = text.chars().filter(|c| !matches!(c, '*' | '`' | '~')).collect();
    let text = clean.as_str();
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > max {
        let mut s: String = line.chars().take(max - 1).collect();
        s.push('…');
        s
    } else if text.lines().count() > 1 {
        format!("{line} …")
    } else {
        line.to_owned()
    }
}

/// «@начало» в конце поля ввода: что сейчас набирают (для подсказок)
pub fn mention_prefix(text: &str) -> Option<&str> {
    let at = text.rfind('@')?;
    let before = text[..at].chars().last();
    if before.is_some_and(is_name_char) {
        return None; // это e-mail, а не упоминание
    }
    let prefix = &text[at + 1..];
    prefix.chars().all(is_name_char).then_some(prefix)
}

/// QR-код в картинку: чёрные модули на белом, `scale` пикселей на модуль, поле в 2 модуля
pub fn qr_pixels(data: &str, scale: u32) -> Option<(u32, Vec<u8>)> {
    let code = qrcode::QrCode::new(data.as_bytes()).ok()?;
    let n = code.width() as u32;
    let colors = code.to_colors();
    let side = (n + 4) * scale;
    let mut px = vec![255u8; (side * side * 3) as usize];
    for y in 0..n {
        for x in 0..n {
            if colors[(y * n + x) as usize] == qrcode::Color::Dark {
                for dy in 0..scale {
                    for dx in 0..scale {
                        let (px_x, px_y) = ((x + 2) * scale + dx, (y + 2) * scale + dy);
                        let off = ((px_y * side + px_x) * 3) as usize;
                        px[off..off + 3].copy_from_slice(&[0, 0, 0]);
                    }
                }
            }
        }
    }
    Some((side, px))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Vec<(String, String)> {
        vec![("Шторм".into(), "#6cb6ff".into())]
    }

    #[test]
    fn links_become_autolinks() {
        assert_eq!(markup("см. https://example.com/a?b=1.", &[], false), "см. <https://example.com/a?b=1>.");
    }

    #[test]
    fn html_and_blocks_are_escaped() {
        assert_eq!(markup("<font color=red>x</font>", &[], false), "\\<font color=red\\>x\\</font\\>");
        assert_eq!(markup("# не заголовок", &[], false), "\\# не заголовок");
        assert_eq!(markup("- пункт", &[], false), "\\- пункт");
        assert_eq!(markup("a [b](c)", &[], false), "a \\[b\\](c)");
    }

    #[test]
    fn emphasis_kept_and_mentions_colored() {
        assert_eq!(markup("**жирно** @шторм!", &m(), false), "**жирно** <font color=\"#6cb6ff\">**@шторм**</font>!");
        assert_eq!(markup("mail@шторм", &m(), false), "mail@шторм", "не упоминание");
        assert!(markup("ок", &[], true).ends_with("(изменено)</font>"));
    }

    #[test]
    fn markup_parses() {
        for text in ["**жирно** и *курсив*, `код`", "https://x.test/a_b", "<3 и > и & и \\", "1. список?\n# решётка", "@Шторм привет"] {
            let md = markup(text, &m(), true);
            assert!(slint::StyledText::from_markdown(&md).is_ok(), "не разобралось: {md}");
        }
    }

    #[test]
    fn mention_prefixes() {
        assert_eq!(mention_prefix("привет @шт"), Some("шт"));
        assert_eq!(mention_prefix("@"), Some(""));
        assert_eq!(mention_prefix("mail@x"), None);
        assert_eq!(mention_prefix("@шторм привет"), None);
    }

    #[test]
    fn snippets() {
        assert_eq!(snippet("коротко", 20), "коротко");
        assert_eq!(snippet("очень длинное сообщение", 10), "очень дли…");
        assert_eq!(snippet("первая\nвторая", 20), "первая …");
        assert_eq!(snippet("**рейд** в `21:00`", 30), "рейд в 21:00");
    }

    #[test]
    fn qr() {
        let (side, px) = qr_pixels("otpauth://totp/Feather:alice?secret=ABC", 4).unwrap();
        assert_eq!(px.len() as u32, side * side * 3);
    }
}

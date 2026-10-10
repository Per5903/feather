//! Мелочи для отображения: цвета, инициалы, время.

use chrono::{Local, TimeZone};
use feather_protocol::Snowflake;
use slint::{Color, SharedString};

pub const PALETTE: &[u32] = &[0xe8a14b, 0x6cb6ff, 0x7ee08a, 0xf07178, 0xc792ea, 0x5ccfe6, 0xffcb6b];

pub fn color(rgb: u32) -> Color {
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Цвет аватарки зависит от ID: у человека он одинаковый на всех устройствах
pub fn user_color(id: Snowflake) -> Color {
    // Младшие биты Snowflake — счётчик, поэтому перемешиваем
    let h = id.0.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 59;
    color(PALETTE[h as usize % PALETTE.len()])
}

pub fn initial(name: &str) -> SharedString {
    name.chars().next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default().into()
}

/// Время по часовому поясу системы: «14:05», для вчерашних и старше — «09.10 14:05»
pub fn time_label(ts_ms: i64) -> SharedString {
    let Some(t) = Local.timestamp_millis_opt(ts_ms).single() else { return SharedString::new() };
    if t.date_naive() == Local::now().date_naive() {
        t.format("%H:%M").to_string().into()
    } else {
        t.format("%d.%m %H:%M").to_string().into()
    }
}

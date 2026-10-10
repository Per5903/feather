//! Двухфакторная защита: одноразовые коды TOTP (RFC 6238), как в Google Authenticator,
//! Aegis, 2FAS и т.п. — шесть цифр, меняются каждые 30 секунд.

use hmac::{Hmac, Mac};
use sha1::Sha1;

const STEP: u64 = 30;
const DIGITS: u32 = 6;
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// Новый секрет: 20 случайных байт в base32 (так его понимают все приложения)
pub fn new_secret() -> String {
    let mut buf = [0u8; 20];
    getrandom::fill(&mut buf).expect("нет источника случайности");
    base32_encode(&buf)
}

/// Ссылка для QR-кода
pub fn uri(secret: &str, username: &str, issuer: &str) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect()
    };
    format!("otpauth://totp/{}:{}?secret={secret}&issuer={}&digits={DIGITS}&period={STEP}", enc(issuer), enc(username), enc(issuer))
}

/// Код верный для текущего 30-секундного окна или соседних (часы телефона могут спешить)
pub fn check(secret: &str, code: &str) -> bool {
    let Some(key) = base32_decode(secret) else { return false };
    let Ok(code) = code.trim().replace(' ', "").parse::<u32>() else { return false };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let counter = now / STEP;
    [counter.wrapping_sub(1), counter, counter + 1].iter().any(|&c| hotp(&key, c) == code)
}

fn hotp(key: &[u8], counter: u64) -> u32 {
    let mut mac = Hmac::<Sha1>::new_from_slice(key).expect("HMAC принимает ключ любой длины");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let bin = u32::from_be_bytes([h[off] & 0x7f, h[off + 1], h[off + 2], h[off + 3]]);
    bin % 10u32.pow(DIGITS)
}

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(BASE32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(BASE32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0);
    for c in s.trim_end_matches('=').chars().filter(|c| !c.is_whitespace()) {
        let v = BASE32.iter().position(|&b| b as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_vectors() {
        // RFC 4226, приложение D: ключ "12345678901234567890"
        let key = b"12345678901234567890";
        let expected = [755224, 287082, 359152, 969429, 338314];
        for (i, e) in expected.iter().enumerate() {
            assert_eq!(hotp(key, i as u64), *e);
        }
        // RFC 6238: T = 59 с → 94287082 (8 цифр) → последние 6 = 287082
        assert_eq!(hotp(key, 59 / STEP), 287082);
    }

    #[test]
    fn base32_roundtrip() {
        let secret = new_secret();
        assert_eq!(secret.len(), 32);
        assert_eq!(base32_decode(&secret).unwrap().len(), 20);
        assert_eq!(base32_encode(b"12345678901234567890"), "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ");
    }
}

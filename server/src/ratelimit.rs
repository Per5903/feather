//! Ограничение частоты запросов (token bucket) в памяти процесса.
//! Для одного сервера этого достаточно; в кластере счётчики переедут в NATS KV / Redis.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::request::Parts;

use crate::Shared;
use crate::error::AppError;

/// Правило: `burst` запросов подряд, затем `per_sec` в секунду
#[derive(Clone, Copy)]
pub struct Rule {
    pub burst: f64,
    pub per_sec: f64,
}

/// Вход и регистрация: перебор паролей должен быть медленным
pub const AUTH: Rule = Rule { burst: 10.0, per_sec: 10.0 / 60.0 };
/// Сообщения: 5 подряд, дальше одно в секунду
pub const MESSAGES: Rule = Rule { burst: 5.0, per_sec: 1.0 };

#[derive(Default)]
pub struct Limiter {
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl Limiter {
    /// Ok — можно; Err — сколько подождать
    pub fn check(&self, key: &str, rule: Rule) -> Result<(), Duration> {
        let now = Instant::now();
        let mut map = self.buckets.lock().unwrap();
        // Вёдра, не трогавшиеся 10 минут, уже полные — их можно забыть, чтобы карта не росла
        if map.len() > 10_000 {
            map.retain(|_, (_, at)| at.elapsed() < Duration::from_secs(600));
        }
        let (tokens, at) = map.entry(key.to_owned()).or_insert((rule.burst, now));
        *tokens = (*tokens + now.duration_since(*at).as_secs_f64() * rule.per_sec).min(rule.burst);
        *at = now;
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - *tokens) / rule.per_sec))
        }
    }
}

/// IP клиента. За обратным прокси (Caddy) — из X-Forwarded-For, но только если
/// FEATHER_TRUST_PROXY=1: иначе любой мог бы подделать заголовок и обойти лимит.
pub struct ClientIp(pub IpAddr);

impl FromRequestParts<Shared> for ClientIp {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, st: &Shared) -> Result<Self, Self::Rejection> {
        if st.cfg.trust_proxy
            && let Some(ip) = parts
                .headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.split(',').next())
                .and_then(|v| v.trim().parse().ok())
        {
            return Ok(ClientIp(ip));
        }
        let ConnectInfo(addr) = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .cloned()
            .ok_or_else(|| AppError::Internal("нет адреса клиента".into()))?;
        Ok(ClientIp(addr.ip()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_allows_burst_then_limits() {
        let l = Limiter::default();
        let rule = Rule { burst: 3.0, per_sec: 1.0 };
        for _ in 0..3 {
            assert!(l.check("k", rule).is_ok());
        }
        let wait = l.check("k", rule).unwrap_err();
        assert!(wait <= Duration::from_secs(1));
        assert!(l.check("другой", rule).is_ok(), "у каждого ключа своё ведро");
    }
}

//! Выбор криптобэкенда TLS для всего процесса.
//!
//! В клиенте одновременно есть aws-lc-rs (reqwest) и ring (LiveKit, WebSocket). Если оба
//! включены, rustls не выбирает сам и паникует на первом защищённом соединении — например,
//! при подключении шлюза к wss://. Найдено на настоящем сервере (VPS), локально по ws:// не видно.

/// Вызвать один раз в самом начале программы (повторный вызов безопасен)
pub fn init() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn wss_does_not_panic_after_init() {
        super::init();
        // Порт закрыт: соединение должно завершиться ошибкой, а не паникой выбора криптобэкенда
        let res = tokio_tungstenite::connect_async("wss://127.0.0.1:9/gateway").await;
        assert!(res.is_err());
    }
}

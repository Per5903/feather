use feather_server::{Config, build};
use tracing_subscriber::EnvFilter;

/// `feather-server healthcheck` — для HEALTHCHECK в Docker без curl и shell
fn healthcheck() -> std::process::ExitCode {
    use std::io::{Read, Write};
    let port = Config::from_env().bind.port();
    let ok = (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        s.write_all(b"GET /health HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut resp = String::new();
        s.read_to_string(&mut resp)?;
        Ok(resp.starts_with("HTTP/1.0 200") || resp.starts_with("HTTP/1.1 200"))
    })()
    .unwrap_or(false);
    if ok { std::process::ExitCode::SUCCESS } else { std::process::ExitCode::FAILURE }
}

fn main() -> std::process::ExitCode {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
    }
    serve();
    std::process::ExitCode::SUCCESS
}

#[tokio::main]
async fn serve() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "feather_server=info".into()))
        .init();
    let cfg = Config::from_env();
    let bind = cfg.bind;
    if !bind.ip().is_loopback() && cfg.livekit_api_secret.starts_with("dev_secret") {
        tracing::warn!("ВНИМАНИЕ: тестовый секрет LiveKit на открытом адресе — задайте LIVEKIT_API_SECRET (deploy/selfhost/setup.sh)");
    }
    let app = build(cfg).await.expect("не удалось подключиться к БД или применить миграции");
    let listener = tokio::net::TcpListener::bind(bind).await.expect("не удалось открыть порт");
    tracing::info!("feather-server слушает http://{bind}");
    // ConnectInfo: IP клиента для ограничения частоты запросов
    axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await.expect("сервер упал");
}

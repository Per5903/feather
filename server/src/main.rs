use feather_server::{Config, build};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "feather_server=info".into()))
        .init();
    let cfg = Config::from_env();
    let bind = cfg.bind;
    let app = build(cfg).await.expect("не удалось подключиться к БД или применить миграции");
    let listener = tokio::net::TcpListener::bind(bind).await.expect("не удалось открыть порт");
    tracing::info!("feather-server слушает http://{bind}");
    axum::serve(listener, app).await.expect("сервер упал");
}

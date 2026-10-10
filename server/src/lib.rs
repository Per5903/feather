//! feather-server: REST API + realtime-шлюз + выдача токенов LiveKit (docs/03-architecture.md).

mod access;
mod auth;
mod bus;
pub mod config;
mod dms;
mod error;
mod gateway;
mod guilds;
mod ids;
mod messages;
pub(crate) mod ratelimit;
mod voice;
mod voice_states;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, patch, post};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use config::Config;

pub struct AppState {
    pub db: PgPool,
    pub cfg: Config,
    ids: ids::Snowflakes,
    bus: bus::Bus,
    voice: voice_states::VoiceStates,
    limiter: ratelimit::Limiter,
}

pub type Shared = Arc<AppState>;

/// Подключается к БД, применяет миграции и собирает приложение
pub async fn build(cfg: Config) -> Result<Router, sqlx::Error> {
    let db = PgPoolOptions::new().max_connections(20).connect(&cfg.database_url).await?;
    sqlx::migrate!("./migrations").run(&db).await?;
    let state = Arc::new(AppState {
        ids: ids::Snowflakes::new(cfg.worker_id),
        bus: bus::Bus::new(),
        voice: Default::default(),
        limiter: Default::default(),
        db,
        cfg,
    });
    Ok(router(state))
}

fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/users/@me", get(auth::me))
        .route("/users/@me/guilds", get(guilds::my_guilds))
        .route("/users/@me/channels", get(dms::list_dms).post(dms::open_dm))
        .route("/guilds", post(guilds::create_guild))
        .route("/guilds/{id}", get(guilds::get_guild))
        .route("/guilds/{id}/channels", get(guilds::list_channels).post(guilds::create_channel))
        .route("/guilds/{id}/members", get(guilds::list_members))
        .route("/guilds/{id}/invites", post(guilds::create_invite))
        .route("/invites/{code}", post(guilds::accept_invite))
        .route("/channels/{id}/messages", get(messages::history).post(messages::send))
        .route("/channels/{id}/messages/{mid}", patch(messages::edit).delete(messages::delete))
        .route("/channels/{id}/ack", post(messages::ack))
        .route("/channels/{id}/voice/join", post(voice::join));
    Router::new()
        .nest("/api/v1", api)
        .route("/gateway", get(gateway::upgrade))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

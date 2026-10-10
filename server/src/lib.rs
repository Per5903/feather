//! feather-server: REST API + realtime-шлюз + выдача токенов LiveKit (docs/03-architecture.md).

mod access;
mod account;
mod admin;
mod attachments;
mod audit;
mod auth;
mod bus;
pub mod config;
mod dms;
mod embeds;
mod error;
mod gateway;
mod guilds;
mod ids;
mod messages;
mod roles;
pub(crate) mod ratelimit;
mod totp;
mod voice;
mod voice_states;

use std::sync::Arc;

use axum::Router;
use axum::routing::{delete, get, patch, post, put};
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
    attachments::spawn_orphan_cleanup(state.clone());
    Ok(router(state))
}

fn router(state: Shared) -> Router {
    let api = Router::new()
        .route("/instance", get(auth::instance))
        .route("/auth/register", post(auth::register))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/users/@me", get(auth::me).patch(account::update_me).delete(account::delete_me))
        .route("/users/@me/password", post(account::change_password))
        .route(
            "/users/@me/avatar",
            put(account::put_avatar).delete(account::delete_avatar).layer(axum::extract::DefaultBodyLimit::max(account::MAX_AVATAR + 1024)),
        )
        .route("/users/@me/2fa", get(account::totp_status).delete(account::totp_disable))
        .route("/users/@me/2fa/setup", post(account::totp_setup))
        .route("/users/@me/2fa/enable", post(account::totp_enable))
        .route("/avatars/{uid}/{version}", get(account::get_avatar))
        .route("/users/@me/guilds", get(guilds::my_guilds))
        .route("/users/@me/channels", get(dms::list_dms).post(dms::open_dm))
        .route("/guilds", post(guilds::create_guild))
        .route("/guilds/{id}", get(guilds::get_guild).patch(guilds::update_guild).delete(guilds::delete_guild))
        .route("/guilds/{id}/channels", get(guilds::list_channels).post(guilds::create_channel))
        .route("/guilds/{id}/members", get(roles::list_members))
        .route("/guilds/{id}/members/{uid}", delete(roles::kick))
        .route("/guilds/{id}/members/{uid}/roles/{rid}", put(roles::add_member_role).delete(roles::remove_member_role))
        .route("/guilds/{id}/roles", get(roles::list_roles).post(roles::create_role))
        .route("/guilds/{id}/roles/{rid}", patch(roles::update_role).delete(roles::delete_role))
        .route("/guilds/{id}/bans", get(roles::list_bans))
        .route("/guilds/{id}/bans/{uid}", put(roles::ban).delete(roles::unban))
        .route("/guilds/{id}/invites", post(guilds::create_invite))
        .route("/guilds/{id}/audit-log", get(audit::list))
        .route("/guilds/{id}/search", get(messages::search_guild))
        .route("/invites/{code}", post(guilds::accept_invite))
        .route("/channels/{id}", patch(guilds::update_channel).delete(guilds::delete_channel))
        .route("/channels/{id}/overwrites/{target}", put(roles::put_overwrite).delete(roles::delete_overwrite))
        .route("/channels/{id}/messages", get(messages::history).post(messages::send))
        .route("/channels/{id}/messages/{mid}", patch(messages::edit).delete(messages::delete))
        .route("/channels/{id}/messages/{mid}/reactions/{emoji}", put(messages::add_reaction).delete(messages::remove_reaction))
        .route("/channels/{id}/messages/{mid}/threads", post(guilds::create_thread))
        .route("/channels/{id}/pins", get(messages::list_pins))
        .route("/channels/{id}/pins/{mid}", put(messages::pin_message).delete(messages::unpin_message))
        .route("/channels/{id}/search", get(messages::search_channel))
        .route("/channels/{id}/ack", post(messages::ack))
        .route(
            "/channels/{id}/attachments",
            post(attachments::upload).layer(axum::extract::DefaultBodyLimit::max(attachments::MAX_SIZE + 1024)),
        )
        .route("/attachments/{id}", get(attachments::download))
        .route("/attachments/{id}/thumb", get(attachments::thumbnail))
        .route("/channels/{id}/voice/join", post(voice::join))
        .route("/admin/users", get(admin::list_users))
        .route("/admin/users/{id}", delete(admin::delete_user))
        .route("/admin/users/{id}/password-reset", post(admin::reset_password))
        .route("/admin/settings", get(admin::get_settings).put(admin::put_settings));
    Router::new()
        .nest("/api/v1", api)
        .route("/gateway", get(gateway::upgrade))
        .route("/health", get(|| async { "ok" }))
        .with_state(state)
}

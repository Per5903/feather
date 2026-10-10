//! Вход в голосовой канал: сервер проверяет права и выдаёт токен LiveKit.
//! Сам звук идёт напрямую клиент ↔ LiveKit, мимо feather-server (docs/06-voice.md).

use std::time::Duration;

use axum::extract::{Path, State};
use axum::Json;
use feather_protocol::{ChannelKind, Snowflake, VoiceJoinResponse, perms};
use livekit_api::access_token::{AccessToken, VideoGrants};

use crate::Shared;
use crate::auth::AuthUser;
use crate::error::{ApiResult, AppError};
use crate::guilds::channel_for_member;

/// Токен короткоживущий: для входа в комнату, дальше LiveKit держит соединение сам
const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);

pub async fn join(State(st): State<Shared>, user: AuthUser, Path(channel_id): Path<Snowflake>) -> ApiResult<Json<VoiceJoinResponse>> {
    let (channel, _) = channel_for_member(&st.db, channel_id, user.id()).await?;
    if channel.kind != ChannelKind::Voice {
        return Err(AppError::BadRequest("not_voice_channel", "это не голосовой канал".into()));
    }
    let p = crate::roles::channel_perms(&st.db, &channel, user.id()).await?.require(perms::VOICE_CONNECT)?;
    // Без права «говорить» — только слушать: LiveKit не даст опубликовать микрофон
    let can_speak = p.has(perms::VOICE_SPEAK);
    let room = format!("voice:{channel_id}");
    let token = AccessToken::with_api_key(&st.cfg.livekit_api_key, &st.cfg.livekit_api_secret)
        .with_identity(&user.id().to_string())
        .with_name(user.0.shown_name())
        .with_ttl(TOKEN_TTL)
        .with_grants(VideoGrants {
            room_join: true,
            room: room.clone(),
            can_publish: Some(can_speak),
            can_subscribe: Some(true),
            ..Default::default()
        })
        .to_jwt()
        .map_err(|e| AppError::Internal(format!("токен LiveKit: {e}")))?;
    Ok(Json(VoiceJoinResponse { url: st.cfg.livekit_url.clone(), token, room }))
}

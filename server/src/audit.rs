//! Журнал модерации: кто, что и с кем сделал на сервере. Читать могут те, у кого есть MANAGE_GUILD.

use axum::Json;
use axum::extract::{Path, Query, State};
use feather_protocol::{AuditEntry, Snowflake, perms};
use serde::Deserialize;

use crate::Shared;
use crate::auth::{AuthUser, user_from_row};
use crate::error::ApiResult;

/// Записать действие. Ошибка записи не должна ломать само действие — только в лог
pub async fn log(st: &Shared, guild: Snowflake, actor: Snowflake, action: &str, target: Option<Snowflake>, details: impl Into<String>) {
    let details: String = details.into().chars().take(200).collect();
    let res = sqlx::query("INSERT INTO audit_log (id, guild_id, actor_id, action, target_id, details) VALUES ($1, $2, $3, $4, $5, $6)")
        .bind(st.ids.next().0 as i64)
        .bind(guild.0 as i64)
        .bind(actor.0 as i64)
        .bind(action)
        .bind(target.map(|t| t.0 as i64))
        .bind(details)
        .execute(&st.db)
        .await;
    if let Err(e) = res {
        tracing::warn!("журнал модерации: {e}");
    }
}

#[derive(Deserialize)]
pub struct AuditQuery {
    before: Option<Snowflake>,
}

pub async fn list(State(st): State<Shared>, user: AuthUser, Path(guild): Path<Snowflake>, Query(q): Query<AuditQuery>) -> ApiResult<Json<Vec<AuditEntry>>> {
    crate::roles::member_perms(&st.db, guild, user.id()).await?.require(perms::MANAGE_GUILD)?;
    let rows: Vec<(i64, String, Option<i64>, String, i64, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT a.id, a.action, a.target_id, a.details, u.id, u.username, u.display_name, u.avatar_id
           FROM audit_log a JOIN users u ON u.id = a.actor_id
          WHERE a.guild_id = $1 AND ($2::bigint IS NULL OR a.id < $2)
          ORDER BY a.id DESC LIMIT 50",
    )
    .bind(guild.0 as i64)
    .bind(q.before.map(|b| b.0 as i64))
    .fetch_all(&st.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(id, action, target, details, uid, un, dn, av)| AuditEntry {
                id: Snowflake(id as u64),
                actor: user_from_row((uid, un, dn, av)),
                action,
                target_id: target.map(|t| Snowflake(t as u64)),
                details,
            })
            .collect(),
    ))
}

//! Вложения: загрузка, превью картинок, отдача файлов.
//!
//! Файл загружается отдельным запросом и сразу попадает на диск (`data/attachments/<id>`),
//! для картинок сервер делает превью JPEG до 400×300 — клиент качает оригинал только по клику
//! (docs/02-performance.md). Отдаём файлы только тем, у кого есть доступ к каналу.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use feather_protocol::{Attachment, Snowflake};
use serde::Deserialize;
use sqlx::PgPool;

use crate::Shared;
use crate::access::channel_access;
use crate::auth::AuthUser;
use crate::error::{ApiResult, AppError};
use crate::ratelimit;

/// Максимальный размер файла
pub const MAX_SIZE: usize = 25 * 1024 * 1024;
const THUMB_W: u32 = 400;
const THUMB_H: u32 = 300;

type Row = (i64, String, i64, String, Option<i32>, Option<i32>, bool);

pub fn from_row((id, filename, size, content_type, w, h, thumb): Row) -> Attachment {
    Attachment {
        id: Snowflake(id as u64),
        filename,
        size: size as u64,
        content_type,
        width: w.map(|v| v as u32),
        height: h.map(|v| v as u32),
        has_thumb: thumb,
    }
}

fn dir(st: &Shared) -> std::path::PathBuf {
    st.cfg.data_dir.join("attachments")
}

/// Имя файла без путей и управляющих символов
fn clean_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("file");
    let s: String = base.chars().filter(|c| !c.is_control()).take(120).collect();
    if s.trim().is_empty() { "file".into() } else { s }
}

#[derive(Deserialize)]
pub struct UploadQuery {
    filename: String,
}

/// Размеры и превью картинки (в отдельном потоке: декодирование тяжёлое)
fn probe_image(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    // Защита от «бомб»: огромные размеры при маленьком файле
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let (w, h) = (img.width(), img.height());
    let thumb = img.thumbnail(THUMB_W, THUMB_H).to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&thumb).ok()?;
    Some((w, h, out))
}

pub async fn upload(
    State(st): State<Shared>,
    user: AuthUser,
    Path(channel_id): Path<Snowflake>,
    Query(q): Query<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Attachment>)> {
    st.limiter.check(&format!("upload:{}", user.id()), ratelimit::UPLOADS).map_err(AppError::rate_limited)?;
    let access = channel_access(&st.db, channel_id, user.id()).await?;
    if !access.can_write() {
        return Err(AppError::Forbidden);
    }
    if body.is_empty() || body.len() > MAX_SIZE {
        return Err(AppError::BadRequest("bad_size", format!("файл: от 1 байта до {} МБ", MAX_SIZE / 1024 / 1024)));
    }
    let filename = clean_name(&q.filename);
    let declared = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");

    let bytes = body.clone();
    let probed = tokio::task::spawn_blocking(move || probe_image(&bytes)).await.map_err(|e| AppError::Internal(e.to_string()))?;
    // Тип для картинок определяем по содержимому, а не по заявлению клиента
    let content_type = match (&probed, image::guess_format(&body)) {
        (Some(_), Ok(fmt)) => fmt.to_mime_type().to_owned(),
        _ if declared.starts_with("image/") => "application/octet-stream".to_owned(),
        _ => declared.chars().take(100).collect(),
    };

    let id = st.ids.next();
    let dir = dir(&st);
    tokio::fs::create_dir_all(&dir).await.map_err(|e| AppError::Internal(format!("каталог вложений: {e}")))?;
    tokio::fs::write(dir.join(id.to_string()), &body).await.map_err(|e| AppError::Internal(format!("запись файла: {e}")))?;
    let (w, h, has_thumb) = match &probed {
        Some((w, h, thumb)) => {
            tokio::fs::write(dir.join(format!("{id}.thumb.jpg")), thumb).await.map_err(|e| AppError::Internal(format!("запись превью: {e}")))?;
            (Some(*w as i32), Some(*h as i32), true)
        }
        None => (None, None, false),
    };
    sqlx::query(
        "INSERT INTO attachments (id, channel_id, uploader_id, filename, size, content_type, width, height, has_thumb)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(id.0 as i64)
    .bind(channel_id.0 as i64)
    .bind(user.id().0 as i64)
    .bind(&filename)
    .bind(body.len() as i64)
    .bind(&content_type)
    .bind(w)
    .bind(h)
    .bind(has_thumb)
    .execute(&st.db)
    .await?;
    Ok((StatusCode::CREATED, Json(from_row((id.0 as i64, filename, body.len() as i64, content_type, w, h, has_thumb)))))
}

/// Привязать загруженные вложения к новому сообщению (только свои, этого канала, ещё не привязанные)
pub async fn attach(db: &mut sqlx::PgConnection, ids: &[Snowflake], message: Snowflake, channel: Snowflake, user: Snowflake) -> ApiResult<Vec<Attachment>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if ids.len() > 10 {
        return Err(AppError::BadRequest("too_many_attachments", "не больше 10 вложений".into()));
    }
    let raw: Vec<i64> = ids.iter().map(|i| i.0 as i64).collect();
    let rows: Vec<Row> = sqlx::query_as(
        "UPDATE attachments SET message_id = $1
          WHERE id = ANY($2) AND uploader_id = $3 AND channel_id = $4 AND message_id IS NULL
          RETURNING id, filename, size, content_type, width, height, has_thumb",
    )
    .bind(message.0 as i64)
    .bind(&raw)
    .bind(user.0 as i64)
    .bind(channel.0 as i64)
    .fetch_all(db)
    .await?;
    if rows.len() != ids.len() {
        return Err(AppError::BadRequest("bad_attachment", "вложение не найдено или уже отправлено".into()));
    }
    let mut out: Vec<Attachment> = rows.into_iter().map(from_row).collect();
    out.sort_by_key(|a| ids.iter().position(|i| *i == a.id));
    Ok(out)
}

/// Вложения для пачки сообщений: message_id → вложения
pub async fn for_messages(db: &PgPool, messages: &[i64]) -> ApiResult<std::collections::HashMap<i64, Vec<Attachment>>> {
    let mut map = std::collections::HashMap::new();
    if messages.is_empty() {
        return Ok(map);
    }
    let rows: Vec<(i64, i64, String, i64, String, Option<i32>, Option<i32>, bool)> = sqlx::query_as(
        "SELECT message_id, id, filename, size, content_type, width, height, has_thumb
           FROM attachments WHERE message_id = ANY($1) ORDER BY id",
    )
    .bind(messages)
    .fetch_all(db)
    .await?;
    for (mid, id, f, s, ct, w, h, t) in rows {
        map.entry(mid).or_insert_with(Vec::new).push(from_row((id, f, s, ct, w, h, t)));
    }
    Ok(map)
}

/// Удалить файлы вложений с диска (после удаления сообщения)
pub async fn remove_files(st: &Shared, ids: &[i64]) {
    let dir = dir(st);
    for id in ids {
        let _ = tokio::fs::remove_file(dir.join(id.to_string())).await;
        let _ = tokio::fs::remove_file(dir.join(format!("{id}.thumb.jpg"))).await;
    }
}

async fn load_for_download(st: &Shared, user: Snowflake, id: Snowflake) -> ApiResult<(String, String, bool)> {
    let row: Option<(i64, String, String, bool)> =
        sqlx::query_as("SELECT channel_id, filename, content_type, has_thumb FROM attachments WHERE id = $1 AND message_id IS NOT NULL")
            .bind(id.0 as i64)
            .fetch_optional(&st.db)
            .await?;
    let (channel, filename, content_type, thumb) = row.ok_or(AppError::NotFound("вложение"))?;
    channel_access(&st.db, Snowflake(channel as u64), user).await.map_err(|_| AppError::NotFound("вложение"))?;
    Ok((filename, content_type, thumb))
}

fn file_response(bytes: Vec<u8>, content_type: &str, filename: &str, inline: bool) -> Response {
    let disposition = format!(
        "{}; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        filename.bytes().map(|b| if b.is_ascii_alphanumeric() || b".-_".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>()
    );
    (
        [
            (header::CONTENT_TYPE, content_type.to_owned()),
            (header::CONTENT_DISPOSITION, disposition),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
            (header::CACHE_CONTROL, "private, max-age=31536000, immutable".to_owned()),
        ],
        bytes,
    )
        .into_response()
}

pub async fn download(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<Response> {
    let (filename, content_type, _) = load_for_download(&st, user.id(), id).await?;
    let bytes = tokio::fs::read(dir(&st).join(id.to_string())).await.map_err(|_| AppError::NotFound("файл"))?;
    // Показывать в браузере можно только картинки; остальное — только скачивание
    let inline = content_type.starts_with("image/");
    Ok(file_response(bytes, &content_type, &filename, inline))
}

pub async fn thumbnail(State(st): State<Shared>, user: AuthUser, Path(id): Path<Snowflake>) -> ApiResult<Response> {
    let (filename, _, thumb) = load_for_download(&st, user.id(), id).await?;
    if !thumb {
        return Err(AppError::NotFound("превью"));
    }
    let bytes = tokio::fs::read(dir(&st).join(format!("{id}.thumb.jpg"))).await.map_err(|_| AppError::NotFound("превью"))?;
    Ok(file_response(bytes, "image/jpeg", &filename, true))
}

/// Фоновая уборка: вложения, загруженные, но не отправленные за час, удаляются вместе с файлами
pub fn spawn_orphan_cleanup(st: Shared) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
        loop {
            tick.tick().await;
            let removed: Result<Vec<(i64,)>, _> = sqlx::query_as(
                "DELETE FROM attachments WHERE message_id IS NULL AND created_at < now() - interval '1 hour' RETURNING id",
            )
            .fetch_all(&st.db)
            .await;
            match removed {
                Ok(ids) if !ids.is_empty() => {
                    let ids: Vec<i64> = ids.into_iter().map(|(i,)| i).collect();
                    remove_files(&st, &ids).await;
                    tracing::info!("удалено брошенных вложений: {}", ids.len());
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("уборка вложений: {e}"),
            }
        }
    });
}

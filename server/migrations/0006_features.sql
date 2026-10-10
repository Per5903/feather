-- Аккаунт: аватар, двухфакторная защита, администратор сервера Feather, удалённые аккаунты.
ALTER TABLE users
    ADD COLUMN avatar_id    BIGINT,
    ADD COLUMN totp_secret  TEXT,
    ADD COLUMN totp_enabled BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN is_admin     BOOLEAN NOT NULL DEFAULT false,
    ADD COLUMN deleted      BOOLEAN NOT NULL DEFAULT false;
-- Администратор — первый зарегистрированный
UPDATE users SET is_admin = true WHERE id = (SELECT id FROM users ORDER BY created_at, id LIMIT 1);

-- Настройки сервера Feather из панели администратора (перекрывают переменные окружения)
CREATE TABLE instance_settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Ветки: текстовый канал внутри текстового
ALTER TABLE channels DROP CONSTRAINT channels_kind_check;
ALTER TABLE channels ADD CONSTRAINT channels_kind_check CHECK (kind IN ('text', 'voice', 'category', 'dm', 'thread'));

-- Права на уровне канала
CREATE TABLE channel_overwrites (
    channel_id BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    target_id  BIGINT NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN ('role', 'member')),
    allow      BIGINT NOT NULL DEFAULT 0,
    deny       BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (channel_id, target_id)
);

-- Сообщения: закреп, ветка, превью ссылок (JSON-массив Embed)
ALTER TABLE messages
    ADD COLUMN pinned_at TIMESTAMPTZ,
    ADD COLUMN thread_id BIGINT REFERENCES channels (id) ON DELETE SET NULL,
    ADD COLUMN embeds    TEXT;
CREATE INDEX messages_pinned ON messages (channel_id) WHERE pinned_at IS NOT NULL;

CREATE TABLE reactions (
    message_id BIGINT NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    emoji      TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (message_id, emoji, user_id)
);

-- Журнал модерации
CREATE TABLE audit_log (
    id        BIGINT PRIMARY KEY,
    guild_id  BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    actor_id  BIGINT NOT NULL REFERENCES users (id),
    action    TEXT NOT NULL,
    target_id BIGINT,
    details   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX audit_log_guild ON audit_log (guild_id, id DESC);

-- Новое право VIEW_CHANNEL (1<<11): у @everyone уже существующих серверов оно есть
UPDATE roles SET permissions = permissions | 2048 WHERE id = guild_id;

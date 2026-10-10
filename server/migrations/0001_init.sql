-- Этап 1, базовая схема (docs/04-data-model.md). ID — Snowflake (BIGINT).
-- Роли, права, вложения, реакции и E2EE добавляются следующими миграциями.

CREATE TABLE users (
    id            BIGINT PRIMARY KEY,
    username      TEXT NOT NULL,
    display_name  TEXT,
    password_hash TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
-- Имя уникально без учёта регистра: «Kerrigan» и «kerrigan» — один пользователь
CREATE UNIQUE INDEX users_username_lower ON users (lower(username));

-- Непрозрачные токены сессий: в базе только SHA-256 токена
CREATE TABLE sessions (
    token_hash   BYTEA PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    device_name  TEXT,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_user ON sessions (user_id);

CREATE TABLE guilds (
    id         BIGINT PRIMARY KEY,
    name       TEXT NOT NULL,
    owner_id   BIGINT NOT NULL REFERENCES users (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE guild_members (
    guild_id  BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    user_id   BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    nickname  TEXT,
    joined_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);
CREATE INDEX guild_members_user ON guild_members (user_id);

CREATE TABLE channels (
    id        BIGINT PRIMARY KEY,
    guild_id  BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    kind      TEXT NOT NULL CHECK (kind IN ('text', 'voice', 'category')),
    name      TEXT NOT NULL,
    parent_id BIGINT REFERENCES channels (id) ON DELETE SET NULL,
    position  INT NOT NULL DEFAULT 0
);
CREATE INDEX channels_guild ON channels (guild_id);

CREATE TABLE messages (
    id          BIGINT PRIMARY KEY,
    channel_id  BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    author_id   BIGINT NOT NULL REFERENCES users (id),
    content     TEXT NOT NULL,
    reply_to_id BIGINT,
    edited_at   TIMESTAMPTZ
);
-- Главный индекс чата: история канала, новые сверху, пагинация по id
CREATE INDEX messages_channel_id ON messages (channel_id, id DESC);

CREATE TABLE invites (
    code       TEXT PRIMARY KEY,
    guild_id   BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    creator_id BIGINT NOT NULL REFERENCES users (id),
    max_uses   INT,
    uses       INT NOT NULL DEFAULT 0,
    expires_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

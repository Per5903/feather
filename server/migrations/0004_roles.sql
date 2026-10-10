-- Роли и права (docs/04-data-model.md). Роль @everyone: id = id сервера, есть у всех участников.
CREATE TABLE roles (
    id          BIGINT PRIMARY KEY,
    guild_id    BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    color       INT NOT NULL DEFAULT 0,
    permissions BIGINT NOT NULL,
    position    INT NOT NULL DEFAULT 0
);
CREATE INDEX roles_guild ON roles (guild_id);

CREATE TABLE member_roles (
    guild_id BIGINT NOT NULL,
    user_id  BIGINT NOT NULL,
    role_id  BIGINT NOT NULL REFERENCES roles (id) ON DELETE CASCADE,
    PRIMARY KEY (guild_id, user_id, role_id),
    FOREIGN KEY (guild_id, user_id) REFERENCES guild_members (guild_id, user_id) ON DELETE CASCADE
);

CREATE TABLE bans (
    guild_id   BIGINT NOT NULL REFERENCES guilds (id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    banned_by  BIGINT NOT NULL REFERENCES users (id),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (guild_id, user_id)
);

-- @everyone для уже существующих серверов: приглашать, писать, голос (1<<6 | 1<<7 | 1<<9 | 1<<10)
INSERT INTO roles (id, guild_id, name, permissions, position)
SELECT id, id, '@everyone', 1728, 0 FROM guilds;

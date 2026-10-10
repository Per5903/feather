-- Личные сообщения: канал без сервера (guild_id NULL) с участниками в channel_recipients.
ALTER TABLE channels ALTER COLUMN guild_id DROP NOT NULL;
ALTER TABLE channels DROP CONSTRAINT channels_kind_check;
ALTER TABLE channels ADD CONSTRAINT channels_kind_check CHECK (kind IN ('text', 'voice', 'category', 'dm'));
-- У личного канала нет сервера, у остальных — обязательно есть
ALTER TABLE channels ADD CONSTRAINT channels_dm_has_no_guild CHECK ((kind = 'dm') = (guild_id IS NULL));

CREATE TABLE channel_recipients (
    channel_id BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    PRIMARY KEY (channel_id, user_id)
);
CREATE INDEX channel_recipients_user ON channel_recipients (user_id);

-- Один личный канал на пару: ключ «меньший id : больший id»
ALTER TABLE channels ADD COLUMN dm_key TEXT UNIQUE;

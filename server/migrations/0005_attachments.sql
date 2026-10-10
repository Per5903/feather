-- Вложения: файл загружается отдельно, затем привязывается к сообщению (message_id).
CREATE TABLE attachments (
    id           BIGINT PRIMARY KEY,
    channel_id   BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    uploader_id  BIGINT NOT NULL REFERENCES users (id),
    message_id   BIGINT REFERENCES messages (id) ON DELETE CASCADE,
    filename     TEXT NOT NULL,
    size         BIGINT NOT NULL,
    content_type TEXT NOT NULL,
    width        INT,
    height       INT,
    has_thumb    BOOLEAN NOT NULL DEFAULT false,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX attachments_message ON attachments (message_id);

-- Отметки о прочтении: до какого сообщения пользователь дочитал канал.
-- Непрочитанное = в канале есть сообщения новее last_read_message_id.
CREATE TABLE read_states (
    user_id              BIGINT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    channel_id           BIGINT NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    last_read_message_id BIGINT NOT NULL,
    PRIMARY KEY (user_id, channel_id)
);

# 04. Модель данных

Все ID — **Snowflake** (64 бита: время в мс, номер воркера, счётчик). Они сортируются по времени, поэтому пагинация простая: `WHERE id < :before ORDER BY id DESC LIMIT 50`.

## Основные таблицы

```sql
users            (id, username, display_name, email NULL, password_hash, avatar_key, created_at)
user_devices     (id, user_id, name, identity_key, created_at, last_seen_at)   -- для E2EE
sessions         (id, user_id, device_id, refresh_token_hash, expires_at)

guilds           (id, name, icon_key, owner_id, created_at)
guild_members    (guild_id, user_id, nickname, joined_at)
roles            (id, guild_id, name, color, permissions BIGINT, position)
member_roles     (guild_id, user_id, role_id)

channels         (id, guild_id NULL, type, name, parent_id, position, topic, e2ee BOOL)
                 -- type: text | voice | category | dm | group_dm
channel_overwrites (channel_id, target_id, target_type, allow BIGINT, deny BIGINT)
channel_recipients (channel_id, user_id)                                     -- для dm/group_dm

messages         (id, channel_id, author_id, content TEXT NULL, ciphertext BYTEA NULL,
                  reply_to_id, thread_id, created_at, edited_at, flags)
attachments      (id, message_id, storage_key, filename, size, content_type, width, height)
reactions        (message_id, user_id, emoji)

invites          (code, guild_id, channel_id, creator_id, expires_at, max_uses, uses)
read_states      (user_id, channel_id, last_read_message_id, mention_count)
bans             (guild_id, user_id, reason, banned_by, created_at)
audit_log        (id, guild_id, actor_id, action, target_id, changes JSONB, created_at)

-- E2EE (MLS)
mls_key_packages (id, device_id, key_package BYTEA, used BOOL)
mls_messages     (id, channel_id, epoch, payload BYTEA)   -- коммиты, welcome-сообщения
```

## Правила

- В обычных каналах заполнено `content`, в E2EE-каналах заполнен `ciphertext`, а `content` пустой. Сервер не может прочитать зашифрованные сообщения.
- Индекс `messages (channel_id, id DESC)` обязателен.
- Права — битовая маска (`feather_protocol::perms`, реализовано: роли, @everyone с id = id сервера, кик, бан). Итоговые права считаются так: `@everyone` → роли участника → overwrites канала (сначала для ролей, потом для конкретного участника). Владелец сервера и право `ADMINISTRATOR` обходят все проверки.
- Файлы хранятся по `storage_key`. Публичных ссылок нет: файл отдаётся через подписанный URL с ограниченным сроком жизни.

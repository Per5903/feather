# 05. Realtime-протокол (Gateway)

Транспорт — WebSocket (WSS). Формат кадров — **MessagePack** со сжатием `permessage-deflate`. Типы описаны в `crates/protocol`, и этот код общий для сервера и клиента.

## Основной принцип

- **Действия** пользователя (отправить, отредактировать, удалить) идут **через REST**. Так проще валидация, rate limit и идемпотентность (клиент передаёт `nonce`).
- **События** (что-то произошло) приходят **через Gateway**.

## Жизненный цикл соединения

```
Клиент                                   Сервер
  │── IDENTIFY {token, device_id, caps} ──►│
  │◄── READY {user, guilds(кратко),        │
  │           read_states, session_id} ────│
  │── SUBSCRIBE_GUILD {guild_id} ─────────►│   полные события только по открытому серверу
  │◄── DISPATCH {t, d, seq} ───────────────│
  │── HEARTBEAT {seq} ────────────────────►│   раз в ~30 с
  │◄── HEARTBEAT_ACK ──────────────────────│
  ...  обрыв связи  ...
  │── RESUME {session_id, last_seq} ──────►│
  │◄── пропущенные события из буфера ──────│
```

## Экономия трафика и ресурсов клиента

- **`SUBSCRIBE_GUILD`:** полные события (сообщения, typing, presence) приходят только по серверам, на которые клиент подписан. Обычно это открытый сервер. По остальным приходят только `UNREAD_UPDATE` и `MENTION`.
- **`MEMBER_LIST_RANGE {guild_id, channel_id, from, to}`:** статусы участников присылаются только для видимой части списка.
- **Игровой режим:** клиент отправляет `CLIENT_STATE {background: true}`, и сервер перестаёт присылать typing и presence до возврата.
- Сервер объединяет частые события (например, presence) в пачки раз в N мс.

## Основные события

`MESSAGE_CREATE`, `MESSAGE_UPDATE`, `MESSAGE_DELETE`, `REACTION_ADD`, `REACTION_REMOVE`, `TYPING_START`, `PRESENCE_UPDATE`, `CHANNEL_CREATE`, `CHANNEL_UPDATE`, `CHANNEL_DELETE`, `GUILD_MEMBER_ADD`, `GUILD_MEMBER_UPDATE`, `GUILD_MEMBER_REMOVE`, `VOICE_STATE_UPDATE`, `UNREAD_UPDATE`, `MLS_MESSAGE` (для E2EE).

## Ограничения

- Rate limit по пользователю и маршруту (token bucket). На single-node счётчики хранятся в памяти, в кластере в NATS KV или Redis.
- Буфер для `RESUME` — последние N событий сессии или T минут. Если клиент отстал больше, он получает новый `READY`.

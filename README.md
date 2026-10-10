# Feather (рабочее название)

Мессенджер, который сообщество или команда ставит на свой сервер. Голос не хуже, чем в Discord, клиент легче Telegram Desktop, данные остаются у вас.

> Название рабочее. Оно отсылает к главному принципу: клиент должен быть «лёгким как перо».

## Для кого

- **Игровые сообщества:** кланы, гильдии, команды, стримерские комьюнити. Для них важны голос с низкой задержкой и клиент, который не отнимает FPS.
- **Небольшие команды**, которым нужен свой сервер вместо чужого облака.

## Три принципа

1. **Лёгкий клиент.** Бюджет ресурсов здесь жёсткое требование. Если фича его нарушает, она не попадает в релиз. Подробнее в [docs/02-performance.md](docs/02-performance.md).
2. **Свой сервер за 5 минут.** Один `docker compose up`. Для небольшой инсталляции не нужны Redis, Kafka или Kubernetes.
3. **Приватность там, где она не ломает функции.** Личные сообщения, закрытые группы и приватные звонки шифруются сквозным шифрованием (E2EE).

## Документация

| Файл | О чём |
|---|---|
| [01-vision.md](docs/01-vision.md) | Позиционирование, конкуренты, чем отличаемся, чего не делаем |
| [02-performance.md](docs/02-performance.md) | **Бюджет ресурсов клиента** и способы его выдержать |
| [03-architecture.md](docs/03-architecture.md) | Архитектура, стек, структура репозитория |
| [04-data-model.md](docs/04-data-model.md) | Схема базы данных |
| [05-protocol.md](docs/05-protocol.md) | Realtime-протокол (Gateway) |
| [06-voice.md](docs/06-voice.md) | Голос, видео, демонстрация экрана |
| [07-security-privacy.md](docs/07-security-privacy.md) | Безопасность, E2EE, юридические вопросы |
| [08-roadmap.md](docs/08-roadmap.md) | Этапы и задачи |
| [09-decisions.md](docs/09-decisions.md) | Принятые решения и открытые вопросы |
| [10-test-infra.md](docs/10-test-infra.md) | Какие серверы нужны для тестов |
| [11-selfhost.md](docs/11-selfhost.md) | **Свой сервер за 5 минут** (Docker, HTTPS) |

## Статус

- **Этап 0 (прототипы рисков):** почти завершён. Результаты замеров в [bench/README.md](bench/README.md). Осталось проверить с человеком push-to-talk поверх игры и настоящий захват экрана.
- **Этап 1 (текстовое MVP):** работает переписка: сервер (регистрация, серверы, каналы, инвайты, сообщения, realtime-шлюз) и клиент на Slint (вход, серверы, каналы, живые сообщения, приглашения, переподключение). Клиент: 82 МБ RAM (18 МБ в экономном режиме) против 871 МБ у Discord. Дальше роли и права, отметки о прочтении, файлы.

## Как запустить

Нужны Docker, Rust (rustup) и pnpm (только для старого прототипа на Tauri).

```bash
# База и голосовой сервер
cp deploy/.env.example deploy/.env
docker compose -f deploy/docker-compose.yml up -d

# Сервер: http://127.0.0.1:8080, API в /api/v1, шлюз в /gateway
cargo run --release -p feather-server

# Тесты сервера (создают и удаляют временную базу)
cargo test -p feather-protocol -p feather-server

# Клиент (по умолчанию подключается к http://127.0.0.1:8080)
cargo run --release -p feather-native
# второй клиент на той же машине — с отдельной сохранённой сессией
FEATHER_PROFILE=second cargo run --release -p feather-native
# демо без сервера: 10 000 сообщений, для замеров
FEATHER_DEMO=1 cargo run --release -p feather-native
```

| Каталог | Что это |
|---|---|
| `server/` | feather-server (Rust, axum, Postgres) |
| `crates/protocol/` | общие типы API и шлюза |
| `client/native/` | клиент на Slint (бинарник `feather`) |
| `client/core/` | ядро клиента без UI: REST и шлюз |
| `client/voice-proto/` | прототип голоса и видео на LiveKit |
| `client/ptt-proto/` | прототип глобального push-to-talk (Wayland) |
| `client/desktop/`, `client/ui/` | старый прототип на Tauri, только для сравнения |
| `bench/` | замеры ресурсов и звука |
| `deploy/` | docker-compose для разработки, `deploy/selfhost/` — для своего сервера |

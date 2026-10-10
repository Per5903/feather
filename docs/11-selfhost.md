# 11. Свой сервер Feather за 5 минут

Всё в одном каталоге: [`deploy/selfhost/`](../deploy/selfhost/). Внутри четыре контейнера: Caddy (HTTPS), feather-server, Postgres, LiveKit (голос).

## Что нужно

- VPS: 2 vCPU, 2–4 ГБ RAM, белый IPv4 (см. [10-test-infra](10-test-infra.md)).
- Домен (или поддомен), A-запись которого указывает на IP сервера.
- Docker с compose (`docker compose version`).

## Установка

```bash
git clone <репозиторий> feather && cd feather/deploy/selfhost
./setup.sh chat.example.com   # создаст .env со случайными паролями и ключами
docker compose up -d          # первая сборка сервера занимает 2–5 минут
```

Откройте в файрволе:

| Порт | Протокол | Зачем |
|---|---|---|
| 80, 443 | TCP | HTTPS (Caddy сам получит сертификат Let's Encrypt) |
| 443 | UDP | HTTP/3 |
| 7881 | TCP | голос через TCP, если UDP закрыт |
| 7882 | UDP | голос и видео |

В клиенте укажите адрес сервера `https://chat.example.com`.

## Проверка

```bash
docker compose ps                         # server: healthy
curl https://chat.example.com/health      # ok
```

## Обновление и резервные копии

```bash
git pull && docker compose up -d --build                                  # обновить
docker compose exec postgres pg_dump -U feather feather > backup.sql      # копия базы
```

Миграции базы применяются автоматически при старте сервера.

## Сколько ресурсов

Сервер в простое — около 4 МБ RAM (bench/README.md, замер 10), образ — 122 МБ. Основное потребление — Postgres и LiveKit (голос, см. [10-test-infra](10-test-infra.md)).

## Проверено (2026-10-10)

Локально с `DOMAIN=localhost` на портах 8081/8443: Caddy выдаёт HTTPS (HTTP/2, внутренний сертификат), сервер healthy, регистрация и вход работают через HTTPS. Не проверено: получение настоящего сертификата Let's Encrypt и голос через интернет (нужен VPS с доменом).

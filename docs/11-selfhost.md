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

## Обновление

```bash
git pull && docker compose up -d --build
```

Миграции базы применяются автоматически при старте сервера.

На слабом VPS сборка образа долгая и грузит процессор (то, что на нём ещё работает, будет тормозить). Можно собрать образ на своём компьютере и передать готовым:

```bash
docker build -f server/Dockerfile -t feather-server:local .                     # на своём компьютере
docker save feather-server:local | gzip | ssh root@сервер 'gunzip | docker load'
ssh root@сервер 'cd feather/deploy/selfhost && docker compose up -d --no-build server'
```

## Резервные копии

Служба `backup` раз в сутки кладёт в `deploy/selfhost/backups/` копию базы (`db-ДАТА.dump`) и файлов: вложений и аватаров (`files-ДАТА.tar.gz`). Хранятся последние 7. Настройки в `.env`: `FEATHER_BACKUP_HOURS=24`, `FEATHER_BACKUP_KEEP=7`.

Копии лежат на том же сервере, поэтому от потери самого сервера они не спасут. Время от времени забирайте их к себе:

```bash
scp -r root@сервер:feather/deploy/selfhost/backups ./feather-backups
```

Восстановление (сервер Feather на время остановить):

```bash
docker compose stop server
docker compose exec -T postgres dropdb -U feather feather
docker compose exec -T postgres createdb -U feather feather
docker compose exec -T postgres pg_restore -U feather -d feather < backups/db-ДАТА.dump
docker run --rm -v feather_files:/data -v "$PWD/backups":/b alpine tar xzf /b/files-ДАТА.tar.gz -C /data
docker compose start server
```

Проверено 2026-10-17: копия создаётся, из неё восстанавливается база со всеми сообщениями.

## Сколько ресурсов

Сервер в простое — около 4 МБ RAM (bench/README.md, замер 10), образ — 122 МБ. Основное потребление — Postgres и LiveKit (голос, см. [10-test-infra](10-test-infra.md)).

## Проверено (2026-10-10)

Локально с `DOMAIN=localhost` на портах 8081/8443: Caddy выдаёт HTTPS (HTTP/2, внутренний сертификат), сервер healthy, регистрация и вход работают через HTTPS. Не проверено: получение настоящего сертификата Let's Encrypt и голос через интернет (нужен VPS с доменом).

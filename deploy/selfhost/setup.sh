#!/usr/bin/env bash
# Создаёт .env для self-hosted Feather со случайными паролями и ключами.
#   ./setup.sh chat.example.com
set -euo pipefail
cd "$(dirname "$0")"
domain="${1:?укажите домен: ./setup.sh chat.example.com}"
if [ -f .env ]; then
  echo ".env уже есть — не перезаписываю (удалите его, если хотите новые ключи)"; exit 1
fi
rand() { head -c 48 /dev/urandom | base64 | tr -dc 'A-Za-z0-9' | head -c "$1"; }
umask 077
cat > .env <<ENV
DOMAIN=$domain
POSTGRES_PASSWORD=$(rand 32)
LIVEKIT_API_KEY=API$(rand 12)
LIVEKIT_API_SECRET=$(rand 48)
ENV
echo "готово: .env создан (права 600)."
echo "Откройте порты: 80/tcp, 443/tcp+udp, 7881/tcp, 7882/udp. Затем: docker compose up -d"

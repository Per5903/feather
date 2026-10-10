#!/usr/bin/env bash
# Установка Feather для текущего пользователя (без sudo):
#   ./install.sh            — установить или обновить
#   ./install.sh --remove   — удалить
set -euo pipefail
cd "$(dirname "$0")"
BIN="$HOME/.local/bin/feather"
APPS="$HOME/.local/share/applications/app.feather.Feather.desktop"
ICON="$HOME/.local/share/icons/hicolor/128x128/apps/app.feather.Feather.png"

if [ "${1:-}" = "--remove" ]; then
  rm -f "$BIN" "$APPS" "$ICON"
  echo "Feather удалён (настройки остались в ~/.config/feather)"
  exit 0
fi

install -Dm755 feather "$BIN"
install -Dm644 feather.png "$ICON"
mkdir -p "$(dirname "$APPS")"
cat > "$APPS" <<DESKTOP
[Desktop Entry]
Type=Application
Name=Feather
Comment=Лёгкий мессенджер с голосом для своего сервера
Exec=$BIN
Icon=app.feather.Feather
Categories=Network;Chat;InstantMessaging;
Terminal=false
StartupWMClass=feather
DESKTOP
command -v update-desktop-database >/dev/null && update-desktop-database "$(dirname "$APPS")" 2>/dev/null || true
command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
echo "Готово: Feather появился в меню приложений (или запустите $BIN)"

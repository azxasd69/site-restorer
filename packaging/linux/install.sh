#!/usr/bin/env bash
# Установка Site Restorer в домашний профиль (без root).
#   ./install.sh            -> ~/.local
#   PREFIX=/usr/local sudo ./install.sh
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-$HOME/.local}"
install -Dm755 "$here/site-restorer" "$prefix/bin/site-restorer"
install -Dm755 "$here/site-restorer-gui" "$prefix/bin/site-restorer-gui"
install -Dm644 "$here/share/icons/site-restorer.svg" "$prefix/share/icons/hicolor/scalable/apps/site-restorer.svg"
install -Dm644 "$here/share/applications/site-restorer.desktop" "$prefix/share/applications/site-restorer.desktop"
if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$prefix/share/applications" || true
fi
echo "Готово. Запуск: site-restorer-gui (окно) или site-restorer --cli --site <адрес>"
echo "Если окно не открывается, установите: sudo apt install libxkbcommon-x11-0 libgl1"

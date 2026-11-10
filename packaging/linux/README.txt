Site Restorer (Linux x86_64)
============================

Состав:
  site-restorer        — консольная версия (статическая, musl)
  site-restorer-gui    — окно (требует libxkbcommon-x11-0, libgl1, libegl1)
  install.sh           — установка в ~/.local
  share/               — .desktop и иконка

Быстрый старт без установки:
  ./site-restorer --cli --site example.com --out ~/copy

Установка:
  ./install.sh
  # или: PREFIX=/usr/local sudo ./install.sh

Зависимости окна (Debian/Ubuntu):
  sudo apt install libxkbcommon-x11-0 libgl1 libegl1

Если в системе нет OpenGL 2.0+ (виртуальная машина, RDP), окно покажет
сообщение — используйте консольную версию.

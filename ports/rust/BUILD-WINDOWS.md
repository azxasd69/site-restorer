# Site Restorer (Rust) — сборка под Windows x86/x64

Дата: 09–10.11.2026. Исходники не изменялись (`D:\dev\apps\site-restorer-rs` — только чтение).
Все артефакты — в `D:\dev\apps\site-restorer-rs\target-win\dist\`.

## 1. Окружение

```
$env:LOCALAPPDATA="$env:USERPROFILE\AppData\Local"; $env:APPDATA="$env:USERPROFILE\AppData\Roaming"
$env:TEMP="$env:LOCALAPPDATA\Temp"; $env:RUSTUP_HOME="D:\dev\rustup"; $env:CARGO_HOME="D:\dev\cargo"
$env:PATH="D:\dev\cargo\bin;$env:PATH"
```

- `rustup show`: host `x86_64-pc-windows-msvc`, toolchain `stable-x86_64-pc-windows-msvc` (default).
- `rustc 1.99.1 (58cf8f9c3 2026-10-13)`, `cargo 1.99.1 (5f94df478 2026-08-27)`.
- `rustup target list --installed`: `i686-pc-windows-msvc`, `x86_64-pc-windows-msvc`.
  → **Обе цели уже были установлены**, `rustup target add` не потребовался.
- MSVC: VS Build Tools 2022, `vcvars64.bat` / `vcvars32.bat` из `VC\Auxiliary\Build`.
- Профиль release из `Cargo.toml`: `opt-level="z"`, `lto=true`, `codegen-units=1`, `panic="abort"`, `strip="symbols"`.

## 2. Сборки

| Сборка | Команда (кратко) | Время | Итог |
|---|---|---|---|
| CLI x64 | `cargo build --release --no-default-features --target x86_64-pc-windows-msvc --target-dir target-win` | 50,9 с | rc=0 |
| CLI x86 | то же, `--target i686-pc-windows-msvc` | 46,9 с | rc=0 |
| GUI x64 | `cmd /c "call vcvars64.bat && cargo build --release --target x86_64-pc-windows-msvc --target-dir target-win"` | 2 м 05 с | rc=0 |
| GUI x86 | то же через `vcvars32.bat`, `--target i686-pc-windows-msvc` | 2 м 09 с | rc=0 |

GUI-сборка (eframe 0.37, glow) собирается без правок: фичи `x11`/`wayland` в Cargo.toml на Windows безвредны.
Все 4 сборки дают одинаковые 3 предупреждения компилятора профиля CLI
(`unused import sanitize::sanitize_site` в `src/main.rs:10`, поля `Progress.eta`/`cancel`, мёртвый `cancel()` в `src/core.rs`) — к сборке не относятся, в GUI-варианте набор предупреждений меньше (поля читаются).

Важно: CLI и GUI кладут бинарник в **один и тот же** путь `<target-dir>\<triple>\release\site-restorer.exe`,
поэтому после каждой сборки exe копировался в `dist\` с суффиксом; CLI пересобирался после GUI,
чтобы не потерять CLI-вариант.

## 3. Размеры (dist\)

| Файл | Байт | Архитектура (PE) |
|---|---|---|
| site-restorer-cli-x64.exe | 2 733 568 | x64 |
| site-restorer-cli-x86.exe | 2 091 520 | x86 |
| site-restorer-gui-x64.exe | 7 266 304 | x64 |
| site-restorer-gui-x86.exe | 6 474 752 | x86 |

Разрядность проверена чтением PE-заголовка (Machine: 0x8664 / 0x14c).

## 4. Проверка GUI на хосте (Windows 11 Pro 26300)

Запуск `Start-Process` → пауза 6 с → `Get-Process`:

| Бинарник | Процесс через 6 с | MainWindowHandle | MainWindowTitle | Responding |
|---|---|---|---|---|
| gui-x64.exe | жив, PID 992 | 1837102 | `Site Restorer — копия сайта из веб-архива` | True |
| gui-x86.exe | жив, PID 31976 | — | `Site Restorer — копия сайта из веб-архива` | True |

Оба процесса после проверки убиты (свой PID), повторная проверка `Get-Process` → процесса нет.
Окно открывается, заголовок соответствует `with_title(...)` из `src/main.rs:26`.

## 5. CLI — прогон на хосте (x64 и x86)

```
site-restorer-cli-{x64,x86}.exe --cli --site example.com --out <dir> --limit 5
```

Обе сборки: rc=0, вывод
`ИТОГ: скачано 1 из 1, страниц 1, ссылок 1, битых 1`, созданы
`site\index.html` (6814 Б), `mirror\index.html`, `data\inventory.jsonl`, `report.html` (2481 Б).
Каталоги прогонов: `target-win\host-run-x64`, `target-win\host-run-x86`.

## 6. ВМ Win10-x86 (Hyper-V) — прогон НЕ выполнен

Что установлено фактами:

- `Get-VM` недоступен: сессия не администратор (`IsInRole(Administrator)=False`) и не входит в
  «Hyper-V Administrators»; команда падает `Value cannot be null (Parameter 'name')`,
  журнал `Hyper-V-VMMS-Admin` — `Attempted to perform an unauthorized operation`.
  `Msvm_ComputerSystem` в WMI показывает только сам хост (`KOMP`, EnabledState=2).
- Файлы ВМ на месте: `D:\VMs\Win10-x86\Virtual Hard Disks\Win10-x86.vhdx` (15,47 ГБ),
  конфигурация `...\Virtual Machines\8BA126B5-E82E-4FB9-BB79-15B6AA7187CF.vmcx`.
- ВМ **уже запущена**: 4 процесса `vmwp.exe`, `vmms`/`vmcompute`/`HvHost` Running,
  mtime vhdx обновлялся во время работы (00:06:37 → 00:20:06), т.е. гость пишет на диск.
  `Start-VM` не требовался и без прав невозможен.
- Сеть: адаптер `vEthernet (Default Switch)` 172.27.176.1/20; гость `WIN10-X86`
  (NetBIOS, WORKGROUP) — **172.27.178.208**, MAC `00-15-5d-1f-ae-04`.
- Порты гостя: **3389 открыт**, 445/139/135/5985/5986 закрыты → SMB и PowerShell Remoting недоступны.
- RDP-вход состоялся: `mstsc` с сохранённым credential (`WIN10-X86\admin`) подключился
  (TCP ESTABLISHED), на скриншоте — рабочий стол Win10 x86, панель задач, вход выполнен.

Что помешало выполнить прогон:

1. Скопировать файл в гостя нечем: SMB/WinRM закрыты, проброс диска в RDP
   (`drivestoredirect:s:D:\`) и `alternate shell` с `run.bat` из `\\tsclient\D\...` не дали
   ни одного файла на шаре (`stage1.txt`/`run-out.txt` не появились за 100 с, гость при этом
   не разлогинился → batch не запускался, `alternate shell` не применился).
2. Управление гостем синтетическим вводом невозможно: `SendKeys`/`mouse_event` в окно mstsc
   не доходят до сессии — `Ctrl+Esc` открывал «Пуск» **хоста**, а не гостя,
   хотя `GetForegroundWindow()` возвращал handle mstsc. mstsc отбрасывает инъецированный ввод.
3. Альтернативный клиент FreeRDP, который такой ввод принимает, поставить не удалось:
   в бакетах scoop его нет (`versions/freerdp-nightly`), а ассет `freerdp-3.33.0.zip` с GitHub
   оказался архивом **исходников**, не Windows-бинарников; сборка из исходников вне задачи.
   Временный каталог `D:\dev\cache\freerdp` удалён.

Итог: CLI-x86 на ВМ **не запускался**. Проверка x86-бинарника выполнена на хосте под WOW64 (§5)
— это подтверждает работоспособность 32-битной сборки, но не поведение на Win10 19045 x86.

### Что нужно, чтобы закрыть пункт
- Хост-сессия с правами администратора Hyper-V (или членство в «Hyper-V Administrators»),
  либо включённые в госте SMB (445) / WinRM (5985), либо готовый RDP-клиент, не фильтрующий
  инъецированный ввод (FreeRDP/`wfreerdp` из MSI, не из source-zip).
- Далее: `copy site-restorer-cli-x86.exe \\172.27.178.208\C$\Temp\sr.exe` и
  `sr.exe --cli --site example.com --out C:\Temp\sr-out --limit 5`.

## 7. Повтор сборки с нуля

```powershell
$env:LOCALAPPDATA="$env:USERPROFILE\AppData\Local"; $env:APPDATA="$env:USERPROFILE\AppData\Roaming"
$env:TEMP="$env:LOCALAPPDATA\Temp"; $env:RUSTUP_HOME="D:\dev\rustup"; $env:CARGO_HOME="D:\dev\cargo"
$env:PATH="D:\dev\cargo\bin;$env:PATH"
cd D:\dev\apps\site-restorer-rs
$T="D:\dev\apps\site-restorer-rs\target-win"
cargo build --release --no-default-features --target x86_64-pc-windows-msvc --target-dir $T
cargo build --release --no-default-features --target i686-pc-windows-msvc   --target-dir $T
cmd /c "call `"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat`" && cargo build --release --target x86_64-pc-windows-msvc --target-dir $T"
cmd /c "call `"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars32.bat`" && cargo build --release --target i686-pc-windows-msvc --target-dir $T"
```

## 8. Остаточный риск

- GUI проверен только фактом открытия окна; отрисовка кадра (glow/OpenGL) и функциональность
  кнопок не проверялись — скриншот окна не снимался.
- Прогон CLI ограничен `--limit 5` на `example.com`: 1 страница, 1 ссылка, 1 битая.
  Многопоточная загрузка, докачка и большие сайты не проверялись.
- x86-варианты не запускались на реальной 32-битной ОС (только WOW64 на x64-хосте).
- 1С-правила/BSL и прочие домены не затрагивались; MCP-проверки не применялись.

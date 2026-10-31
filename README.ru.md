<p align="center"><img src="assets/icon/AppIcon-256.png" width="128" height="128" alt=""></p>

# OpenRDP

[English version](README.md)

Открытый RDP-сервер для macOS на [IronRDP](https://github.com/Devolutions/IronRDP). Клиент — любой RDP-клиент с GFX/H.264: Windows App (Mac, Windows, iOS), FreeRDP/Remmina (Linux).

Как устроено:

- захват — ScreenCaptureKit, сразу NV12 full-range BT.709 (цветовое пространство AVC в MS-RDPEGFX), без конвертации;
- кодирование — VideoToolbox H.264, realtime, low-latency, без B-кадров;
- графика — канал EGFX, AVC420; поверхность выровнена по 16, лишнее обрезается регионом;
- размер экрана — виртуальный дисплей под окно клиента, HiDPI, предел 3072×1920 по площади (`--max-size`); после отключения живёт 5 минут, чтобы окна не разъезжались;
- ввод — `CGEventPost`: скан-коды RDP → keycode Mac, Win → Command, Alt → Option, подсчёт двойных кликов;
- курсор — pointer-сообщения RDP, клиент рисует его сам;
- буфер обмена — текст и картинки в обе стороны;
- питание — Мак не засыпает, пока работает сервер; экран не гаснет, пока подключён клиент; подключение будит уснувший экран.

## Сборка и запуск

```bash
cargo build --release
```

```bash
./target/release/openrdp --user me --pass secret
```

Слушает `0.0.0.0:3389`, вход по NLA. Логин и пароль — собственные для сервера, не учётная запись macOS; пароль можно передать и через `OPENRDP_PASSWORD`. Сертификат самоподписанный (RSA: Windows App не принимает ECDSA), создаётся при первом запуске в `~/Library/Application Support/openrdp`. Остальные флаги — `--help`.

Права macOS (System Settings → Privacy & Security) выдаются приложению, из которого запущен сервер (Terminal, Ghostty и т. п.), после чего его нужно перезапустить. Сервер сам запрашивает оба права при старте, и macOS добавляет это приложение в списки:

- **Screen & System Audio Recording** — без него картинки нет;
- **Accessibility** — без него не работают клавиатура и мышь.

### Автозапуск

```bash
./target/release/openrdp install --user me --pass secret
```

Копирует сервер в `~/Applications/OpenRDP.app` и ставит LaunchAgent `com.openrdp.server`. Сервер стартует при входе в систему и перезапускается, если упал. Пароль лежит в `~/Library/LaunchAgents/com.openrdp.server.plist` с правами 600, лог пишется в `~/Library/Logs/openrdp.log`. Права macOS в этом режиме выдаются «OpenRDP», а не терминалу. Выдать их можно только за самим маком, после этого сервер нужно перезапустить:

```bash
launchctl kickstart -k gui/$(id -u)/com.openrdp.server
```

Если на диске ещё лежит старая установка, `install` удаляет тот агент, `~/Applications/macRDP.app`, его лог и `~/Library/Application Support/macrdp`. Ничего не переносится, поэтому macOS снова спрашивает Screen Recording и Accessibility.

После обновления сборки снова выполните `install`. Он подписывает приложение локальным сертификатом из отдельной связки ключей в `~/Library/Application Support/openrdp` (`signing.keychain-db`, пароль в `signing.password`), поэтому macOS узнаёт новую сборку как то же приложение, и права сохраняются. Если подписать так не получилось, `install` предупредит и подпишет ad hoc, и тогда права придётся выдавать заново. `openrdp uninstall` удаляет и агент, и приложение.

**На экране входа сервер не работает:** он запускается только после логина. После перезагрузки с FileVault разблокировка диска по SSH в систему не логинит: подключитесь через «Общий экран» (VNC) и войдите там, например через SSH-туннель — `ssh -N -L 5901:127.0.0.1:5900 you@mac`, затем `vnc://127.0.0.1:5901`. Другой вариант — `sudo fdesetup authrestart` перед перезагрузкой.

### Проверка и пробуждение: `openrdp-doctor`

[`scripts/openrdp-doctor`](scripts/openrdp-doctor) проверяет всё, от чего зависит удалённый доступ, и чинит частые случаи. Запускать на хосте, например по SSH:

```bash
scripts/openrdp-doctor           # проверка: сессия, агент, порт, права, подпись, питание, сеть
scripts/openrdp-doctor wake      # разбудить экран
scripts/openrdp-doctor restart   # перезапустить агент
scripts/openrdp-doctor vpn       # поднять WireGuard (нужна открытая сессия)
```

С другой машины `openrdp-doctor wol <MAC> [broadcast]` будит уснувший Мак по Wake-on-LAN (на хосте нужен `pmset womp 1`).

`--test-pattern` шлёт синтетическую картинку вместо экрана: проверка видеопути без прав на запись экрана.

Проверка с FreeRDP:

```bash
sdl-freerdp /v:127.0.0.1 /u:me /p:secret /cert:ignore /gfx:avc420
```

В Windows App на Mac клавишу Command стоит отправлять как Windows — тогда на хосте она станет Command.

## Участие и лицензия

Как помочь — [CONTRIBUTING.md](CONTRIBUTING.md), уязвимости — [SECURITY.md](SECURITY.md). Лицензия — MIT или Apache-2.0 на выбор ([LICENSE-MIT](LICENSE-MIT), [LICENSE-APACHE](LICENSE-APACHE)).

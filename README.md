# macRDP

RDP-сервер для macOS на [IronRDP](https://github.com/Devolutions/IronRDP). Клиент — любой RDP-клиент с GFX/H.264: Windows App (Mac, Windows, iOS), FreeRDP/Remmina (Linux).

Как устроено:

- захват — ScreenCaptureKit, сразу NV12 full-range BT.709 (цветовое пространство AVC в MS-RDPEGFX), без конвертации;
- кодирование — VideoToolbox H.264, realtime, low-latency, без B-кадров;
- графика — канал EGFX, AVC420; поверхность выровнена по 16, лишнее обрезается регионом;
- ввод — `CGEventPost`: скан-коды RDP → keycode Mac, Win → Command, Alt → Option, подсчёт двойных кликов.

## Сборка и запуск

```bash
cargo build --release
```

```bash
./target/release/macrdp --user me --pass secret
```

Слушает `0.0.0.0:3389`, вход по NLA. Сертификат самоподписанный (RSA: Windows App не принимает ECDSA), создаётся при первом запуске в `~/Library/Application Support/macrdp`. Остальные флаги — `--help`.

Права macOS (System Settings → Privacy & Security) выдаются приложению, из которого запущен сервер (Terminal, Ghostty и т. п.), после чего его нужно перезапустить. Сервер сам запрашивает оба права при старте, и macOS добавляет это приложение в списки:

- **Screen & System Audio Recording** — без него картинки нет;
- **Accessibility** — без него не работают клавиатура и мышь.

### Автозапуск

```bash
./target/release/macrdp install --user me --pass secret
```

Копирует сервер в `~/Applications/macRDP.app` и ставит LaunchAgent `com.macrdp.server`. Сервер стартует при входе в систему и перезапускается, если упал. Пароль лежит в `~/Library/LaunchAgents/com.macrdp.server.plist` с правами 600, лог пишется в `~/Library/Logs/macrdp.log`. Права macOS в этом режиме выдаются «macRDP», а не терминалу. Выдать их можно только за самим маком, после этого сервер нужно перезапустить:

```bash
launchctl kickstart -k gui/$(id -u)/com.macrdp.server
```

После обновления сборки снова выполните `install`. Он подписывает приложение локальным сертификатом из отдельной связки ключей в `~/Library/Application Support/macrdp` (`signing.keychain-db`, пароль в `signing.password`), поэтому macOS узнаёт новую сборку как то же приложение, и права сохраняются. Если подписать так не получилось, `install` предупредит и подпишет ad hoc, и тогда права придётся выдавать заново. `macrdp uninstall` удаляет и агент, и приложение. На экране входа сервер не работает: он запускается только после логина, а после перезагрузки с FileVault нужен пароль за маком или `sudo fdesetup authrestart`.

`--test-pattern` шлёт синтетическую картинку вместо экрана: проверка видеопути без прав на запись экрана.

Проверка с FreeRDP:

```bash
sdl-freerdp /v:127.0.0.1 /u:me /p:secret /cert:ignore /gfx:avc420
```

В Windows App на Mac клавишу Command стоит отправлять как Windows — тогда на хосте она станет Command.

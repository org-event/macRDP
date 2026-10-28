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

`--test-pattern` шлёт синтетическую картинку вместо экрана: проверка видеопути без прав на запись экрана.

Проверка с FreeRDP:

```bash
sdl-freerdp /v:127.0.0.1 /u:me /p:secret /cert:ignore /gfx:avc420
```

В Windows App на Mac клавишу Command стоит отправлять как Windows — тогда на хосте она станет Command.

---
name: zed-nightly-build
description: Build the Zed Nightly Windows installer locally from this fork (zn-zed) and distribute it to all Windows users on this machine, without waiting for GitHub CI. Use whenever the user asks to build Zed locally, rebuild/update Zed Nightly, make a local installer, собрать/пересобрать zed nightly, "собери инсталлятор", "обнови zed nightly локально", or wants to test their fork changes in an installed (not dev) build. Also use when CI is down or too slow and the user wants a build now.
---

# Zed Nightly: локальная сборка инсталлятора

Всё делает один скрипт — `scripts/build-nightly.ps1`. Он сам:
временно ставит `crates/zed/RELEASE_CHANNEL = nightly` (и гарантированно
возвращает обратно, даже при падении), запускает `script/bundle-windows.ps1`
в отдельном pwsh с убранной переменной `CI` (иначе включатся гейты подписи
Azure), проверяет результат и копирует инсталлятор в
`C:\Users\Public\Downloads\Zed-Nightly-Setup.exe` — оттуда его может запустить
любой пользователь Windows на этой машине.

## Как запускать

Сборка занимает 40–90 минут и грузит CPU — всегда запускай в фоне
(`run_in_background: true`) и сообщи пользователю ориентир по времени:

```powershell
& "<repo>\.claude\skills\zed-nightly-build\scripts\build-nightly.ps1"
```

Флаги:
- `-Install` — после сборки сразу тихо обновить Zed Nightly текущего пользователя
  (по умолчанию НЕ ставим — пользователь решил ставить руками).
- `-SkipCopy` — не копировать в Public Downloads.
- `-DryRun` — все проверки и подмена/восстановление канала без самой сборки
  (~секунды). Используй для быстрой диагностики окружения.
- `-Architecture aarch64` — если вдруг понадобится ARM-сборка.

Скрипт можно звать из Windows PowerShell 5.1 — он сам перезапустится в pwsh.

## После сборки — что сказать пользователю

- Инсталлятор: `<repo>\target\Zed-x86_64.exe`, копия в
  `C:\Users\Public\Downloads\Zed-Nightly-Setup.exe`.
- Обновление = просто запустить инсталлятор поверх (можно
  `/VERYSILENT /NORESTART`). Настройки и данные не теряются — они в
  `%LOCALAPPDATA%\ZedNightly` каждого пользователя, инсталлятор их не трогает.
- Другие пользователи (например, dns) запускают копию из Public Downloads под
  своим логином — per-user установку нельзя сделать за них из чужой сессии.

## Подводные камни

- **Не запускай две сборки одновременно** и не гоняй параллельно `cargo build`
  в этом же checkout — cargo передерётся за target-папку.
- **Непушенутый HEAD**: скрипт предупредит, если HEAD впереди origin/main.
  Локальная сборка с таким SHA будет затёрта автообновлением, как только CI
  выпустит релиз с другим SHA. Если пользователь хочет пожить на локальной
  сборке — предложи либо запушить, либо временно `"auto_update": false` в
  настройках Zed Nightly.
- **Запускай из основного checkout** (`C:\Users\NikitaDev\Desktop\zed`), не из
  git worktree — у worktree отдельная target-папка, сборка с нуля будет дольше,
  а скрипт собирает тот репозиторий, в котором лежит сам.
- Если сборка падает на этапе Inno Setup/SDK — сначала прогони `-DryRun`: он
  проверяет ISCC, cargo и структуру репозитория и укажет, чего не хватает.

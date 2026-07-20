---
name: verify
description: Verify Zed changes in the running app on this Windows machine - build, launch the dev-channel instance, drive it with synthetic input, read ground truth from the SQLite KV store.
---

# Verifying Zed changes live (Windows, this machine)

## Build & launch

```powershell
cargo build   # debug build, ~2-4 min incremental
$env:ZED_RELEASE_CHANNEL = 'dev'   # separate "Zed Dev" instance; never touches the installed Zed Preview
Start-Process "C:\Users\NikitaDev\Desktop\zed\target\debug\zed.exe" -ArgumentList '"<project dir>"'
```

Demo project for the inbox panel: `C:\Users\NikitaDev\Desktop\inbox-demo`.
Wait for `MainWindowHandle -ne 0` on the process whose `Path` is `target\debug\zed.exe`.
The desktop watcher moves the window to Claude's virtual desktop automatically.

## Driving keystrokes / screenshots

- `SwitchToThisWindow(hwnd, true)` (user32) activates the window and switches
  the active virtual desktop to it; activating the previously-foreground hwnd
  at the end switches back. `[System.Windows.Forms.SendKeys]::SendWait(...)`
  for keys, `Graphics.CopyFromScreen` for screenshots.
- **Run such scripts with the sandbox disabled** — sandboxed processes get
  `SendWait: "The operation completed successfully"` and
  `CopyFromScreen: "The handle is invalid"`.
- The same two errors (plus `GetForegroundWindow() == 0`) mean the **session
  is locked**. Poll `OpenInputDesktop(0,$false,0x1)` until non-zero, then retry.
- If the user is actively typing, Windows may refuse the foreground switch and
  keystrokes land in *their* window — verify afterwards which window actually
  received input (screenshot may show the user's desktop) and prefer DB ground
  truth over screenshots.
- Clipboard: `[Windows.Forms.Clipboard]::SetImage/SetText/SetFileDropList`
  (pwsh 7 is STA already). SetImage produces CF_DIB — same as Win+Shift+S.

## Ground truth without the UI

Per-project inbox state lives in the SQLite KV store:
`%LOCALAPPDATA%\Zed\db\0-dev\db.sqlite`, table `scoped_kv_store`,
`namespace = 'inbox_panel'`, value = the `InboxFile` JSON. Copy the whole dir
(including `-wal`/`-shm`) before reading; query with python's sqlite3
(`python -X utf8`, console is cp1252). Saves are debounced 250 ms.

Pasted-image attachments land in `%LOCALAPPDATA%\Zed\inbox_attachments\<key>\`.

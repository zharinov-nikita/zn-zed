---
name: verify
description: Verify Zed changes on this Windows machine - build, launch the dev-channel instance for the user, hand over a QA checklist, and read ground truth from the SQLite KV store.
---

# Verifying Zed changes (Windows, this machine)

**The user drives the UI. You never do.** No synthetic keystrokes, no mouse
automation, no screenshots-as-verification — see `CLAUDE.local.md`. Your job is
to build, launch the dev instance, hand over a QA checklist, and check whatever
can be checked without the UI.

## Build & launch

```powershell
cargo build   # debug build, ~2-4 min incremental
$env:ZED_RELEASE_CHANNEL = 'dev'   # separate "Zed Dev" instance; never touches the installed Zed Preview
Start-Process "C:\Users\NikitaDev\Desktop\zed\target\debug\zed.exe" -ArgumentList '"<project dir>"'
```

Demo project for the inbox panel: `C:\Users\NikitaDev\Desktop\inbox-demo`.
Wait for `MainWindowHandle -ne 0` on the process whose `Path` is `target\debug\zed.exe`.
A running dev instance locks `zed.exe` — close it before the next `cargo build`.

## The QA checklist

Hand over a flat `- [ ]` list, grouped by scenario, each item a concrete action
plus the expected result ("chip shows `… (done)`", not "check it works"). Add a
separate section for known bugs and for anything the current binary is expected
to fail, so a red item isn't mistaken for a new regression. Then wait for the
results and fix from them.

## Ground truth without the UI

Per-project inbox state lives in the SQLite KV store:
`%LOCALAPPDATA%\Zed\db\0-dev\db.sqlite`, table `scoped_kv_store`,
`namespace = 'inbox_panel'`, value = the `InboxFile` JSON. Copy the whole dir
(including `-wal`/`-shm`) before reading; query with python's sqlite3
(`python -X utf8`, console is cp1252). Saves are debounced 250 ms.

Pasted-image attachments land in `%LOCALAPPDATA%\Zed\inbox_attachments\<key>\`.

The embedded inbox MCP server is the other UI-free window into a running
instance: `inbox_list_items` / `inbox_get_item` report exactly what the panel
holds.

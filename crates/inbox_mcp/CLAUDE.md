# inbox_mcp

MCP server embedded in the Zed process, exposing the inbox panel over
streamable HTTP on `127.0.0.1`. Tools operate on the live per-window
`InboxStore` entities on the UI thread, so the panel updates instantly and
mutations persist through the store's normal debounced save (~250ms — a hard
kill right after a call can lose the last mutation, same as UI edits).

**Contract rule:** any change to the inbox data model or store operations in
`crates/inbox_panel` must update the tools here (schemas, validation, docs)
in the same PR — see the matching gotcha in `crates/inbox_panel/CLAUDE.md`.

## Commands

```bash
cargo test -p inbox_mcp      # dispatcher + tool tests (gpui::test + FakeFs)
./script/clippy -p inbox_mcp # lint (never plain `cargo clippy`, per repo .rules)
```

## Endpoint

- URL: `http://127.0.0.1:<port>/mcp`; fixed port per release channel — Dev
  42817, Nightly 42818, Preview 42819, Stable 42820 — with an ephemeral
  fallback when the port is taken. Never hardcode the constant: read the
  actual URL from `ZED_INBOX_MCP_URL`.
- Auth: `Authorization: Bearer <token>`, random per Zed run. Exported as
  `ZED_INBOX_MCP_TOKEN`. Requests without it get 401.
- Discovery for clients started outside Zed (no inherited env): the
  per-channel data dir holds `inbox_mcp.json` with `{ "url", "token" }`
  (e.g. `%LOCALAPPDATA%\Zed Nightly\inbox_mcp.json`), rewritten on every
  start.
- `POST /mcp` — one JSON-RPC message, one `application/json` response (no
  SSE; notifications get `202`). `DELETE` → 200 (stateless, no session id),
  `GET` → 405.
- Gate: `inbox_panel.mcp_server` setting (default true), read once at
  startup in `init_inbox_mcp` (`crates/zed/src/main.rs`).

## The three consumers

1. **Built-in agent** — `init_inbox_mcp` registers the server in the
   `BuiltinContextServers` global (`crates/project/src/context_server_store.rs`),
   which `resolve_all_context_server_settings` merges ahead of user settings;
   the agent then sees it like any HTTP context server (profile gating via
   `enable_all_context_servers` applies).
2. **ACP agents** (Claude Code in the agent panel) — forwarded automatically
   by `mcp_servers_for_project` (`crates/agent_servers/src/acp.rs`) as
   `acp::McpServer::Http` with the auth header.
3. **Terminal MCP clients** — integrated terminals inherit the process env;
   `ZED_INBOX_MCP_URL`/`ZED_INBOX_MCP_TOKEN` are set in `inbox_mcp::init`.
   One-time client setup: `claude mcp add --transport http zed-inbox
   "$env:ZED_INBOX_MCP_URL" --header "Authorization: Bearer $env:ZED_INBOX_MCP_TOKEN"`.

## Layout

- `inbox_mcp.rs` — **crate root** (`[lib] path`), re-exports `init` + handle.
- `http_server.rs` — tiny_http accept loop on dedicated OS threads, auth,
  routing, and the bridge into GPUI (unbounded channel → single foreground
  dispatcher task → bounded std reply channel with a 15s timeout).
- `rpc.rs` — JSON-RPC dispatch (`initialize`, `ping`, `tools/list`,
  `tools/call`); wire structs come from `context_server::types` so the server
  can't drift from Zed's own MCP client. Tests live here.
- `tools.rs` — the 11 `inbox_*` tools (`listener.rs`-style trait: doc comment
  on the input struct = tool description, schemars draft07 + inlined
  subschemas).
- `project_resolve.rs` — resolves the optional `project` argument against
  `InboxStoreRegistry` (in `inbox_panel::inbox_store`).

## Gotchas

- **Never park a smol executor thread on `tiny_http`'s blocking `recv()`** —
  only the dedicated `InboxMcpN` OS threads may block. Every wait on the
  foreground has a timeout so a stalled UI thread can't wedge HTTP workers.
- Tool handlers run on the foreground with `&mut App`; mutations must go
  through `store.update(cx, …)` so the panel re-renders. Never touch the KV
  store directly — the panel would not reload and the debounced save would
  clobber the write.
- Store lookups go through weak handles (`InboxStoreRegistry`), upgraded per
  call: a closed window surfaces as a tool error, never a panic. Two windows
  on the same worktree share one KV key; only the first registered store is
  used.
- Mutating tools validate `kind`/tag keys against the catalogs and item ids
  against the store first — the underlying store mutations silently no-op on
  unknown ids and the panel silently hides dangling keys, which would read
  as success or data loss to the caller.
- `DisableAiSettings` stops the context-server consumers (built-in agent,
  ACP); the terminal path keeps working regardless.
- A user-settings `context_servers` entry named `zed-inbox` is ignored (the
  builtin registration wins); disable the server via `inbox_panel.mcp_server`
  instead.

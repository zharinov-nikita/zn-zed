//! Minimal streamable-HTTP transport for the embedded MCP server.
//!
//! `POST /mcp` carries one JSON-RPC message and gets a single JSON response
//! (no SSE streaming — the spec allows a plain `application/json` reply, and
//! both Zed's own HTTP client and Claude Code accept it). Notifications are
//! answered with `202 Accepted`, `DELETE` (session teardown) with `200`; the
//! server is stateless and issues no session id.
//!
//! Threading: the accept loop runs on dedicated OS threads — tiny_http's
//! blocking `recv()` must never park a smol executor thread. Each request is
//! forwarded over a channel to a single foreground dispatcher task, which
//! runs the tool handlers with full `&mut App` access (so stores mutate on
//! the UI thread and the panel re-renders live) and answers through a
//! bounded std channel the HTTP thread waits on with a timeout.

use std::sync::Arc;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use gpui::App;
use rand::RngCore as _;
use release_channel::ReleaseChannel;
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server};
use util::ResultExt as _;

use crate::rpc::McpDispatcher;

/// How long an HTTP worker waits for the foreground dispatcher before
/// answering 500 — a stalled foreground must not wedge the HTTP workers.
const FOREGROUND_TIMEOUT: Duration = Duration::from_secs(15);

/// Two workers: enough for the agent-panel client and a terminal client to
/// overlap without queueing behind each other.
const WORKER_COUNT: usize = 2;

/// Environment variables exported to child processes (integrated terminals,
/// tasks), so MCP clients launched from inside Zed can find the server.
pub const ENV_URL: &str = "ZED_INBOX_MCP_URL";
pub const ENV_TOKEN: &str = "ZED_INBOX_MCP_TOKEN";

/// The running server's coordinates, for registering it as a context server.
pub struct InboxMcpHandle {
    url: String,
    token: String,
}

impl InboxMcpHandle {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

/// Fixed per-channel default ports, so hand-written client configs keep
/// working across restarts. Zed is single-instance per release channel, and
/// distinct channels get distinct ports; a bind failure (some other process
/// squatting the port) falls back to an ephemeral one.
fn default_port(channel: ReleaseChannel) -> u16 {
    match channel {
        ReleaseChannel::Dev => 42817,
        ReleaseChannel::Nightly => 42818,
        ReleaseChannel::Preview => 42819,
        ReleaseChannel::Stable => 42820,
    }
}

/// Binds the listener, starts the worker threads and the foreground
/// dispatcher, and exports the discovery env vars. Returns `None` (with a
/// log line) when no port could be bound.
pub fn init(cx: &mut App) -> Option<InboxMcpHandle> {
    let channel = ReleaseChannel::try_global(cx).unwrap_or_default();
    let port = default_port(channel);
    let server = Server::http(("127.0.0.1", port))
        .or_else(|error| {
            log::warn!(
                "inbox mcp: port {port} unavailable ({error}), falling back to an ephemeral port"
            );
            Server::http("127.0.0.1:0")
        })
        .map_err(|error| log::error!("inbox mcp: failed to bind a localhost port: {error}"))
        .ok()?;
    let port = server
        .server_addr()
        .to_ip()
        .expect("bound to a TCP address")
        .port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let token = generate_token();

    // Child processes (terminals, tasks) inherit the process environment, so
    // this is all the discovery a terminal MCP client needs.
    //
    // SAFETY: called once during app init from the main thread. On Windows —
    // the platform this fork targets — `SetEnvironmentVariable` is
    // internally synchronized.
    unsafe {
        std::env::set_var(ENV_URL, &url);
        std::env::set_var(ENV_TOKEN, &token);
    }
    // MCP clients started *outside* Zed don't inherit the env; give them a
    // discovery file in the per-channel data dir (protected by the user
    // profile's ACL, like the rest of Zed's local state).
    write_discovery_file(&url, &token);

    let (request_tx, mut request_rx) =
        unbounded::<(String, SyncSender<Option<String>>)>();

    // The single foreground dispatcher: tool handlers need `&mut App`, and
    // funneling every request through one task means store mutations are
    // naturally serialized.
    cx.spawn(async move |cx| {
        let dispatcher = McpDispatcher::new();
        while let Some((body, reply)) = request_rx.next().await {
            let response = cx.update(|cx| dispatcher.handle(&body, cx));
            reply.send(response).ok();
        }
    })
    .detach();

    let server = Arc::new(server);
    for worker in 0..WORKER_COUNT {
        let server = server.clone();
        let request_tx = request_tx.clone();
        let token = token.clone();
        std::thread::Builder::new()
            .name(format!("InboxMcp{worker}"))
            .spawn(move || serve(&server, &request_tx, &token))
            .log_err();
    }

    log::info!("inbox mcp: serving on {url}");
    Some(InboxMcpHandle { url, token })
}

fn write_discovery_file(url: &str, token: &str) {
    let path = paths::data_dir().join("inbox_mcp.json");
    let contents = serde_json::json!({ "url": url, "token": token }).to_string();
    if let Err(error) = std::fs::write(&path, contents) {
        log::warn!("inbox mcp: failed to write discovery file {path:?}: {error}");
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn serve(
    server: &Server,
    request_tx: &UnboundedSender<(String, SyncSender<Option<String>>)>,
    token: &str,
) {
    // `recv` blocks until a request arrives and errors when the server is
    // shut down (all `Arc<Server>` clones dropped won't happen while workers
    // hold one, so in practice this loop lives for the process lifetime).
    while let Ok(mut request) = server.recv() {
        let response = handle_request(&mut request, request_tx, token);
        request.respond(response).log_err();
    }
}

fn handle_request(
    request: &mut Request,
    request_tx: &UnboundedSender<(String, SyncSender<Option<String>>)>,
    token: &str,
) -> ResponseBox {
    let path = request.url().split('?').next().unwrap_or_default();
    if path != "/mcp" {
        return status(404);
    }
    if !authorized(request, token) {
        return status(401);
    }
    match request.method() {
        Method::Post => {
            let mut body = String::new();
            if request.as_reader().read_to_string(&mut body).is_err() {
                return status(400);
            }
            let (reply_tx, reply_rx) = sync_channel(1);
            if request_tx.unbounded_send((body, reply_tx)).is_err() {
                // The foreground dispatcher is gone; the app is shutting down.
                return status(500);
            }
            match reply_rx.recv_timeout(FOREGROUND_TIMEOUT) {
                Ok(Some(response)) => Response::from_string(response)
                    .with_header(
                        Header::from_bytes("Content-Type", "application/json")
                            .expect("static header"),
                    )
                    .boxed(),
                // A notification: no response body is due.
                Ok(None) => status(202),
                Err(_) => {
                    log::error!(
                        "inbox mcp: foreground dispatcher did not answer within {:?}",
                        FOREGROUND_TIMEOUT
                    );
                    status(500)
                }
            }
        }
        // Stateless server: session teardown is a no-op.
        Method::Delete => status(200),
        // No SSE stream is offered; 405 per the streamable HTTP spec.
        _ => status(405),
    }
}

fn authorized(request: &Request, token: &str) -> bool {
    request.headers().iter().any(|header| {
        header.field.equiv("Authorization")
            && header
                .value
                .as_str()
                .strip_prefix("Bearer ")
                .is_some_and(|value| value == token)
    })
}

fn status(code: u16) -> ResponseBox {
    Response::empty(code).boxed()
}

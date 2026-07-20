//! An MCP (Model Context Protocol) server embedded in the Zed process,
//! exposing the inbox panel's data to MCP clients over streamable HTTP on
//! localhost. Because the tools operate on the live per-window
//! [`inbox_panel::InboxStore`] entities, the panel reflects every change
//! immediately and mutations persist through the store's normal debounced
//! save.
//!
//! Consumers:
//! - Zed's built-in agent and ACP agents (Claude Code in the agent panel):
//!   the server is registered as a builtin HTTP context server.
//! - Claude Code (or any MCP client) in Zed's integrated terminal: the URL
//!   and bearer token are exported as `ZED_INBOX_MCP_URL` /
//!   `ZED_INBOX_MCP_TOKEN`.

mod http_server;
mod project_resolve;
mod rpc;
mod tools;

pub use http_server::{InboxMcpHandle, init};

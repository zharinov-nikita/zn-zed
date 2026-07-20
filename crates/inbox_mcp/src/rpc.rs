//! JSON-RPC dispatch for the embedded MCP server.
//!
//! One entry point, [`McpDispatcher::handle`]: a raw JSON-RPC message in, an
//! optional JSON-RPC response out (`None` for notifications — the HTTP layer
//! answers those with `202 Accepted`). All wire structs come from
//! `context_server::types`, so the server can't drift from Zed's own MCP
//! client.

use context_server::client::RequestId;
use context_server::types::{
    CallToolParams, CallToolResponse, Implementation, InitializeParams, InitializeResponse,
    LATEST_PROTOCOL_VERSION, ListToolsResponse, ProtocolVersion, ServerCapabilities,
    ToolResponseContent, ToolsCapabilities, VERSION_2024_11_05, VERSION_2025_03_26,
    VERSION_2025_06_18,
};
use gpui::App;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tools::{self, RegisteredTool};

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;

#[derive(Deserialize)]
struct IncomingMessage {
    id: Option<RequestId>,
    method: Option<String>,
    #[serde(default)]
    params: Option<Value>,
}

pub struct McpDispatcher {
    tools: Vec<RegisteredTool>,
}

impl Default for McpDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl McpDispatcher {
    pub fn new() -> Self {
        Self {
            tools: tools::all_tools(),
        }
    }

    /// Handles one JSON-RPC message. Returns `None` when no response is due
    /// (the message was a notification).
    pub fn handle(&self, body: &str, cx: &mut App) -> Option<String> {
        let message: IncomingMessage = match serde_json::from_str(body) {
            Ok(message) => message,
            Err(error) => {
                return Some(error_response(
                    json!(null),
                    PARSE_ERROR,
                    format!("failed to parse message: {error}"),
                ));
            }
        };
        let Some(method) = message.method else {
            // A message without a method is either a batch (unsupported) or a
            // stray response; neither is something we can serve.
            return Some(error_response(
                json!(message.id),
                INVALID_REQUEST,
                "message has no method".to_string(),
            ));
        };
        let Some(id) = message.id else {
            // Notifications expect no response; ignore them all, including
            // `notifications/initialized` — the server is stateless.
            log::debug!("inbox mcp: ignoring notification {method}");
            return None;
        };

        Some(match method.as_str() {
            "initialize" => self.handle_initialize(id, message.params),
            "ping" => ok_response(id, json!({})),
            "tools/list" => self.handle_list_tools(id),
            "tools/call" => self.handle_call_tool(id, message.params, cx),
            _ => error_response(
                json!(id),
                METHOD_NOT_FOUND,
                format!("unhandled method {method}"),
            ),
        })
    }

    fn handle_initialize(&self, id: RequestId, params: Option<Value>) -> String {
        let params: InitializeParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return error_response(json!(id), INVALID_PARAMS, error),
        };
        // Echo the client's version when we know it; otherwise answer with
        // the latest we support, per the MCP version negotiation rules.
        let supported = [
            VERSION_2024_11_05,
            VERSION_2025_03_26,
            VERSION_2025_06_18,
            LATEST_PROTOCOL_VERSION,
        ];
        let client_version = params.protocol_version.0;
        let protocol_version = if supported.contains(&client_version.as_str()) {
            client_version
        } else {
            LATEST_PROTOCOL_VERSION.to_string()
        };
        let response = InitializeResponse {
            protocol_version: ProtocolVersion(protocol_version),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapabilities {
                    list_changed: Some(false),
                }),
                ..ServerCapabilities::default()
            },
            server_info: Implementation {
                name: "zed-inbox".to_string(),
                title: Some("Zed Inbox".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
                description: Some(
                    "Tools for the inbox panel of the running Zed instance".to_string(),
                ),
            },
            meta: None,
        };
        match serde_json::to_value(response) {
            Ok(result) => ok_response(id, result),
            Err(error) => error_response(json!(id), INTERNAL_ERROR, error.to_string()),
        }
    }

    fn handle_list_tools(&self, id: RequestId) -> String {
        let response = ListToolsResponse {
            tools: self.tools.iter().map(|tool| tool.tool.clone()).collect(),
            next_cursor: None,
            meta: None,
        };
        match serde_json::to_value(response) {
            Ok(result) => ok_response(id, result),
            Err(error) => error_response(json!(id), INTERNAL_ERROR, error.to_string()),
        }
    }

    fn handle_call_tool(&self, id: RequestId, params: Option<Value>, cx: &mut App) -> String {
        let params: CallToolParams = match parse_params(params) {
            Ok(params) => params,
            Err(error) => return error_response(json!(id), INVALID_PARAMS, error),
        };
        let Some(tool) = self
            .tools
            .iter()
            .find(|tool| tool.tool.name == params.name)
        else {
            return error_response(
                json!(id),
                INVALID_PARAMS,
                format!("tool not found: {}", params.name),
            );
        };
        // Handler failures are tool-level errors (`isError: true`), not
        // protocol errors: the calling model is expected to read them and
        // self-correct.
        let response = match tool.call(params.arguments, cx) {
            Ok(output) => CallToolResponse {
                content: vec![ToolResponseContent::Text { text: output.text }],
                is_error: Some(false),
                meta: None,
                structured_content: output.structured,
            },
            Err(error) => CallToolResponse {
                content: vec![ToolResponseContent::Text {
                    text: format!("{error:#}"),
                }],
                is_error: Some(true),
                meta: None,
                structured_content: None,
            },
        };
        match serde_json::to_value(response) {
            Ok(result) => ok_response(id, result),
            Err(error) => error_response(json!(id), INTERNAL_ERROR, error.to_string()),
        }
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, String> {
    serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|error| format!("invalid params: {error}"))
}

fn ok_response(id: RequestId, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i32, message: String) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use fs::FakeFs;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use inbox_panel::InboxStore;
    use inbox_panel::inbox_model::CatalogKind;
    use pretty_assertions::assert_eq;
    use project::Project;
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;

    use super::*;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            // A fresh in-memory database per test: without it the store would
            // fall back to the process-wide shared test DB and tests would
            // pollute each other through it.
            cx.set_global(db::AppDatabase::test_new());
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    async fn build_store(
        fs: Arc<FakeFs>,
        root: &str,
        cx: &mut TestAppContext,
    ) -> (Entity<Project>, Entity<InboxStore>) {
        let project = Project::test(fs.clone(), [Path::new(root)], cx).await;
        let store = cx.new(|cx| InboxStore::new(project.clone(), fs, cx));
        cx.run_until_parked();
        (project, store)
    }

    /// Sends one JSON-RPC message and parses the response.
    fn handle(
        dispatcher: &McpDispatcher,
        message: Value,
        cx: &mut TestAppContext,
    ) -> Option<Value> {
        cx.update(|cx| dispatcher.handle(&message.to_string(), cx))
            .map(|response| serde_json::from_str(&response).unwrap())
    }

    fn call_tool(
        dispatcher: &McpDispatcher,
        name: &str,
        arguments: Value,
        cx: &mut TestAppContext,
    ) -> Value {
        let response = handle(
            dispatcher,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": { "name": name, "arguments": arguments },
            }),
            cx,
        )
        .unwrap();
        response["result"].clone()
    }

    fn tool_text(result: &Value) -> String {
        result["content"][0]["text"].as_str().unwrap_or_default().to_string()
    }

    #[gpui::test]
    async fn test_initialize_list_tools_and_notifications(cx: &mut TestAppContext) {
        init_test(cx);
        let dispatcher = McpDispatcher::new();

        let response = handle(
            &dispatcher,
            json!({
                "jsonrpc": "2.0",
                "id": 0,
                "method": "initialize",
                "params": {
                    "protocolVersion": LATEST_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" },
                },
            }),
            cx,
        )
        .unwrap();
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(LATEST_PROTOCOL_VERSION)
        );
        assert_eq!(response["result"]["serverInfo"]["name"], json!("zed-inbox"));

        // An unknown client version negotiates down to our latest.
        let response = handle(
            &dispatcher,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "9999-01-01",
                    "capabilities": {},
                    "clientInfo": { "name": "test", "version": "1.0" },
                },
            }),
            cx,
        )
        .unwrap();
        assert_eq!(
            response["result"]["protocolVersion"],
            json!(LATEST_PROTOCOL_VERSION)
        );

        // Notifications produce no response.
        assert_eq!(
            handle(
                &dispatcher,
                json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                cx,
            ),
            None
        );

        let response = handle(
            &dispatcher,
            json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
            cx,
        )
        .unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 11);
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"inbox_capture"));
        assert!(names.contains(&"inbox_list_projects"));
        for tool in tools {
            assert!(
                tool["description"].as_str().is_some_and(|d| !d.is_empty()),
                "tool {} must carry a description",
                tool["name"]
            );
        }

        let response = handle(
            &dispatcher,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "no/such/method" }),
            cx,
        )
        .unwrap();
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));

        let response = handle(
            &dispatcher,
            json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }),
            cx,
        )
        .unwrap();
        assert_eq!(response["result"], json!({}));
    }

    #[gpui::test]
    async fn test_capture_get_and_list_roundtrip(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs, path!("/root"), cx).await;
        let dispatcher = McpDispatcher::new();

        let result = call_tool(
            &dispatcher,
            "inbox_capture",
            json!({ "text": "fix the panel", "body": "See the crash log.", "from": "src/main.rs:1" }),
            cx,
        );
        assert_eq!(result["isError"], json!(false));
        let id = result["structuredContent"]["item"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 1);
            assert_eq!(store.items()[0].text, "fix the panel");
            assert_eq!(store.items()[0].body.as_deref(), Some("See the crash log."));
        });

        let result = call_tool(&dispatcher, "inbox_get_item", json!({ "id": id }), cx);
        assert_eq!(result["isError"], json!(false));
        assert!(tool_text(&result).contains("# fix the panel"));

        let result = call_tool(&dispatcher, "inbox_list_items", json!({}), cx);
        assert_eq!(result["isError"], json!(false));
        assert!(tool_text(&result).contains("fix the panel"));
        assert_eq!(
            result["structuredContent"]["items"].as_array().unwrap().len(),
            1
        );
    }

    #[gpui::test]
    async fn test_unknown_catalog_keys_are_rejected(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs, path!("/root"), cx).await;
        let dispatcher = McpDispatcher::new();

        let result = call_tool(
            &dispatcher,
            "inbox_capture",
            json!({ "text": "task", "kind": "nope" }),
            cx,
        );
        assert_eq!(result["isError"], json!(true));
        assert!(tool_text(&result).contains("unknown list key"));
        store.read_with(cx, |store, _| assert_eq!(store.items().len(), 0));

        // With a real catalog entry the same capture goes through, and the
        // valid keys show up in the rejection message.
        let key = store.update(cx, |store, cx| store.add_entry(CatalogKind::List, cx));
        let result = call_tool(
            &dispatcher,
            "inbox_capture",
            json!({ "text": "task", "kind": key }),
            cx,
        );
        assert_eq!(result["isError"], json!(false));
        assert_eq!(
            result["structuredContent"]["kind_label"],
            json!("New list")
        );
    }

    #[gpui::test]
    async fn test_update_toggle_and_delete(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, store) = build_store(fs, path!("/root"), cx).await;
        let dispatcher = McpDispatcher::new();

        let result = call_tool(&dispatcher, "inbox_capture", json!({ "text": "draft" }), cx);
        let id = result["structuredContent"]["item"]["id"]
            .as_str()
            .unwrap()
            .to_string();

        let result = call_tool(
            &dispatcher,
            "inbox_update_item",
            json!({ "id": id, "text": "final", "body": "details" }),
            cx,
        );
        assert_eq!(result["isError"], json!(false));
        store.read_with(cx, |store, _| {
            assert_eq!(store.items()[0].text, "final");
            assert_eq!(store.items()[0].body.as_deref(), Some("details"));
        });

        // An empty string clears the body.
        call_tool(
            &dispatcher,
            "inbox_update_item",
            json!({ "id": id, "body": "" }),
            cx,
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.items()[0].body, None);
        });

        let result = call_tool(&dispatcher, "inbox_toggle_cleared", json!({ "id": id }), cx);
        assert!(tool_text(&result).contains("processed"));
        store.read_with(cx, |store, _| assert!(store.items()[0].is_cleared()));

        let result = call_tool(&dispatcher, "inbox_delete_item", json!({ "id": id }), cx);
        assert_eq!(result["isError"], json!(false));
        store.read_with(cx, |store, _| {
            assert_eq!(store.items().len(), 0);
            assert_eq!(store.archived().len(), 0);
        });

        // Mutating a missing item is a tool error, not a silent no-op.
        let result = call_tool(&dispatcher, "inbox_delete_item", json!({ "id": id }), cx);
        assert_eq!(result["isError"], json!(true));
        assert!(tool_text(&result).contains("no inbox item"));
    }

    #[gpui::test]
    async fn test_project_targeting(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/one"), json!({})).await;
        fs.insert_tree(path!("/two"), json!({})).await;
        let (_project_one, store_one) = build_store(fs.clone(), path!("/one"), cx).await;
        let (_project_two, store_two) = build_store(fs.clone(), path!("/two"), cx).await;
        let dispatcher = McpDispatcher::new();

        // Both projects are listed.
        let result = call_tool(&dispatcher, "inbox_list_projects", json!({}), cx);
        assert_eq!(
            result["structuredContent"]["projects"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Without `project` the target is ambiguous, and the error teaches
        // the caller what to pass.
        let result = call_tool(&dispatcher, "inbox_capture", json!({ "text": "task" }), cx);
        assert_eq!(result["isError"], json!(true));
        let text = tool_text(&result);
        assert!(text.contains("several projects"));
        assert!(text.contains(path!("/one")));

        // Targeting by worktree root routes to the right store.
        let result = call_tool(
            &dispatcher,
            "inbox_capture",
            json!({ "text": "task", "project": path!("/two") }),
            cx,
        );
        assert_eq!(result["isError"], json!(false));
        store_two.read_with(cx, |store, _| assert_eq!(store.items().len(), 1));
        store_one.read_with(cx, |store, _| assert_eq!(store.items().len(), 0));

        // A dropped store falls out of the registry: the other project
        // becomes the unambiguous default.
        drop(store_two);
        cx.run_until_parked();
        let result = call_tool(&dispatcher, "inbox_capture", json!({ "text": "solo" }), cx);
        assert_eq!(result["isError"], json!(false));
        store_one.read_with(cx, |store, _| assert_eq!(store.items().len(), 1));
    }

    #[gpui::test]
    async fn test_restore_requires_archived_item(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/root"), json!({})).await;
        let (_project, _store) = build_store(fs, path!("/root"), cx).await;
        let dispatcher = McpDispatcher::new();

        let result = call_tool(&dispatcher, "inbox_capture", json!({ "text": "open" }), cx);
        let id = result["structuredContent"]["item"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let result = call_tool(&dispatcher, "inbox_restore_item", json!({ "id": id }), cx);
        assert_eq!(result["isError"], json!(true));
        assert!(tool_text(&result).contains("not archived"));
    }
}

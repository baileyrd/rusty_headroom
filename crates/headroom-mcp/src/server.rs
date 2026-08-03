//! The Headroom MCP server.
//!
//! Exposes three tools to any agent that speaks MCP:
//!
//! - `headroom_compress` — compress content, returning the compressed form
//! - `headroom_retrieve` — get back the original behind a `<<ccr:HASH>>` marker
//! - `headroom_stats` — what this session has saved
//!
//! # Why retrieve matters most
//!
//! Compression is the visible feature, but `headroom_retrieve` is what makes it
//! *safe*. Without it, every marker in the conversation is a dead end and the model
//! has been told content is recoverable when it is not.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Every tool this server registers, in the order it lists them.
///
/// Exported so a consumer — the CLI's `tools` command, a test — names them from one
/// place. A second hand-maintained list is a list that drifts, and the failure is a
/// command confidently reporting a tool that no longer exists.
pub const TOOL_NAMES: [&str; 3] = ["headroom_compress", "headroom_retrieve", "headroom_stats"];

use headroom_core::auth_mode::{AuthMode, CompressionPolicy};
use headroom_core::ccr::{handle_retrieve, CcrStore, Retrieval};
use headroom_core::pipeline::Orchestrator;
use headroom_core::tokenizer::{HeuristicEstimator, Tokenizer};
use serde_json::{json, Value};

use crate::protocol::{failure, success, tool_result, ErrorCode, Request, PROTOCOL_VERSION};

/// Running totals for `headroom_stats`.
///
/// Atomics rather than a lock: these are updated on every compression and read rarely,
/// and a stats counter must never be able to block a compression.
#[derive(Debug, Default)]
struct Stats {
    calls: AtomicU64,
    compressions: AtomicU64,
    tokens_before: AtomicU64,
    tokens_after: AtomicU64,
    retrievals: AtomicU64,
}

/// An MCP server over the Headroom compressors.
pub struct McpServer {
    store: Arc<dyn CcrStore>,
    /// The same routing brain the proxy uses.
    ///
    /// This server used to hold its own compressor set and its own content-type match.
    /// Three copies of that decision existed — here, in the CLI, and in `headroom-core`
    /// — and they drifted: the core's table had no code arm, so the proxy forwarded
    /// source files uncompressed while this tool compressed them. A model asking
    /// `headroom_compress` what it would save should get the number the proxy would
    /// actually deliver.
    orchestrator: Orchestrator,
    stats: Stats,
}

impl McpServer {
    /// Creates a server backed by `store`.
    pub fn new(store: Arc<dyn CcrStore>) -> Self {
        Self {
            orchestrator: Orchestrator::new(store.clone()),
            store,
            stats: Stats::default(),
        }
    }

    /// The tool definitions advertised to the client.
    pub fn tool_definitions(&self) -> Value {
        json!([
            {
                "name": "headroom_compress",
                "description": "Compress bulky content — JSON, logs, search results, diffs, or \
        source code — into a much smaller form that preserves what matters. The original is \
        retained and can be recovered with headroom_retrieve.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": { "type": "string", "description": "The content to compress." }
                    },
                    "required": ["content"]
                }
            },
            {
                "name": "headroom_retrieve",
                "description": "Retrieve the full original content behind a <<ccr:HASH>> marker.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "hash": { "type": "string", "description": "The hash from the marker." }
                    },
                    "required": ["hash"]
                }
            },
            {
                "name": "headroom_stats",
                "description": "Report how much this session has compressed.",
                "inputSchema": { "type": "object", "properties": {} }
            }
        ])
    }

    /// Handles one request, returning the response to send.
    ///
    /// Returns `None` for notifications, which expect no reply. Sending one anyway
    /// would desynchronize a client that counts responses.
    pub fn handle(&self, request: &Request) -> Option<Value> {
        let id = request.id.clone();

        match request.method.as_str() {
            "initialize" => Some(success(
                id,
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "headroom", "version": env!("CARGO_PKG_VERSION") }
                }),
            )),
            "tools/list" => Some(success(id, json!({ "tools": self.tool_definitions() }))),
            "tools/call" => Some(self.call_tool(id, &request.params)),
            // Notifications carry no id and expect no reply.
            method if method.starts_with("notifications/") => None,
            other => Some(failure(
                id,
                ErrorCode::MethodNotFound,
                &format!("unknown method: {other}"),
            )),
        }
    }

    /// Dispatches a `tools/call`.
    fn call_tool(&self, id: Option<Value>, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return failure(id, ErrorCode::InvalidParams, "missing tool name");
        };
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

        self.stats.calls.fetch_add(1, Ordering::Relaxed);

        match name {
            "headroom_compress" => match arguments.get("content").and_then(Value::as_str) {
                Some(content) => success(id, self.compress(content)),
                None => failure(id, ErrorCode::InvalidParams, "missing 'content'"),
            },
            "headroom_retrieve" => match arguments.get("hash").and_then(Value::as_str) {
                Some(hash) => success(id, self.retrieve(hash)),
                None => failure(id, ErrorCode::InvalidParams, "missing 'hash'"),
            },
            "headroom_stats" => success(id, tool_result(self.stats_report(), false)),
            other => failure(
                id,
                ErrorCode::InvalidParams,
                &format!("unknown tool: {other}"),
            ),
        }
    }

    /// Compresses `content`, or explains why it was left alone.
    fn compress(&self, content: &str) -> Value {
        // Pay-as-you-go: a tool call is the caller compressing their own content
        // deliberately, not a relayed request whose credential decides what is
        // permitted. The proxy applies the real policy to real traffic.
        let policy = CompressionPolicy::for_mode(AuthMode::PayAsYouGo);
        let transform = self.orchestrator.transform_for(content, policy, "");

        let estimator = HeuristicEstimator::new();
        let before = estimator.count(content);

        let Some(transform) = transform else {
            // Returning the content unchanged rather than an error. The caller asked
            // for something smaller and gets something correct; a failure here would
            // make the tool look broken for the ordinary case of unremarkable input.
            return tool_result(content, false);
        };

        let mut block =
            headroom_core::Block::new(headroom_core::BlockKind::Text, content.to_owned());
        match headroom_core::validated_apply(transform, &mut block, &estimator) {
            Ok(outcome) if outcome.is_compressed() => {
                let after = estimator.count(block.content());
                self.stats.compressions.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .tokens_before
                    .fetch_add(before as u64, Ordering::Relaxed);
                self.stats
                    .tokens_after
                    .fetch_add(after as u64, Ordering::Relaxed);
                tool_result(block.content(), false)
            }
            _ => tool_result(content, false),
        }
    }

    /// Retrieves the original behind a marker.
    fn retrieve(&self, hash: &str) -> Value {
        self.stats.retrievals.fetch_add(1, Ordering::Relaxed);
        match handle_retrieve(self.store.as_ref(), hash) {
            Retrieval::Found(bytes) => {
                tool_result(String::from_utf8_lossy(&bytes).into_owned(), false)
            }
            // A tool-level failure, not a protocol error: the call worked, the content
            // is simply not there.
            other => tool_result(other.message(), true),
        }
    }

    /// Renders the session totals.
    fn stats_report(&self) -> String {
        let before = self.stats.tokens_before.load(Ordering::Relaxed);
        let after = self.stats.tokens_after.load(Ordering::Relaxed);
        let saved = before.saturating_sub(after);
        let percent = if before > 0 {
            (saved as f64 / before as f64) * 100.0
        } else {
            0.0
        };

        format!(
            "calls: {}\ncompressions: {}\nretrievals: {}\ntokens before: {before}\ntokens after: {after}\ntokens saved: {saved} ({percent:.0}%)",
            self.stats.calls.load(Ordering::Relaxed),
            self.stats.compressions.load(Ordering::Relaxed),
            self.stats.retrievals.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Line;
    use headroom_core::ccr::{parse_marker, InMemoryCcrStore};

    fn server() -> McpServer {
        McpServer::new(Arc::new(InMemoryCcrStore::new()))
    }

    fn request(method: &str, params: Value) -> Request {
        serde_json::from_value(json!({
            "jsonrpc": "2.0", "id": 1, "method": method, "params": params
        }))
        .unwrap()
    }

    fn call(name: &str, arguments: Value) -> Request {
        request(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    fn bulky_json() -> String {
        let records: Vec<String> = (0..150)
            .map(|i| {
                format!(
                    r#"{{"path":"src/m{i}.rs","kind":"file","ok":true,"size":{}}}"#,
                    100 + i
                )
            })
            .collect();
        format!("[{}]", records.join(","))
    }

    // ---- protocol ----

    #[test]
    fn initialize_reports_tool_capability() {
        let response = server().handle(&request("initialize", json!({}))).unwrap();
        assert_eq!(response["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert!(response["result"]["capabilities"]["tools"].is_object());
        assert_eq!(response["result"]["serverInfo"]["name"], "headroom");
    }

    #[test]
    fn tools_list_advertises_exactly_the_three_tools() {
        let response = server().handle(&request("tools/list", json!({}))).unwrap();
        let tools = response["result"]["tools"].as_array().unwrap();

        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, TOOL_NAMES.to_vec());
    }

    #[test]
    fn every_tool_declares_an_object_input_schema() {
        // A client that cannot read the schema cannot call the tool.
        let response = server().handle(&request("tools/list", json!({}))).unwrap();
        for tool in response["result"]["tools"].as_array().unwrap() {
            assert_eq!(tool["inputSchema"]["type"], "object", "{}", tool["name"]);
        }
    }

    #[test]
    fn a_notification_gets_no_reply() {
        // Replying would desynchronize a client that counts responses.
        let notification: Request =
            serde_json::from_value(json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .unwrap();
        assert!(server().handle(&notification).is_none());
    }

    #[test]
    fn an_unknown_method_is_a_protocol_error() {
        let response = server()
            .handle(&request("no/such/method", json!({})))
            .unwrap();
        assert_eq!(response["error"]["code"], ErrorCode::MethodNotFound.code());
    }

    #[test]
    fn a_missing_argument_is_an_invalid_params_error() {
        let response = server()
            .handle(&call("headroom_compress", json!({})))
            .unwrap();
        assert_eq!(response["error"]["code"], ErrorCode::InvalidParams.code());
    }

    // ---- compression ----

    #[test]
    fn bulky_json_comes_back_smaller() {
        let server = server();
        let source = bulky_json();
        let response = server
            .handle(&call("headroom_compress", json!({ "content": source })))
            .unwrap();

        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.len() < source.len() / 2, "not compressed");
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn unremarkable_content_comes_back_unchanged_rather_than_as_an_error() {
        // The caller asked for something smaller and gets something correct. An error
        // here would make the tool look broken for the ordinary case.
        let server = server();
        let response = server
            .handle(&call(
                "headroom_compress",
                json!({ "content": "just a short note" }),
            ))
            .unwrap();

        assert_eq!(
            response["result"]["content"][0]["text"],
            "just a short note"
        );
        assert_eq!(response["result"]["isError"], false);
    }

    // ---- retrieval ----

    #[test]
    fn the_original_is_retrievable_through_the_marker_the_compressor_emitted() {
        // The round trip that makes lossy compression safe.
        let server = server();
        let source = bulky_json();

        let compressed = server
            .handle(&call(
                "headroom_compress",
                json!({ "content": source.clone() }),
            ))
            .unwrap();
        let text = compressed["result"]["content"][0]["text"].as_str().unwrap();

        let marker_line = text
            .lines()
            .find(|l| l.starts_with("full content: "))
            .expect("marker emitted");
        let marker = marker_line.trim_start_matches("full content: ");
        parse_marker(marker).expect("well-formed marker");

        let retrieved = server
            .handle(&call("headroom_retrieve", json!({ "hash": marker })))
            .unwrap();

        assert_eq!(retrieved["result"]["content"][0]["text"], source);
        assert_eq!(retrieved["result"]["isError"], false);
    }

    #[test]
    fn an_unknown_hash_is_a_tool_failure_not_a_protocol_error() {
        let response = server()
            .handle(&call("headroom_retrieve", json!({ "hash": "not-a-hash" })))
            .unwrap();

        assert_eq!(response["result"]["isError"], true);
        assert!(
            response.get("error").is_none(),
            "should not be a protocol error"
        );
    }

    // ---- stats ----

    #[test]
    fn stats_reflect_work_actually_done() {
        let server = server();
        server
            .handle(&call(
                "headroom_compress",
                json!({ "content": bulky_json() }),
            ))
            .unwrap();

        let response = server.handle(&call("headroom_stats", json!({}))).unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap();

        assert!(text.contains("compressions: 1"), "{text}");
        assert!(text.contains("tokens saved:"), "{text}");
    }

    #[test]
    fn stats_on_a_fresh_server_do_not_divide_by_zero() {
        let response = server().handle(&call("headroom_stats", json!({}))).unwrap();
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("tokens saved: 0 (0%)"), "{text}");
    }

    // ---- framing ----

    #[test]
    fn every_response_serializes_to_one_line() {
        // Compressed output is full of newlines. If any reached the transport
        // unescaped, every response after the first would be unparseable.
        let server = server();
        for req in [
            request("initialize", json!({})),
            request("tools/list", json!({})),
            call("headroom_compress", json!({ "content": bulky_json() })),
            call("headroom_stats", json!({})),
        ] {
            let response = server.handle(&req).unwrap();
            let line = Line(response).to_string();
            assert!(!line.contains('\n'), "framing broken for {}", req.method);
            serde_json::from_str::<Value>(&line).expect("parseable");
        }
    }
}

//! Minimal JSON-RPC 2.0 framing for MCP.
//!
//! Only what the three Headroom tools need: `initialize`, `tools/list`, `tools/call`.
//! A full MCP implementation is a much larger surface, and everything not needed here
//! would be untested code claiming to work.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The protocol revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// An incoming JSON-RPC request.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// Always `"2.0"`; retained so a response can echo the caller's framing.
    #[serde(default)]
    pub jsonrpc: String,
    /// Correlation id. Absent for notifications, which expect no reply.
    #[serde(default)]
    pub id: Option<Value>,
    /// The method being called.
    pub method: String,
    /// Method parameters.
    #[serde(default)]
    pub params: Value,
}

/// A JSON-RPC error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The request was not valid JSON.
    ParseError,
    /// The request was valid JSON but not a valid request object.
    InvalidRequest,
    /// No such method.
    MethodNotFound,
    /// The method exists but the parameters were wrong.
    InvalidParams,
}

impl ErrorCode {
    /// The wire value.
    pub fn code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
        }
    }
}

/// Builds a success response.
pub fn success(id: Option<Value>, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// Builds an error response.
pub fn failure(id: Option<Value>, code: ErrorCode, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code.code(), "message": message }
    })
}

/// Wraps text as an MCP tool result.
///
/// `is_error` marks a *tool-level* failure, which is different from a JSON-RPC error:
/// the call succeeded, and the tool is reporting that what it was asked to do did not
/// work. Conflating the two would make a missing CCR entry look like a protocol fault.
pub fn tool_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error
    })
}

/// Serializes a response as one newline-delimited JSON line.
#[derive(Debug, Clone, Serialize)]
pub struct Line(pub Value);

impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match serde_json::to_string(&self.0) {
            Ok(rendered) => f.write_str(&rendered),
            // A response that cannot serialize must still produce a parseable line;
            // otherwise the client blocks forever waiting for one.
            Err(_) => f.write_str(
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error"}}"#,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_parses_from_the_wire_form() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
        let request: Request = serde_json::from_str(raw).unwrap();
        assert_eq!(request.method, "tools/list");
        assert_eq!(request.id, Some(json!(1)));
    }

    #[test]
    fn a_notification_parses_without_an_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let request: Request = serde_json::from_str(raw).unwrap();
        assert!(request.id.is_none());
    }

    #[test]
    fn missing_params_default_rather_than_failing() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let request: Request = serde_json::from_str(raw).unwrap();
        assert!(request.params.is_null());
    }

    #[test]
    fn error_codes_match_the_json_rpc_spec() {
        assert_eq!(ErrorCode::ParseError.code(), -32700);
        assert_eq!(ErrorCode::InvalidRequest.code(), -32600);
        assert_eq!(ErrorCode::MethodNotFound.code(), -32601);
        assert_eq!(ErrorCode::InvalidParams.code(), -32602);
    }

    #[test]
    fn a_tool_level_failure_is_not_a_protocol_error() {
        // The call succeeded; the tool is reporting the work did not. Conflating them
        // would make a missing CCR entry look like a protocol fault.
        let result = tool_result("not found", true);
        assert_eq!(result["isError"], true);
        assert!(result.get("error").is_none());
    }

    #[test]
    fn a_response_line_is_a_single_line_of_json() {
        let line = Line(success(Some(json!(7)), json!({"ok": true}))).to_string();
        assert!(
            !line.contains('\n'),
            "framing broken by an embedded newline"
        );
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["id"], 7);
    }

    #[test]
    fn content_with_newlines_does_not_break_the_framing() {
        // Compressed output is full of newlines. If they reached the transport
        // unescaped, every response after the first would be unparseable.
        let line = Line(tool_result("line one\nline two\nline three", false)).to_string();
        assert!(!line.contains('\n'));
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            parsed["content"][0]["text"],
            "line one\nline two\nline three"
        );
    }
}

//! Minimal JSON-RPC 2.0 over stdio, the transport MCP speaks.
//!
//! INTENT: the protocol surface an MCP server actually needs is `initialize`,
//! `tools/list`, `tools/call` and `ping`. Hand-rolling it is about eighty lines
//! and pins the wire format to something this repository controls, instead of
//! to a client library that revises its API faster than the spec it implements.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// MCP revision this bridge implements when the client states no preference.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// An incoming message. Without `id` it is a notification and gets no answer.
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// An outgoing response.
#[derive(Debug, Serialize)]
pub struct Outgoing {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Outgoing {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// `-32601` method not found, `-32602` invalid params, `-32603` internal.
    pub fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// The `initialize` result: what this server is and what it can do.
pub fn initialize_result(requested_version: Option<&str>) -> Value {
    json!({
        // Echoing the client's version when it states one keeps older clients
        // working; the spec allows the server to answer with its own otherwise.
        "protocolVersion": requested_version.unwrap_or(PROTOCOL_VERSION),
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "findery",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions":
            "Semantic index over local codebases. Prefer `search_code` over reading files or \
             grepping when you need to LOCATE something: it returns ranked, line-numbered \
             excerpts with host paths you can open directly. Sync a project after large \
             external changes; small edits are picked up by `sync_project` incrementally."
    })
}

/// A tool-call result carrying plain text.
pub fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text.into() }],
        "isError": is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_has_no_id() {
        let msg: Incoming =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .expect("parse");
        assert!(msg.id.is_none());
        assert_eq!(msg.method, "notifications/initialized");
    }

    #[test]
    fn responses_never_carry_both_result_and_error() {
        let ok = serde_json::to_value(Outgoing::ok(json!(1), json!({"a":1}))).expect("serialize");
        assert!(ok.get("error").is_none());
        assert_eq!(ok["jsonrpc"], "2.0");

        let err = serde_json::to_value(Outgoing::err(json!(1), -32601, "nope")).expect("serialize");
        assert!(err.get("result").is_none());
        assert_eq!(err["error"]["code"], -32601);
    }

    #[test]
    fn initialize_echoes_the_clients_protocol_version() {
        let v = initialize_result(Some("2024-11-05"));
        assert_eq!(v["protocolVersion"], "2024-11-05");
        assert_eq!(initialize_result(None)["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(v["serverInfo"]["name"], "findery");
    }

    #[test]
    fn a_failed_tool_call_is_a_result_not_a_protocol_error() {
        // MCP wants tool failures visible to the model, not swallowed by the
        // transport, so they ride back as `isError` on a normal result.
        let v = text_result("project `x` not found", true);
        assert_eq!(v["isError"], true);
        assert_eq!(v["content"][0]["type"], "text");
    }
}

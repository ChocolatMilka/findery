//! Minimal JSON-RPC 2.0 over stdio, the transport MCP speaks.
//!
//! INTENT: the protocol surface an MCP server actually needs is `initialize`,
//! `tools/list`, `tools/call`, `prompts/list`, `prompts/get` and `ping`.
//! Hand-rolling it is about a hundred lines and pins the wire format to
//! something this repository controls, instead of to a client library that
//! revises its API faster than the spec it implements.

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

/// A JSON-RPC error: its code and a message for a human.
#[derive(Debug, Serialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Outgoing {
    /// A successful answer to request `id`.
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

/// What the model reads once, at the start of a session: when to reach for
/// which tool. A decision table, because that is what it consults mid-task.
pub const INSTRUCTIONS: &str = "findery indexes this codebase by meaning and by identifier: it is your eyes on code you have not opened. `project` is optional everywhere — it is detected from .findery/config.toml above the working directory, else from the registered root that contains it.

| You need | Call |
|---|---|
| where or how something is done, a concept, a behaviour | search {query} |
| a definition / callers / implementors / tests / a setting / an error | search {query, intent: symbol / usage / implementations / test / config / error} |
| everything a review, refactor, doc check or security pass must read | search {query, intent: review / refactor / doc-sync / divergence / security / onboard} — or the prompts of the same names |
| an exact identifier or message | search {query, mode: \"keyword\"}; grep when you need every occurrence |
| the rest of a hit, or the lines around it | read {locator: \"path:L12-L40\", context: 20} |
| which files exist under a path | read {glob: \"crates/**/*.rs\"} |
| a file you are about to edit | your own Read tool |
| results that miss code you just wrote | sync {paths: [...]} |

Cite hits by their locator. A `note:` line above the hits means the answer is narrower than asked (keyword-only, stale index): read it first.";

/// The `initialize` result: what this server is and what it can do.
pub fn initialize_result(requested_version: Option<&str>) -> Value {
    json!({
        // Echoing the client's version when it states one keeps older clients
        // working; the spec allows the server to answer with its own otherwise.
        "protocolVersion": requested_version.unwrap_or(PROTOCOL_VERSION),
        "capabilities": {
            "tools": { "listChanged": false },
            "prompts": { "listChanged": false },
        },
        "serverInfo": {
            "name": "findery",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
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
        assert_eq!(v["capabilities"]["prompts"]["listChanged"], false);
    }

    #[test]
    fn the_instructions_are_a_decision_table_over_the_five_tools() {
        for tool in ["search {", "read {", "sync {"] {
            assert!(INSTRUCTIONS.contains(tool), "{tool}");
        }
        assert!(INSTRUCTIONS.contains("optional everywhere"));
        assert!(
            INSTRUCTIONS.len() < 2_000,
            "read once per session: keep it short"
        );
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

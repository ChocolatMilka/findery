//! # findery-mcp — layer L4
//!
//! The MCP bridge. It owns no state: every call becomes one HTTP request to the
//! daemon through [`findery_client::Client`].
//!
//! INTENT: keeping the bridge stateless is what lets several editors, several
//! projects and several machines share one warm model. The bridge is a wire,
//! not a cache.

pub mod catalogue;
pub mod protocol;
pub mod tools;

use findery_client::Client;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Serves the MCP protocol on the given streams until the input closes.
///
/// Reading and writing are injected so the whole loop is testable without a
/// process, a pipe or a live daemon.
pub async fn serve<R, W>(client: Client, input: R, mut output: W) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle_line(&client, &line).await else {
            continue; // a notification: acknowledged by silence, per JSON-RPC
        };
        let mut bytes = serde_json::to_vec(&response).unwrap_or_else(|e| {
            serde_json::to_vec(&protocol::Outgoing::err(
                Value::Null,
                -32603,
                format!("cannot serialize response: {e}"),
            ))
            .unwrap_or_default()
        });
        bytes.push(b'\n');
        output.write_all(&bytes).await?;
        output.flush().await?;
    }
    Ok(())
}

/// Handles one line. `None` means "notification, nothing to answer".
pub async fn handle_line(client: &Client, line: &str) -> Option<protocol::Outgoing> {
    let message: protocol::Incoming = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(e) => {
            // A malformed frame still deserves an answer, with a null id.
            return Some(protocol::Outgoing::err(
                Value::Null,
                -32700,
                format!("parse error: {e}"),
            ));
        }
    };
    let id = message.id.clone()?;

    let response = match message.method.as_str() {
        "initialize" => protocol::Outgoing::ok(
            id,
            protocol::initialize_result(
                message
                    .params
                    .get("protocolVersion")
                    .and_then(Value::as_str),
            ),
        ),
        "ping" => protocol::Outgoing::ok(id, json!({})),
        "tools/list" => protocol::Outgoing::ok(id, catalogue::catalogue()),
        "tools/call" => {
            let name = message
                .params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = message
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            // A failing tool is a normal result with `isError`: the model must
            // see what went wrong and adapt, not receive a transport error it
            // cannot reason about.
            let result = match tools::call(client, name, &args).await {
                Ok(text) => protocol::text_result(text, false),
                Err(e) => {
                    tracing::warn!(tool = name, error = %e, "tool call failed");
                    protocol::text_result(format!("{name} failed: {e}"), true)
                }
            };
            protocol::Outgoing::ok(id, result)
        }
        other => protocol::Outgoing::err(id, -32601, format!("unknown method `{other}`")),
    };
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        // Port 1 is never listening; only offline paths are exercised here.
        Client::new("http://127.0.0.1:1").expect("client")
    }

    async fn ask(line: &str) -> Option<Value> {
        handle_line(&client(), line)
            .await
            .map(|r| serde_json::to_value(r).expect("serialize"))
    }

    #[tokio::test]
    async fn initialize_advertises_tools() {
        let v = ask(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#)
            .await
            .expect("response");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
        assert!(
            v["result"]["instructions"]
                .as_str()
                .expect("instructions")
                .contains("search_code")
        );
    }

    #[tokio::test]
    async fn notifications_get_no_answer() {
        assert!(
            ask(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn tools_list_returns_the_catalogue() {
        let v = ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .expect("response");
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"search_code"), "{names:?}");
        assert!(names.contains(&"sync_project"), "{names:?}");
        assert!(names.contains(&"remove_document"), "{names:?}");
    }

    #[tokio::test]
    async fn a_daemon_that_is_down_becomes_a_readable_tool_error() {
        let v = ask(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"list_projects","arguments":{}}}"#,
        )
        .await
        .expect("response");
        assert!(v["error"].is_null(), "transport must stay healthy: {v}");
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("list_projects failed"), "{text}");
    }

    #[tokio::test]
    async fn an_unknown_method_is_a_protocol_error() {
        let v = ask(r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#)
            .await
            .expect("response");
        assert_eq!(v["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn a_malformed_frame_does_not_kill_the_session() {
        let v = ask("{not json").await.expect("response");
        assert_eq!(v["error"]["code"], -32700);
        assert!(v["id"].is_null());
    }

    #[tokio::test]
    async fn the_loop_answers_every_request_line_in_order() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n"
        );
        let mut out = Vec::new();
        serve(client(), input.as_bytes(), &mut out)
            .await
            .expect("serve");

        let lines: Vec<&str> = std::str::from_utf8(&out)
            .expect("utf8")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        assert_eq!(
            lines.len(),
            2,
            "one answer per request, none for the notification"
        );
        let first: Value = serde_json::from_str(lines[0]).expect("json");
        let second: Value = serde_json::from_str(lines[1]).expect("json");
        assert_eq!(first["id"], 1);
        assert_eq!(second["id"], 2);
    }
}

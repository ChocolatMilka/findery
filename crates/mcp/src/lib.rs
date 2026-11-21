//! # findery-mcp — layer L4
//!
//! The MCP bridge. It owns no index state: every call becomes HTTP requests to
//! the daemon through [`findery_client::Client`]. What it does own is where it
//! stands — the directory it was started in — because that is what lets a call
//! leave `project` out.
//!
//! INTENT: keeping the bridge stateless is what lets several editors, several
//! projects and several machines share one warm model. The bridge is a wire,
//! not a cache.

pub mod args;
pub mod catalogue;
pub mod prompts;
pub mod protocol;
pub mod render;
pub mod tools;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use findery_core::Result;
use findery_core::ids::ProjectId;

use findery_client::Client;

use args::Args;

/// What a tool call tells an agent standing in no project.
const REPAIR: &str = "pass `project` (one of the ids above), or index this directory with \
                      projects {action: \"register\", root: \"<absolute path>\"}";

/// The daemon to talk to, and the directory the bridge stands in.
pub struct Session {
    client: Client,
    cwd: PathBuf,
}

impl Session {
    /// A session over `client`, standing in `cwd`.
    pub fn new(client: Client, cwd: PathBuf) -> Self {
        Self { client, cwd }
    }

    /// The daemon's client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The directory a call without `project` is resolved from.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The project a call means: its `project` argument, else the manifest at
    /// or above the bridge's directory, else the deepest registered root
    /// holding it.
    pub async fn project(&self, args: &Args<'_>) -> Result<ProjectId> {
        let explicit = args.text("project")?;
        self.client
            .resolve_project(explicit.as_deref(), &self.cwd, REPAIR)
            .await
    }

    /// The project a call without `project` would use, if any.
    pub async fn detected(&self) -> Option<ProjectId> {
        self.client
            .resolve_project(None, &self.cwd, REPAIR)
            .await
            .ok()
    }
}

/// Serves the MCP protocol on the given streams until the input closes.
///
/// Reading and writing are injected so the whole loop is testable without a
/// process, a pipe or a live daemon.
pub async fn serve<R, W>(session: &Session, input: R, mut output: W) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let Some(response) = handle_line(session, &line).await else {
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
pub async fn handle_line(session: &Session, line: &str) -> Option<protocol::Outgoing> {
    let message: protocol::Incoming = match serde_json::from_str(line) {
        Ok(m) => m,
        // A malformed frame still deserves an answer, with a null id.
        Err(e) => {
            let reply = protocol::Outgoing::err(Value::Null, -32700, format!("parse error: {e}"));
            return Some(reply);
        }
    };
    let id = message.id.clone()?;
    Some(
        match answer(session, &message.method, &message.params).await {
            Ok(result) => protocol::Outgoing::ok(id, result),
            Err((code, text)) => protocol::Outgoing::err(id, code, text),
        },
    )
}

/// The result of one request, or its JSON-RPC error.
async fn answer(
    session: &Session,
    method: &str,
    params: &Value,
) -> std::result::Result<Value, (i32, String)> {
    match method {
        "initialize" => {
            let version = params.get("protocolVersion").and_then(Value::as_str);
            Ok(protocol::initialize_result(version))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(catalogue::catalogue()),
        "tools/call" => Ok(call_tool(session, params).await),
        "prompts/list" => Ok(prompts::list()),
        "prompts/get" => prompts::get(session, params)
            .await
            .map_err(|refused| (-32602, refused.0)),
        other => Err((-32601, format!("unknown method `{other}`"))),
    }
}

/// A failing tool is a normal result with `isError`: the model must see what
/// went wrong and adapt, not receive a transport error it cannot reason about.
async fn call_tool(session: &Session, params: &Value) -> Value {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = params.get("arguments").unwrap_or(&Value::Null);
    match tools::call(session, name, args).await {
        Ok(text) => protocol::text_result(text, false),
        Err(e) => {
            tracing::warn!(tool = name, error = %e, "tool call failed");
            protocol::text_result(format!("{name} failed: {e}"), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        // Port 1 is never listening; only offline paths are exercised here.
        let client = Client::new("http://127.0.0.1:1").expect("client");
        Session::new(client, std::env::temp_dir())
    }

    async fn ask(line: &str) -> Option<Value> {
        handle_line(&session(), line)
            .await
            .map(|r| serde_json::to_value(r).expect("serialize"))
    }

    #[tokio::test]
    async fn initialize_advertises_tools_and_prompts() {
        let v = ask(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#)
            .await
            .expect("response");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(v["result"]["capabilities"]["prompts"]["listChanged"], false);
        let instructions = v["result"]["instructions"].as_str().expect("instructions");
        assert!(instructions.contains("search {query}"), "{instructions}");
    }

    #[tokio::test]
    async fn notifications_get_no_answer() {
        let silent = ask(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert!(silent.is_none());
    }

    #[tokio::test]
    async fn tools_list_is_exactly_the_five_tools() {
        let v = ask(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#)
            .await
            .expect("response");
        let names: Vec<&str> = v["result"]["tools"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names, ["search", "read", "projects", "sync", "note"]);
    }

    #[tokio::test]
    async fn a_daemon_that_is_down_becomes_a_readable_tool_error() {
        let v = ask(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"projects","arguments":{}}}"#,
        )
        .await
        .expect("response");
        assert!(v["error"].is_null(), "transport must stay healthy: {v}");
        assert_eq!(v["result"]["isError"], true);
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        assert!(text.contains("projects failed"), "{text}");
    }

    #[tokio::test]
    async fn a_strict_argument_is_refused_before_the_daemon_is_asked() {
        let v = ask(
            r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search","arguments":{"query":"x","langs":["cobol"]}}}"#,
        )
        .await
        .expect("response");
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        assert!(
            text.contains("unknown `langs` value `cobol`; valid: rust"),
            "{text}"
        );
        assert!(
            !text.contains("unreachable"),
            "refused before any I/O: {text}"
        );
    }

    #[tokio::test]
    async fn an_old_tool_name_is_refused_naming_the_five() {
        let v = ask(
            r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search_code","arguments":{}}}"#,
        )
        .await
        .expect("response");
        let text = v["result"]["content"][0]["text"].as_str().expect("text");
        assert!(
            text.contains("the tools are: search, read, projects, sync, note"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn prompts_are_listed_and_an_unknown_one_is_an_invalid_params_error() {
        let v = ask(r#"{"jsonrpc":"2.0","id":7,"method":"prompts/list"}"#)
            .await
            .expect("response");
        assert_eq!(v["result"]["prompts"].as_array().map(Vec::len), Some(7));
        let v =
            ask(r#"{"jsonrpc":"2.0","id":8,"method":"prompts/get","params":{"name":"deploy"}}"#)
                .await
                .expect("response");
        assert_eq!(v["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn resources_stay_unsupported() {
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
        serve(&session(), input.as_bytes(), &mut out)
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

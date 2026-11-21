//! The bridge end to end: JSON-RPC lines in, text out, against a real daemon
//! — router, engine, chunkers, SQLite — served on loopback inside the test.
//! Only the embedding model is the mock (CONV-036); nothing leaves the
//! machine.
//!
//! What this pins is what an agent relies on: a call without `project`
//! finds it, a read returns the lines, a re-registration keeps what the
//! manifest declared, a sync answers within its wait, a note round-trips, and
//! a default search stays inside its token budget.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use findery_mcp::Session;
use serde_json::{Value, json};

use findery_core::api::RegisterProject;
use findery_core::config::{Config, EmbedderBackend};
use findery_core::model::{ProjectSpec, SearchFilter};

use findery_client::Client;

struct Daemon {
    _data: tempfile::TempDir,
    project: tempfile::TempDir,
    client: Client,
}

impl Daemon {
    async fn start() -> Self {
        let data = tempfile::tempdir().expect("data dir");
        let mut cfg = Config::default();
        cfg.embedder.backend = EmbedderBackend::Mock;
        cfg.embedder.dim = 128;
        cfg.store.data_dir = data.path().display().to_string();
        cfg.store.export_dir = data.path().join("exports").display().to_string();
        cfg.chunk.min_chars = 1;
        cfg.models.insert(
            "other".into(),
            toml::from_str("weights = \"other\"").expect("profile"),
        );
        let state = findery_server::state_from_config(cfg).await.expect("state");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        tokio::spawn(async move { axum::serve(listener, findery_server::app(state)).await });
        Self {
            _data: data,
            project: fixture(),
            client: Client::new(url).expect("client"),
        }
    }

    fn root(&self) -> PathBuf {
        self.project.path().to_path_buf()
    }

    fn root_text(&self) -> String {
        self.root().display().to_string().replace('\\', "/")
    }

    /// Registers the fixture as `demo` and indexes it.
    async fn indexed(&self, extra: RegisterProject) -> findery_core::ids::ProjectId {
        let project = self
            .client
            .register(&RegisterProject {
                root: self.root_text(),
                id: Some("demo".into()),
                ..extra
            })
            .await
            .expect("register");
        self.client.sync_now(&project.id).await.expect("sync");
        project.id
    }

    fn session_in(&self, cwd: &Path) -> Session {
        Session::new(self.client.clone(), cwd.to_path_buf())
    }
}

/// Two Rust files and a README.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("project dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(
        root.join("src/net.rs"),
        "//! Networking helpers.\n\n\
         /// Retries an operation with exponential backoff.\n\
         pub fn retry_with_backoff(attempts: u32) -> bool {\n    attempts < 5\n}\n\n\
         /// Opens a pooled connection.\n\
         pub fn connect(url: &str) -> Option<()> {\n    let _ = url;\n    Some(())\n}\n",
    )
    .expect("write");
    std::fs::write(
        root.join("src/pool.rs"),
        "/// A bounded pool of connections.\npub struct Pool {\n    size: usize,\n}\n",
    )
    .expect("write");
    std::fs::write(
        root.join("README.md"),
        "# Demo\n\nRetry policy lives in src/net.rs.\n\n## Pool\n\nPools are bounded.\n",
    )
    .expect("write");
    dir
}

/// One `tools/call`, as Claude Code sends it: `(is_error, text)`.
async fn call(session: &Session, tool: &str, arguments: Value) -> (bool, String) {
    let line = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });
    let reply = findery_mcp::handle_line(session, &line.to_string())
        .await
        .expect("a request gets an answer");
    let reply = serde_json::to_value(reply).expect("serialize");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .expect("text")
        .to_string();
    (reply["result"]["isError"] == true, text)
}

fn manifest(root: &Path, body: &str) {
    let dir = root.join(".findery");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(dir.join("config.toml"), body).expect("write");
}

#[tokio::test]
async fn a_search_without_project_resolves_from_the_manifest_above_the_cwd() {
    let daemon = Daemon::start().await;
    daemon.indexed(RegisterProject::default()).await;
    manifest(&daemon.root(), "project = \"demo\"\n");
    let session = daemon.session_in(&daemon.root().join("src"));

    let (error, text) = call(&session, "search", json!({ "query": "retry with backoff" })).await;
    assert!(!error, "{text}");
    assert!(text.contains("· `demo` ·"), "{text}");
    assert!(text.contains("src/net.rs:L"), "{text}");
    assert!(
        text.contains("model embedder"),
        "the header names the model: {text}"
    );
}

#[tokio::test]
async fn a_registered_root_is_detected_when_there_is_no_manifest() {
    let daemon = Daemon::start().await;
    daemon.indexed(RegisterProject::default()).await;
    let session = daemon.session_in(&daemon.root().join("src"));

    let (error, text) = call(&session, "projects", json!({})).await;
    assert!(!error, "{text}");
    assert!(
        text.contains("* `demo`"),
        "the detected project is marked: {text}"
    );
    assert!(text.contains("rust "), "with its language mix: {text}");
}

#[tokio::test]
async fn a_directory_in_no_project_is_told_which_projects_exist() {
    let daemon = Daemon::start().await;
    daemon.indexed(RegisterProject::default()).await;
    let elsewhere = tempfile::tempdir().expect("elsewhere");
    let session = daemon.session_in(elsewhere.path());

    let (error, text) = call(&session, "search", json!({ "query": "retry" })).await;
    assert!(error, "{text}");
    assert!(text.contains("no indexed project contains"), "{text}");
    assert!(text.contains("known projects: `demo`"), "{text}");
    assert!(text.contains("projects {action: \"register\""), "{text}");
}

#[tokio::test]
async fn a_read_returns_the_lines_of_a_locator_with_their_context() {
    let daemon = Daemon::start().await;
    daemon.indexed(RegisterProject::default()).await;
    let session = daemon.session_in(&daemon.root());

    let (error, text) = call(
        &session,
        "read",
        json!({ "locator": "src/net.rs:L4-L4", "context": 1 }),
    )
    .await;
    assert!(!error, "{text}");
    assert!(text.contains("src/net.rs:L3-L5 of "), "{text}");
    assert!(text.contains("from disk"), "{text}");
    assert!(text.contains("pub fn retry_with_backoff"), "{text}");

    let (error, text) = call(&session, "read", json!({ "glob": "src/**" })).await;
    assert!(!error, "{text}");
    assert!(text.contains("2 indexed file(s)"), "{text}");
    assert!(text.contains("src/pool.rs · rust ·"), "{text}");
    assert!(!text.contains("README.md"), "{text}");

    let (error, text) = call(
        &session,
        "read",
        json!({ "glob": "src/**", "locator": "a:L1" }),
    )
    .await;
    assert!(error && text.contains("send it alone"), "{text}");
}

#[tokio::test]
async fn a_note_is_read_back_from_the_index_and_removed() {
    let daemon = Daemon::start().await;
    daemon.indexed(RegisterProject::default()).await;
    let session = daemon.session_in(&daemon.root());

    let text = "# Decision\n\nRetries stop after five attempts.\n";
    let (error, out) = call(
        &session,
        "note",
        json!({ "path": "notes/decision.md", "text": text }),
    )
    .await;
    assert!(!error && out.contains("noted `notes/decision.md`"), "{out}");

    let (error, out) = call(
        &session,
        "read",
        json!({ "locator": "notes/decision.md:L3" }),
    )
    .await;
    assert!(!error, "{out}");
    assert!(out.contains("from the index"), "a note has no file: {out}");
    assert!(out.contains("Retries stop after five attempts."), "{out}");

    let (_, out) = call(
        &session,
        "note",
        json!({ "action": "remove", "path": "notes/decision.md" }),
    )
    .await;
    assert!(out.contains("removed `notes/decision.md`"), "{out}");
    let (_, out) = call(
        &session,
        "note",
        json!({ "action": "remove", "path": "notes/decision.md" }),
    )
    .await;
    assert!(out.contains("nothing indexed at"), "{out}");
}

/// The regression the consolidation fixed: registering a root again from the
/// bridge — which never read the manifest — used to wipe what it declared.
#[tokio::test]
async fn a_bridge_registration_keeps_scopes_instruction_intents_and_pin() {
    let daemon = Daemon::start().await;
    let scopes = BTreeMap::from([(
        "api".to_string(),
        SearchFilter {
            paths: vec!["src/**".into()],
            ..SearchFilter::default()
        },
    )]);
    let intents =
        toml::from_str("[usage]\ninstruction = \"Given an API, retrieve its importers.\"\n")
            .expect("layer");
    let id = daemon
        .indexed(RegisterProject {
            scopes: Some(scopes.clone()),
            instruction: Some("Given a question, retrieve the answer.".into()),
            intents: Some(intents),
            model: Some("other".into()),
            exclude: Some(vec!["**/generated/**".into()]),
            ..RegisterProject::default()
        })
        .await;
    let session = daemon.session_in(&daemon.root());

    let (error, text) = call(
        &session,
        "projects",
        json!({ "action": "register", "root": daemon.root_text() }),
    )
    .await;
    assert!(!error, "{text}");
    assert!(text.contains("kept: scopes api"), "{text}");

    let spec: ProjectSpec = daemon.client.project(&id).await.expect("project").spec;
    assert_eq!(spec.scopes, scopes);
    assert_eq!(
        spec.instruction.as_deref(),
        Some("Given a question, retrieve the answer.")
    );
    assert!(spec.intents.contains_key("usage"), "{:?}", spec.intents);
    assert_eq!(spec.model.as_ref().map(|m| m.as_str()), Some("other"));
    assert!(spec.exclude.contains(&"**/generated/**".to_string()));
}

#[tokio::test]
async fn a_bridge_registration_of_a_new_root_carries_its_manifest() {
    let daemon = Daemon::start().await;
    manifest(
        &daemon.root(),
        "project = \"cloned\"\ninstruction = \"Given X, retrieve Y.\"\n\n[scopes.docs]\npaths = [\"**/*.md\"]\n",
    );
    let session = daemon.session_in(&daemon.root());
    let (error, text) = call(
        &session,
        "projects",
        json!({ "action": "register", "root": daemon.root_text() }),
    )
    .await;
    assert!(!error, "{text}");
    assert!(
        text.contains("registered `cloned`"),
        "the manifest names the id: {text}"
    );
    let id = findery_core::ids::ProjectId::new("cloned").expect("id");
    let spec = daemon.client.project(&id).await.expect("project").spec;
    assert!(spec.scopes.contains_key("docs"));
    assert_eq!(spec.instruction.as_deref(), Some("Given X, retrieve Y."));
}

#[tokio::test]
async fn a_sync_answers_its_report_within_the_wait_or_its_job_to_follow() {
    let daemon = Daemon::start().await;
    let project = daemon
        .client
        .register(&RegisterProject {
            root: daemon.root_text(),
            id: Some("demo".into()),
            ..RegisterProject::default()
        })
        .await
        .expect("register");
    let session = daemon.session_in(&daemon.root());

    let (error, text) = call(&session, "sync", json!({ "wait": 0 })).await;
    assert!(!error, "{text}");
    let job = text
        .split("job `")
        .nth(1)
        .and_then(|rest| rest.split('`').next())
        .unwrap_or_else(|| panic!("a job id: {text}"))
        .to_string();
    assert!(text.contains("sync {job: \""), "{text}");

    let (error, text) = call(&session, "sync", json!({ "job": job, "wait": 30 })).await;
    assert!(!error, "{text}");
    assert!(
        text.contains(&format!("sync `{}` (incremental) done", project.id)),
        "{text}"
    );

    let (error, text) = call(&session, "sync", json!({ "paths": ["src/net.rs"] })).await;
    assert!(!error && text.contains("(paths) done"), "{text}");

    let (error, text) = call(&session, "sync", json!({ "mode": "turbo" })).await;
    assert!(error && text.contains("valid: incremental, full"), "{text}");
}

#[tokio::test]
async fn a_workflow_search_answers_one_section_per_leg() {
    let daemon = Daemon::start().await;
    daemon.indexed(RegisterProject::default()).await;
    let session = daemon.session_in(&daemon.root());
    let (error, text) = call(
        &session,
        "search",
        json!({ "query": "retry_with_backoff", "intent": "review" }),
    )
    .await;
    assert!(!error, "{text}");
    assert!(
        text.contains("intent review · 4 legs: callers, tests, docs, similar"),
        "{text}"
    );
    for leg in ["## callers", "## tests", "## docs", "## similar"] {
        assert!(text.contains(leg), "{leg}: {text}");
    }
}

/// The budget the docs quote, measured through the real daemon: eight hits of
/// functions far longer than the cap, at the bridge's defaults.
#[tokio::test]
async fn a_default_search_of_eight_long_hits_stays_inside_its_token_budget() {
    let daemon = Daemon::start().await;
    let body = "    let step = state.advance(&input)?; // keeps the engine moving\n".repeat(28);
    for i in 0..10 {
        let text = format!(
            "/// Engine step {i}.\npub fn step_{i}(state: &mut State) -> Result<()> {{\n{body}    Ok(())\n}}\n"
        );
        std::fs::write(daemon.root().join(format!("src/engine_{i}.rs")), text).expect("write");
    }
    daemon.indexed(RegisterProject::default()).await;
    let session = daemon.session_in(&daemon.root());
    let (error, text) = call(
        &session,
        "search",
        json!({ "query": "how does the engine step" }),
    )
    .await;
    assert!(!error, "{text}");
    assert_eq!(text.matches("\n[").count(), 8, "{text}");
    eprintln!("default search, k = 8: {} chars", text.len());
    assert!(text.len() <= 14_000, "{} chars", text.len());
}

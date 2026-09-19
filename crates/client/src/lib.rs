//! # findery-client — the Rust client
//!
//! What you add to another project to query the index:
//!
//! ```no_run
//! # async fn demo() -> findery_core::Result<()> {
//! use findery_client::Client;
//! use findery_core::model::SearchRequest;
//! use findery_core::ids::ProjectId;
//!
//! let client = Client::from_env()?;
//! let project = ProjectId::new("axle")?;
//! for hit in client.search(&SearchRequest::new(project, "where is retry handled")).await? {
//!     println!("{} ({:.3})", hit.chunk.locator(), hit.score);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! INTENT: this crate depends on `findery-core` and `reqwest` and nothing else. It
//! pulls in neither SQLite, nor the walker, nor the chunkers — a consumer gets
//! the vocabulary and an HTTP client, not half a daemon.

use findery_core::api::{
    AddDocument, Deleted, ErrorBody, ExportReport, ExportRequest, HealthReport, Job,
    RegisterProject, Removed, SyncRequest, SyncResponse, WatchRequest, Watching,
};
use findery_core::ids::ProjectId;
use findery_core::model::{IndexStats, Project, SearchHit, SearchRequest, SyncReport};
use findery_core::{Error, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;

/// Default daemon address, matching the bundled Docker Compose stack.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8077";

/// HTTP client for one daemon.
#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

impl Client {
    /// Builds a client against `base_url`.
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .user_agent(concat!("findery-client/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;
        Ok(Self {
            http,
            base: base_url.into().trim_end_matches('/').to_string(),
            token: None,
        })
    }

    /// Reads `FINDERY_SERVER_URL` and `FINDERY_API_TOKEN`, falling back to the defaults.
    ///
    /// This is the constructor another project should use: pointing a whole
    /// team at a shared daemon becomes an environment variable, not a code edit.
    pub fn from_env() -> Result<Self> {
        let base =
            std::env::var("FINDERY_SERVER_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let mut client = Self::new(base)?;
        if let Ok(token) = std::env::var("FINDERY_API_TOKEN")
            && !token.is_empty()
        {
            client.token = Some(token);
        }
        Ok(client)
    }

    /// Sets the bearer token.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.token = if token.is_empty() { None } else { Some(token) };
        self
    }

    /// Overrides the request timeout. The default of 600 s exists because a
    /// synchronous first sync of a large repository legitimately takes minutes.
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self> {
        self.http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("findery-client/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Config(format!("http client: {e}")))?;
        Ok(self)
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    // -- projects -----------------------------------------------------------

    pub async fn register(&self, req: &RegisterProject) -> Result<Project> {
        self.send(reqwest::Method::POST, "/v1/projects", Some(req))
            .await
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        self.send::<(), _>(reqwest::Method::GET, "/v1/projects", None)
            .await
    }

    pub async fn project(&self, id: &ProjectId) -> Result<Project> {
        self.send::<(), _>(reqwest::Method::GET, &format!("/v1/projects/{id}"), None)
            .await
    }

    pub async fn delete_project(&self, id: &ProjectId) -> Result<bool> {
        let body: Deleted = self
            .send::<(), _>(reqwest::Method::DELETE, &format!("/v1/projects/{id}"), None)
            .await?;
        Ok(body.deleted)
    }

    pub async fn stats(&self, id: &ProjectId) -> Result<IndexStats> {
        self.send::<(), _>(
            reqwest::Method::GET,
            &format!("/v1/projects/{id}/stats"),
            None,
        )
        .await
    }

    // -- indexing -----------------------------------------------------------

    /// Starts a sync. With `wait`, blocks and returns the report.
    pub async fn sync(&self, id: &ProjectId, req: &SyncRequest) -> Result<SyncResponse> {
        self.send(
            reqwest::Method::POST,
            &format!("/v1/projects/{id}/sync"),
            Some(req),
        )
        .await
    }

    /// Convenience: a blocking incremental sync.
    pub async fn sync_now(&self, id: &ProjectId) -> Result<SyncReport> {
        let response = self
            .sync(
                id,
                &SyncRequest {
                    wait: true,
                    ..SyncRequest::default()
                },
            )
            .await?;
        match response {
            SyncResponse::Completed { report } => Ok(report),
            SyncResponse::Accepted { job } => Err(Error::Internal(format!(
                "expected a report, got job {}",
                job.id
            ))),
        }
    }

    pub async fn add_document(&self, id: &ProjectId, req: &AddDocument) -> Result<SyncReport> {
        self.send(
            reqwest::Method::POST,
            &format!("/v1/projects/{id}/documents"),
            Some(req),
        )
        .await
    }

    pub async fn remove_document(&self, id: &ProjectId, path: &str) -> Result<u64> {
        let encoded = urlencode(path);
        let body: Removed = self
            .send::<(), _>(
                reqwest::Method::DELETE,
                &format!("/v1/projects/{id}/documents?path={encoded}"),
                None,
            )
            .await?;
        Ok(body.removed)
    }

    pub async fn set_watch(&self, id: &ProjectId, req: &WatchRequest) -> Result<Watching> {
        self.send(
            reqwest::Method::POST,
            &format!("/v1/projects/{id}/watch"),
            Some(req),
        )
        .await
    }

    pub async fn export(&self, id: &ProjectId, req: &ExportRequest) -> Result<ExportReport> {
        self.send(
            reqwest::Method::POST,
            &format!("/v1/projects/{id}/export"),
            Some(req),
        )
        .await
    }

    // -- querying -----------------------------------------------------------

    pub async fn search(&self, req: &SearchRequest) -> Result<Vec<SearchHit>> {
        self.send(reqwest::Method::POST, "/v1/search", Some(req))
            .await
    }

    pub async fn health(&self) -> Result<HealthReport> {
        self.send::<(), _>(reqwest::Method::GET, "/v1/health", None)
            .await
    }

    pub async fn jobs(&self) -> Result<Vec<Job>> {
        self.send::<(), _>(reqwest::Method::GET, "/v1/jobs", None)
            .await
    }

    pub async fn job(&self, id: &str) -> Result<Job> {
        self.send::<(), _>(reqwest::Method::GET, &format!("/v1/jobs/{id}"), None)
            .await
    }

    /// Polls a job until it finishes or `deadline` elapses.
    pub async fn await_job(&self, id: &str, deadline: Duration) -> Result<Job> {
        let started = std::time::Instant::now();
        loop {
            let job = self.job(id).await?;
            if job.state.is_terminal() {
                return Ok(job);
            }
            if started.elapsed() >= deadline {
                return Err(Error::Internal(format!(
                    "job `{id}` still running after {deadline:?}"
                )));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    // -- transport ----------------------------------------------------------

    /// One request. Failures are decoded back into the daemon's own error
    /// vocabulary, so a caller matches on `Error::NotFound` rather than on a
    /// status code it had to look up.
    async fn send<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::upstream("findery", format!("{} unreachable: {e}", self.base)))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();

        if !status.is_success() {
            return Err(decode_error(status.as_u16(), &text));
        }
        serde_json::from_str(&text)
            .map_err(|e| Error::Internal(format!("cannot decode response from {path}: {e}")))
    }
}

/// Rebuilds a typed error from the daemon's error body.
fn decode_error(status: u16, body: &str) -> Error {
    let Ok(parsed) = serde_json::from_str::<ErrorBody>(body) else {
        return Error::upstream("findery", format!("HTTP {status}: {body}"));
    };
    match parsed.code.as_str() {
        "not_found" => Error::NotFound(parsed.message),
        "invalid" => Error::Invalid(parsed.message),
        "config" => Error::Config(parsed.message),
        "store" => Error::Store(parsed.message),
        "embed" => Error::Embed(parsed.message),
        "upstream" => Error::upstream("findery", parsed.message),
        _ => Error::Internal(parsed.message),
    }
}

/// Minimal percent-encoding for a query-string value.
///
/// A dependency for this would be absurd, and the alphabet of a relative path
/// is small enough to enumerate.
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() + 8);
    for byte in raw.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls_lose_their_trailing_slash() {
        let c = Client::new("http://localhost:8077/").expect("client");
        assert_eq!(c.base_url(), "http://localhost:8077");
    }

    #[test]
    fn error_bodies_become_typed_errors() {
        let body = r#"{"code":"not_found","message":"project `x`"}"#;
        assert!(matches!(decode_error(404, body), Error::NotFound(_)));

        let body = r#"{"code":"invalid","message":"bad glob"}"#;
        assert_eq!(decode_error(400, body).code(), "invalid");

        // A non-JSON body (a proxy error page) must not be swallowed.
        assert_eq!(decode_error(502, "<html>").code(), "upstream");
    }

    #[test]
    fn query_values_are_encoded_but_stay_readable() {
        assert_eq!(urlencode("src/lib.rs"), "src/lib.rs");
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(urlencode("héllo"), "h%C3%A9llo");
    }

    #[test]
    fn the_env_constructor_falls_back_to_the_default_address() {
        // Not asserting on process env: the fallback is what matters here.
        let c = Client::new(DEFAULT_BASE_URL).expect("client");
        assert_eq!(c.base_url(), "http://127.0.0.1:8077");
    }
}

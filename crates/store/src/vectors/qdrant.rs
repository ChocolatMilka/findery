//! Qdrant vector store, over its REST API.
//!
//! INTENT: the REST surface is used deliberately instead of the gRPC client
//! crate. It keeps the dependency tree to `reqwest`, which is already here for
//! the embedder, and it decouples our release cadence from a client library
//! that tracks the server's major versions.
//!
//! One collection per project and bucket member, named `<prefix><project_id>`
//! for a file with no namespace and `<prefix><namespace>.<project_id>` for one
//! with. The namespace is the member file's `meta.vector_namespace`: empty for
//! a file written before members existed, so its collections keep the names
//! they already have, and the recipe tag for a member — two models of the same
//! width would otherwise write into one collection and be scored against each
//! other. The `.` is outside both alphabets, so no namespace and id can spell
//! another pair's name. Exact-match
//! predicates (`lang`, `kind`) are pushed into the server filter; path globs
//! and symbol substrings are evaluated locally after over-fetching, because
//! Qdrant has no glob predicate and a wrong translation is worse than a second
//! pass over a few hundred rows.

use std::time::Duration;

use serde_json::{Value, json};

use findery_core::config::QdrantConfig;
use findery_core::ids::{ChunkId, ProjectId};
use findery_core::model::filter::CompiledFilter;
use findery_core::model::{ChunkKind, Lang, ScoredChunk, SearchFilter};
use findery_core::traits::{VectorPayload, VectorRecord, VectorStore};
use findery_core::{Error, Result};

const SERVICE: &str = "qdrant";
/// Over-fetch factor when a predicate has to be applied client-side.
const LOCAL_FILTER_FANOUT: usize = 8;
const LOCAL_FILTER_CAP: usize = 1024;

/// Longest namespace accepted: a recipe tag, with room.
const NAMESPACE_MAX: usize = 32;
/// Between a namespace and a project id: in neither alphabet.
const NAMESPACE_SEPARATOR: char = '.';

/// A vector store backed by a Qdrant cluster, one collection per project.
pub struct QdrantStore {
    http: reqwest::Client,
    cfg: QdrantConfig,
    /// Inserted between the configured prefix and the project id.
    namespace: String,
}

/// Refuses a namespace that could not sit inside a collection name.
///
/// `[a-z0-9_-]`, at most [`NAMESPACE_MAX`] characters, empty allowed: the same
/// alphabet a project id uses, so the collection name stays one path segment.
pub fn check_namespace(namespace: &str) -> Result<()> {
    let ok = namespace.len() <= NAMESPACE_MAX
        && namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "vector namespace `{namespace}`: expected at most {NAMESPACE_MAX} characters of \
             a-z, 0-9, `_` and `-`"
        )))
    }
}

impl QdrantStore {
    /// A client for the cluster `cfg` names, writing under `namespace`.
    /// Nothing is contacted until the first call.
    pub fn new(cfg: QdrantConfig, namespace: &str) -> Result<Self> {
        check_namespace(namespace)?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .user_agent(concat!("findery/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Config(format!("qdrant http client: {e}")))?;
        Ok(Self {
            http,
            cfg,
            namespace: namespace.to_string(),
        })
    }

    fn collection(&self, project: &ProjectId) -> String {
        let prefix = &self.cfg.collection_prefix;
        if self.namespace.is_empty() {
            format!("{prefix}{project}")
        } else {
            format!("{prefix}{}{NAMESPACE_SEPARATOR}{project}", self.namespace)
        }
    }

    fn url(&self, suffix: &str) -> String {
        format!(
            "{}/{}",
            self.cfg.url.trim_end_matches('/'),
            suffix.trim_start_matches('/')
        )
    }

    /// Sends a request and returns the parsed body, or a typed upstream error.
    async fn call(
        &self,
        method: reqwest::Method,
        suffix: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let mut req = self.http.request(method, self.url(suffix));
        if let Some(key) = &self.cfg.api_key {
            req = req.header("api-key", key);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| Error::upstream(SERVICE, e.to_string()))?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(Error::upstream(
                SERVICE,
                format!("HTTP {status}: {}", head(&text, 400)),
            ));
        }
        if text.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&text)
            .map_err(|e| Error::upstream(SERVICE, format!("malformed response: {e}")))
    }
}

#[async_trait::async_trait]
impl VectorStore for QdrantStore {
    fn backend(&self) -> &'static str {
        "qdrant"
    }

    async fn ensure_project(&self, project: &ProjectId, dim: usize) -> Result<()> {
        let name = self.collection(project);
        // A present collection with the wrong width must be reported, not
        // silently written into: the mismatch would only surface at query time.
        if let Ok(existing) = self
            .call(reqwest::Method::GET, &format!("collections/{name}"), None)
            .await
        {
            let size = existing
                .pointer("/result/config/params/vectors/size")
                .and_then(Value::as_u64);
            return match size {
                Some(s) if s as usize == dim => Ok(()),
                // Another embedding space, not a broken store: the same 409 and
                // the same repair the SQLite matrix gives for the same conflict.
                Some(s) => Err(Error::width_mismatch(project, s as usize, dim)),
                None => Ok(()),
            };
        }

        let mut body = json!({
            "vectors": { "size": dim, "distance": "Cosine" }
        });
        if self.cfg.scalar_quantization {
            body["quantization_config"] =
                json!({ "scalar": { "type": "int8", "always_ram": true } });
        }
        self.call(
            reqwest::Method::PUT,
            &format!("collections/{name}"),
            Some(body),
        )
        .await?;
        // Exact-match predicates are only cheap with an index behind them.
        for field in ["lang", "kind"] {
            let _ = self
                .call(
                    reqwest::Method::PUT,
                    &format!("collections/{name}/index?wait=true"),
                    Some(json!({ "field_name": field, "field_schema": "keyword" })),
                )
                .await;
        }
        tracing::info!(collection = %name, dim, "qdrant collection ready");
        Ok(())
    }

    async fn drop_project(&self, project: &ProjectId) -> Result<()> {
        let name = self.collection(project);
        self.call(
            reqwest::Method::DELETE,
            &format!("collections/{name}"),
            None,
        )
        .await?;
        Ok(())
    }

    async fn upsert(&self, project: &ProjectId, records: &[VectorRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }
        let name = self.collection(project);
        let points: Vec<Value> = records
            .iter()
            .map(|r| {
                json!({
                    "id": r.id.as_u64(),
                    "vector": r.vector,
                    "payload": {
                        "chunk_id": r.id.as_str(),
                        "path": r.payload.path,
                        "lang": r.payload.lang.as_str(),
                        "kind": r.payload.kind.as_str(),
                        "symbol": r.payload.symbol,
                    }
                })
            })
            .collect();
        self.call(
            reqwest::Method::PUT,
            &format!("collections/{name}/points?wait=true"),
            Some(json!({ "points": points })),
        )
        .await?;
        Ok(())
    }

    async fn delete(&self, project: &ProjectId, ids: &[ChunkId]) -> Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let name = self.collection(project);
        let numeric: Vec<u64> = ids.iter().map(ChunkId::as_u64).collect();
        self.call(
            reqwest::Method::POST,
            &format!("collections/{name}/points/delete?wait=true"),
            Some(json!({ "points": numeric })),
        )
        .await?;
        Ok(ids.len() as u64)
    }

    async fn search(
        &self,
        project: &ProjectId,
        query: &[f32],
        k: usize,
        filter: &SearchFilter,
    ) -> Result<Vec<ScoredChunk>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let name = self.collection(project);
        let compiled = CompiledFilter::new(filter)?;
        let needs_local = !filter.paths.is_empty() || filter.symbol_contains.is_some();
        let limit = if needs_local {
            (k * LOCAL_FILTER_FANOUT).min(LOCAL_FILTER_CAP)
        } else {
            k
        };

        let mut body = json!({
            "vector": query,
            "limit": limit,
            "with_payload": true,
        });
        if let Some(server_filter) = server_filter(filter) {
            body["filter"] = server_filter;
        }

        let resp = self
            .call(
                reqwest::Method::POST,
                &format!("collections/{name}/points/search"),
                Some(body),
            )
            .await?;
        let results = resp
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut out = Vec::with_capacity(k);
        for item in results {
            let Some(payload) = item.get("payload") else {
                continue;
            };
            let Some(chunk_id) = payload.get("chunk_id").and_then(Value::as_str) else {
                continue;
            };
            if needs_local && !compiled.matches(&read_payload(payload)) {
                continue;
            }
            let score = item.get("score").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            out.push(ScoredChunk {
                id: ChunkId::from_hex(chunk_id),
                score,
            });
            if out.len() == k {
                break;
            }
        }
        Ok(out)
    }

    async fn count(&self, project: &ProjectId) -> Result<u64> {
        let name = self.collection(project);
        let resp = self
            .call(
                reqwest::Method::POST,
                &format!("collections/{name}/points/count"),
                Some(json!({ "exact": true })),
            )
            .await?;
        Ok(resp
            .pointer("/result/count")
            .and_then(Value::as_u64)
            .unwrap_or(0))
    }
}

// ---------------------------------------------------------------------------

/// Builds the server-side half of the predicate: the exact matches only.
fn server_filter(filter: &SearchFilter) -> Option<Value> {
    let mut must = Vec::new();
    if !filter.langs.is_empty() {
        must.push(json!({
            "key": "lang",
            "match": { "any": filter.langs.iter().map(Lang::as_str).collect::<Vec<_>>() }
        }));
    }
    if !filter.kinds.is_empty() {
        must.push(json!({
            "key": "kind",
            "match": { "any": filter.kinds.iter().map(ChunkKind::as_str).collect::<Vec<_>>() }
        }));
    }
    if must.is_empty() {
        None
    } else {
        Some(json!({ "must": must }))
    }
}

fn read_payload(payload: &Value) -> VectorPayload {
    VectorPayload {
        path: payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        lang: payload
            .get("lang")
            .and_then(Value::as_str)
            .and_then(Lang::parse)
            .unwrap_or(Lang::Other),
        kind: payload
            .get("kind")
            .and_then(Value::as_str)
            .and_then(ChunkKind::parse)
            .unwrap_or(ChunkKind::Text),
        symbol: payload
            .get("symbol")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn head(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collection_names_are_prefixed_per_project() {
        let s = QdrantStore::new(QdrantConfig::default(), "").expect("client");
        let p = ProjectId::new("axle").expect("id");
        assert_eq!(s.collection(&p), "findery_axle");
        assert_eq!(
            s.url("/collections/x"),
            "http://127.0.0.1:6333/collections/x"
        );
    }

    /// A member of another recipe gets its own collection; a file with no
    /// namespace keeps the name its collections were created under.
    #[test]
    fn the_namespace_sits_between_the_prefix_and_the_project() {
        let p = ProjectId::new("axle").expect("id");
        let member = QdrantStore::new(QdrantConfig::default(), "zvpmtsdf").expect("client");
        assert_eq!(member.collection(&p), "findery_zvpmtsdf.axle");
        let legacy = QdrantStore::new(QdrantConfig::default(), "").expect("client");
        assert_ne!(member.collection(&p), legacy.collection(&p));
    }

    /// Project ids may contain `_`, so a namespace glued to the id spelled the
    /// name of a legacy collection: `findery_` + `abcdefgh_x` was
    /// `findery_abcdefgh_` + `x`.
    #[test]
    fn no_namespace_and_id_spell_another_pairs_collection() {
        let store = |ns: &str| QdrantStore::new(QdrantConfig::default(), ns).expect("client");
        let id = |raw: &str| ProjectId::new(raw).expect("id");
        let names = [
            store("").collection(&id("abcdefgh_x")),
            store("abcdefgh").collection(&id("x")),
            store("abcdefgh_").collection(&id("x")),
            store("abcdefgh").collection(&id("_x")),
        ];
        let distinct: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "{names:?}");
    }

    #[test]
    fn a_namespace_that_cannot_be_part_of_a_name_is_refused() {
        for bad in ["../x", "A_", "a b", "a/b", &"x".repeat(NAMESPACE_MAX + 1)] {
            assert_eq!(
                check_namespace(bad).map_err(|e| e.code()),
                Err("invalid"),
                "{bad}"
            );
            assert!(QdrantStore::new(QdrantConfig::default(), bad).is_err());
        }
        assert!(check_namespace("").is_ok());
        assert!(check_namespace("47m32eui_").is_ok());
    }

    #[test]
    fn only_exact_predicates_reach_the_server() {
        let with_globs = SearchFilter {
            paths: vec!["src/**".into()],
            ..Default::default()
        };
        assert!(
            server_filter(&with_globs).is_none(),
            "globs must stay local"
        );

        let exact = SearchFilter {
            langs: vec![Lang::Rust],
            kinds: vec![ChunkKind::Function, ChunkKind::Method],
            ..Default::default()
        };
        let f = server_filter(&exact).expect("server filter");
        assert_eq!(f["must"][0]["key"], "lang");
        assert_eq!(f["must"][1]["match"]["any"][1], "method");
    }

    #[test]
    fn payloads_round_trip_through_json() {
        let v = json!({"path": "src/a.rs", "lang": "rust", "kind": "method", "symbol": "go"});
        let p = read_payload(&v);
        assert_eq!(p.path, "src/a.rs");
        assert_eq!(p.lang, Lang::Rust);
        assert_eq!(p.kind, ChunkKind::Method);
        assert_eq!(p.symbol.as_deref(), Some("go"));
    }

    #[test]
    fn an_unknown_payload_degrades_instead_of_failing() {
        let p = read_payload(&json!({}));
        assert_eq!(p.lang, Lang::Other);
        assert_eq!(p.kind, ChunkKind::Text);
        assert!(p.symbol.is_none());
    }
}

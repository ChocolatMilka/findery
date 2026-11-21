//! Engine-level tests: boot, model profiles, leases, lifecycle, degraded
//! search — the behaviours that need a whole `Indexer` over a real data
//! directory. Only the embedder is swapped for the mock (CONV-036).

mod adoption;
mod drain;
mod lifecycle;
mod models_switch;
mod progress;
mod register;
mod search;
mod targeted;
mod throughput;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;

use findery_core::api::RegisterProject;
use findery_core::config::{Config, EmbedderConfig};
use findery_core::embedding::prompt::QueryPrompt;
use findery_core::ids::ProjectId;
use findery_core::model::{SearchMode, SearchRequest, SyncMode, SyncReport};
use findery_core::traits::Embedder;
use findery_core::{Error, Result};

use findery_embed::MockEmbedder;

use crate::models::BuildFn;
use crate::{Indexer, sanitize_rel};

/// A daemon configuration over `data_dir`: the mock at 128 dims as
/// `[embedder]`, plus `extra` (profiles, `default_model`, server knobs).
pub(super) fn config(data_dir: &Path, extra: &str) -> Config {
    let mut cfg = Config::from_toml_str(&format!(
        "{extra}\n[embedder]\nbackend = \"mock\"\ndim = 128\n"
    ))
    .expect("config");
    cfg.store.data_dir = data_dir.display().to_string();
    cfg.store.export_dir = data_dir.join("exports").display().to_string();
    cfg.chunk.min_chars = 1;
    cfg
}

/// The profiles most tests switch between. `other` and `twin` are the same
/// mock weights under two aliases — the same width as `embedder`, so only the
/// recipe tells them apart, never the width.
pub(super) const PROFILES: &str = "[models.other]\nweights = \"other\"\n\
                                   [models.twin]\nweights = \"other\"\n";

/// A small project on disk: two Rust files and a README.
pub(super) fn project_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("project dir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(
        root.join("src/net.rs"),
        "/// Retries an operation with exponential backoff.\n\
         pub fn retry_with_backoff(attempts: u32) -> bool {\n    attempts < 5\n}\n",
    )
    .expect("write");
    std::fs::write(
        root.join("src/pool.rs"),
        "/// Opens a pooled connection.\npub fn connect(url: &str) -> bool {\n    !url.is_empty()\n}\n",
    )
    .expect("write");
    std::fs::write(
        root.join("README.md"),
        "# Demo\n\nRetry policy lives in src/net.rs.\n",
    )
    .expect("write");
    dir
}

/// `dir` as a host path, separators normalized.
pub(super) fn root_of(dir: &Path) -> String {
    dir.display().to_string().replace('\\', "/")
}

pub(super) async fn boot(cfg: &Config) -> Arc<Indexer> {
    Indexer::new(cfg.clone()).await.expect("boot")
}

pub(super) async fn boot_with(cfg: &Config, build: BuildFn) -> Arc<Indexer> {
    Indexer::with_builder(cfg.clone(), build)
        .await
        .expect("boot")
}

/// Registers the project at `root` under `id`, in the bucket inside it.
pub(super) async fn register(idx: &Indexer, root: &Path, id: &str) -> ProjectId {
    registered(idx, root, id, RegisterProject::default()).await
}

/// Registers with the rest of the request taken from `extra`.
pub(super) async fn registered(
    idx: &Indexer,
    root: &Path,
    id: &str,
    extra: RegisterProject,
) -> ProjectId {
    let project = idx
        .register(RegisterProject {
            root: root_of(root),
            id: Some(id.to_string()),
            ..extra
        })
        .await
        .expect("register");
    project.id
}

pub(super) async fn sync(idx: &Indexer, id: &ProjectId) -> SyncReport {
    idx.sync(
        id,
        SyncMode::Incremental,
        &crate::ProgressHandle::detached(),
    )
    .await
    .expect("sync")
}

pub(super) fn query(id: &ProjectId, text: &str, mode: SearchMode) -> SearchRequest {
    let mut req = SearchRequest::new(id.clone(), text);
    req.mode = Some(mode);
    req
}

/// The member files of a family, by scanning its base the way the store does.
pub(super) fn member_files(base: &Path) -> Vec<PathBuf> {
    let family = findery_store::FamilyPath::from_base(base).expect("family");
    findery_store::scan(&family)
        .expect("scan")
        .into_iter()
        .map(|m| m.path)
        .collect()
}

/// The base path of a project-local bucket.
pub(super) fn local_base(root: &Path) -> PathBuf {
    root.join(".findery").join("bucket.db")
}

/// The member file a recipe gets in a project-local bucket.
pub(super) fn local_member(root: &Path, recipe: &str) -> PathBuf {
    let recipe = findery_core::embedding::recipe::Recipe::parse(recipe).expect("recipe");
    findery_core::config::bucket_member_path(&local_base(root), &recipe)
}

/// A file no process holds can be renamed — on Windows, the same test as a
/// deletion, without losing the file.
pub(super) fn is_released(path: &Path) -> bool {
    let moved = path.with_extension("moved");
    let released = std::fs::rename(path, &moved).is_ok();
    if released {
        std::fs::rename(&moved, path).expect("put back");
    }
    released
}

/// What a wrapped mock awaits before each document batch, given its size:
/// the time a real model spends, a lane it books, a counter it reads.
pub(super) type BeforeBatch = Arc<dyn Fn(usize) -> BoxFuture<'static, Result<()>> + Send + Sync>;

/// The mock, with a step of the test's own ahead of every document batch.
struct Wrapped {
    inner: MockEmbedder,
    before: BeforeBatch,
}

#[async_trait::async_trait]
impl Embedder for Wrapped {
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed_documents(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        (self.before)(inputs.len()).await?;
        self.inner.embed_documents(inputs).await
    }

    async fn embed_query(&self, query: &str, how: QueryPrompt<'_>) -> Result<Vec<f32>> {
        self.inner.embed_query(query, how).await
    }

    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

/// A builder whose embedders await `before` ahead of each document batch.
pub(super) fn wrapped_builder(before: BeforeBatch) -> BuildFn {
    Arc::new(move |cfg: EmbedderConfig, _known: Option<usize>| {
        let before = before.clone();
        Box::pin(async move {
            let inner = MockEmbedder::new(cfg.dim);
            let embedder: Arc<dyn Embedder> = Arc::new(Wrapped { inner, before });
            Ok(embedder)
        })
    })
}

/// A builder whose embedders take `delay` per document batch: the time a real
/// model spends, during which a drain has to wait for the sync — or stop it.
pub(super) fn slow_builder(delay: Duration) -> BuildFn {
    wrapped_builder(Arc::new(move |_| {
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Ok(())
        })
    }))
}

fn error_of<T>(outcome: Result<T>) -> Error {
    match outcome {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    }
}

// -- the engine's own smallest promises ---------------------------------------

#[test]
fn traversal_is_refused_and_separators_normalized() {
    assert_eq!(sanitize_rel("src\\lib.rs").expect("ok"), "src/lib.rs");
    assert_eq!(sanitize_rel("/src/lib.rs").expect("ok"), "src/lib.rs");
    assert!(sanitize_rel("../etc/passwd").is_err());
    assert!(sanitize_rel("src/../../x").is_err());
    assert!(sanitize_rel("   ").is_err());
}

#[test]
fn a_dotted_filename_is_not_traversal() {
    assert_eq!(
        sanitize_rel("src/..hidden.rs").expect("ok"),
        "src/..hidden.rs"
    );
}

/// With no `default_model`, the daemon embeds exactly as before profiles
/// existed: `[embedder]`, under the recipe `<model>:<dim>`.
#[tokio::test]
async fn without_a_default_model_the_embedder_section_names_todays_recipe() {
    let dir = tempfile::tempdir().expect("tempdir");
    let idx = boot(&config(dir.path(), "")).await;
    assert_eq!(idx.default_recipe().expect("recipe").as_str(), "mock:128");
}

#[tokio::test]
async fn the_default_model_picks_the_profile_the_daemon_embeds_with() {
    let extra = "[models.small]\ndim = 64\n\
                 [models.wrapped]\ndocument_template = \"search_document: {text}\"\n";
    let dir = tempfile::tempdir().expect("tempdir");
    let small = boot(&config(
        dir.path(),
        &format!("default_model = \"small\"\n{extra}"),
    ))
    .await;
    assert_eq!(small.default_recipe().expect("recipe").as_str(), "mock:64");

    let other = tempfile::tempdir().expect("tempdir");
    let wrapped = boot(&config(
        other.path(),
        &format!("default_model = \"wrapped\"\n{extra}"),
    ))
    .await;
    let recipe = wrapped.default_recipe().expect("recipe");
    assert!(
        recipe.as_str().starts_with("mock|doc="),
        "a document template opens its own space: {recipe}"
    );
}

#[tokio::test]
async fn an_unknown_default_model_refuses_to_boot_and_names_the_known_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = config(
        dir.path(),
        &format!("default_model = \"harier\"\n{PROFILES}"),
    );
    let err = error_of(Indexer::new(cfg).await);
    assert_eq!(err.code(), "model_unknown");
    assert!(err.to_string().contains("embedder, other, twin"), "{err}");
}

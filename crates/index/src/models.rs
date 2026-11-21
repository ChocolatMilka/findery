//! The embedder pool: one embedder per model profile, built when first needed,
//! and the default profile a project with no pin uses.
//!
//! INTENT: the daemon serves several models at once so that switching is a
//! choice made per operation, not a restart. The default profile is built at
//! boot, with today's degraded-boot rule (a runtime that is down is fine as long
//! as the width is known). Every other profile is built on first use, through a
//! `OnceCell` that does **not** cache a failure — a runtime started after the
//! first attempt is picked up by the next one, with no restart.
//!
//! A profile has a vector space before it has an embedder: its recipe is
//! computable from its configured width, or the width a bucket remembers for
//! its identity. That is what lets a read find the right bucket file without
//! dialing a runtime nobody started.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use findery_core::api::{ModelHealth, ModelInfo};
use findery_core::config::{Config, EmbedderConfig, user_config_path};
use findery_core::embedding::recipe::{Recipe, RecipeTag};
use findery_core::ids::ModelAlias;
use findery_core::traits::Embedder;
use findery_core::{Error, Result};

/// What building one embedder returns.
pub type BuildFuture = Pin<Box<dyn Future<Output = Result<Arc<dyn Embedder>>> + Send>>;

/// How an embedder is built from a profile and the width already known for it.
/// [`findery_embed::build`] in production; a test swaps it to observe the pool.
pub type BuildFn = Arc<dyn Fn(EmbedderConfig, Option<usize>) -> BuildFuture + Send + Sync>;

/// The production builder.
pub fn default_builder() -> BuildFn {
    Arc::new(|cfg: EmbedderConfig, known: Option<usize>| {
        Box::pin(async move { findery_embed::build(&cfg, known).await })
    })
}

/// One profile and the embedder built for it, if any.
struct Slot {
    cfg: EmbedderConfig,
    /// The width a bucket built earlier remembers for this profile's identity.
    remembered: Option<usize>,
    built: tokio::sync::OnceCell<Arc<dyn Embedder>>,
    /// Why the last build failed, for `health`. Cleared by a success.
    last_error: Mutex<Option<String>>,
}

impl Slot {
    /// The recipe identity: `weights`, else the name the embedder reports.
    fn identity(&self) -> &str {
        self.cfg.identity(findery_embed::served_model(&self.cfg))
    }

    /// The width, without building anything.
    fn known_dim(&self) -> Option<usize> {
        findery_embed::configured_dim(&self.cfg).or(self.remembered)
    }
}

/// Every profile, and which one is the default.
pub struct EmbedderPool {
    slots: BTreeMap<ModelAlias, Slot>,
    default: RwLock<ModelAlias>,
    build: BuildFn,
}

impl EmbedderPool {
    /// Builds the pool and the default profile's embedder.
    ///
    /// `remembered` maps a recipe identity to the width a bucket on disk holds
    /// it at. A default profile that cannot be built refuses the boot, as it
    /// always did: with no width there is nothing to open a store with.
    pub async fn boot(
        cfg: &Config,
        remembered: &HashMap<String, usize>,
        build: BuildFn,
    ) -> Result<Self> {
        let (default, _) = cfg.default_profile()?;
        let mut slots = BTreeMap::new();
        for (alias, profile) in cfg.profiles()? {
            let identity = profile.identity(findery_embed::served_model(&profile));
            let remembered = remembered.get(identity).copied();
            slots.insert(
                alias,
                Slot {
                    cfg: profile,
                    remembered,
                    built: tokio::sync::OnceCell::new(),
                    last_error: Mutex::new(None),
                },
            );
        }
        let pool = Self {
            slots,
            default: RwLock::new(default.clone()),
            build,
        };
        pool.embedder(&default).await?;
        Ok(pool)
    }

    /// The profile a project with no pin uses right now.
    pub fn default_alias(&self) -> ModelAlias {
        self.default.read().clone()
    }

    /// Makes `alias` the default for every later operation.
    pub fn set_default(&self, alias: &str) -> Result<ModelAlias> {
        let alias = self.known(alias)?;
        *self.default.write() = alias.clone();
        Ok(alias)
    }

    /// The profile an operation on a project uses: its pin, else the default.
    /// A pin no profile defines is refused, naming the ones that exist.
    pub fn resolve(&self, pin: Option<&ModelAlias>) -> Result<ModelAlias> {
        match pin {
            Some(alias) => self.known(alias.as_str()),
            None => Ok(self.default_alias()),
        }
    }

    /// Validates an alias against the profiles this daemon has.
    pub fn known(&self, raw: &str) -> Result<ModelAlias> {
        ModelAlias::new(raw)
            .ok()
            .filter(|alias| self.slots.contains_key(alias))
            .ok_or_else(|| {
                let known: Vec<&str> = self.slots.keys().map(ModelAlias::as_str).collect();
                Error::model_unknown(raw, &known, &user_config_path())
            })
    }

    fn slot(&self, alias: &ModelAlias) -> Result<&Slot> {
        match self.slots.get(alias) {
            Some(slot) => Ok(slot),
            // `known` names the profiles that exist; an alias it accepts and
            // the map lacks is a bug, never caller input.
            None => match self.known(alias.as_str()) {
                Err(unknown) => Err(unknown),
                Ok(_) => Err(Error::Internal(format!("profile `{alias}` has no slot"))),
            },
        }
    }

    /// The embedder of `alias`, built on first use. A failed build is not
    /// remembered: the next call tries again.
    pub async fn embedder(&self, alias: &ModelAlias) -> Result<Arc<dyn Embedder>> {
        let slot = self.slot(alias)?;
        let built = slot
            .built
            .get_or_try_init(|| (self.build)(slot.cfg.clone(), slot.remembered))
            .await;
        match built {
            Ok(embedder) => {
                *slot.last_error.lock() = None;
                Ok(embedder.clone())
            }
            Err(e) => {
                *slot.last_error.lock() = Some(e.to_string());
                Err(e)
            }
        }
    }

    /// How `alias`'s runtime wants its inputs sliced: `(batch_size,
    /// concurrency)`, the sync's unit of embedding progress.
    pub fn batching(&self, alias: &ModelAlias) -> Result<(usize, usize)> {
        let cfg = &self.slot(alias)?.cfg;
        Ok((cfg.batch_size.max(1), cfg.concurrency.max(1)))
    }

    /// The embedder of `alias` when it was already built — never builds.
    pub fn built(&self, alias: &ModelAlias) -> Option<Arc<dyn Embedder>> {
        self.slots.get(alias)?.built.get().cloned()
    }

    /// The vector space of `alias`, without building anything when the width
    /// is known: from the embedder already built, else the configured width,
    /// else the one a bucket remembers.
    pub fn recipe_of(&self, alias: &ModelAlias) -> Result<Recipe> {
        let slot = self.slot(alias)?;
        let doc = slot.cfg.document_template.as_deref();
        if let Some(embedder) = slot.built.get() {
            return Recipe::of(slot.cfg.identity(embedder.model()), embedder.dim(), doc);
        }
        match slot.known_dim() {
            Some(dim) => Recipe::of(slot.identity(), dim, doc),
            None => Err(Error::Config(format!(
                "the vector width of model `{alias}` is unknown until its runtime answers: start \
                 `{}`, or set `dim` under [models.{alias}]",
                slot.cfg.endpoint()
            ))),
        }
    }

    /// The vector space of `alias`, building its embedder when nothing else
    /// can tell the width.
    pub async fn resolve_recipe(&self, alias: &ModelAlias) -> Result<Recipe> {
        match self.recipe_of(alias) {
            Ok(recipe) => Ok(recipe),
            Err(_) => {
                self.embedder(alias).await?;
                self.recipe_of(alias)
            }
        }
    }

    /// The vector space a write under `alias` works in.
    ///
    /// A write is about to embed anyway, so the embedder is built first and
    /// names the recipe from the width it serves. When it cannot be built, the
    /// width known without it still names the recipe — a write whose batches
    /// are all cached needs no runtime — and only when neither can is the
    /// build's own failure the answer.
    pub async fn write_recipe(&self, alias: &ModelAlias) -> Result<Recipe> {
        let built = self.embedder(alias).await;
        match (self.recipe_of(alias), built) {
            (Ok(recipe), _) => Ok(recipe),
            (Err(_), Err(build_failure)) => Err(build_failure),
            (Err(unknown_width), Ok(_)) => Err(unknown_width),
        }
    }

    /// The recipe a profile resolves `tag` to, when one does: what a member's
    /// file name promises, so a file holding something else is caught.
    pub fn recipe_for_tag(&self, tag: &RecipeTag) -> Option<Recipe> {
        self.slots
            .keys()
            .filter_map(|alias| self.recipe_of(alias).ok())
            .find(|recipe| recipe.tag() == *tag)
    }

    /// Every profile, as the daemon resolves it without probing.
    pub fn list(&self) -> Vec<ModelInfo> {
        let default = self.default_alias();
        self.slots
            .iter()
            .map(|(alias, slot)| {
                let recipe = self.recipe_of(alias).ok();
                ModelInfo {
                    alias: alias.clone(),
                    model: slot.cfg.model.clone(),
                    dim: recipe.as_ref().map(Recipe::dim),
                    recipe: recipe.map(|r| r.as_str().to_string()),
                    is_default: *alias == default,
                    connected: slot.built.initialized(),
                }
            })
            .collect()
    }

    /// Every profile with a liveness probe of the ones already built.
    pub async fn health(&self) -> Vec<ModelHealth> {
        let mut out = Vec::with_capacity(self.slots.len());
        for profile in self.list() {
            let (ok, detail) = match self.built(&profile.alias) {
                Some(embedder) => match embedder.health().await {
                    Ok(()) => (true, None),
                    Err(e) => (false, Some(e.to_string())),
                },
                None => {
                    let last = self
                        .slots
                        .get(&profile.alias)
                        .and_then(|slot| slot.last_error.lock().clone());
                    let why = last.map_or_else(
                        || "not used yet: built on the first operation that needs it".to_string(),
                        |e| format!("could not be built: {e}"),
                    );
                    (false, Some(why))
                }
            };
            out.push(ModelHealth {
                profile,
                ok,
                detail,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use findery_embed::MockEmbedder;

    use super::*;

    fn config(extra: &str) -> Config {
        Config::from_toml_str(&format!(
            "{extra}\n[embedder]\nbackend = \"mock\"\ndim = 16\n\
             [models.other]\nweights = \"other\"\n\
             [models.twin]\nweights = \"other\"\n"
        ))
        .expect("config")
    }

    /// A builder that fails `failures` times, then builds a mock — and counts.
    fn flaky(failures: usize) -> (BuildFn, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let build: BuildFn = Arc::new(move |cfg: EmbedderConfig, _known: Option<usize>| {
            let attempt = seen.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt < failures {
                    return Err(Error::upstream("embedder", "connection refused"));
                }
                let embedder: Arc<dyn Embedder> = Arc::new(MockEmbedder::new(cfg.dim));
                Ok(embedder)
            })
        });
        (build, calls)
    }

    /// The default is built at boot, the others only when an operation needs
    /// them.
    #[tokio::test]
    async fn only_the_default_profile_is_built_at_boot() {
        let (build, calls) = flaky(0);
        let pool = EmbedderPool::boot(&config(""), &HashMap::new(), build)
            .await
            .expect("boot");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let other = ModelAlias::new("other").expect("alias");
        assert!(pool.built(&other).is_none());
        assert_eq!(
            pool.recipe_of(&other).expect("known width").as_str(),
            "other:16",
            "a recipe needs no runtime when the width is configured"
        );
        pool.embedder(&other).await.expect("built on use");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        pool.embedder(&other).await.expect("cached");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a success is kept");
    }

    /// A runtime started after the first attempt is picked up by the next one.
    #[tokio::test]
    async fn a_failed_build_is_not_cached_and_the_next_call_retries() {
        let (build, calls) = flaky(0);
        let pool = EmbedderPool::boot(&config(""), &HashMap::new(), build)
            .await
            .expect("boot");
        let (flaky_build, flaky_calls) = flaky(1);
        let pool = EmbedderPool {
            build: flaky_build,
            ..pool
        };
        let other = ModelAlias::new("other").expect("alias");
        let Err(err) = pool.embedder(&other).await else {
            panic!("the first attempt fails");
        };
        assert_eq!(err.code(), "upstream");
        let health = pool.health().await;
        let reported = health
            .iter()
            .find(|m| m.profile.alias == other)
            .expect("listed");
        assert!(!reported.ok && !reported.profile.connected);
        assert!(
            reported
                .detail
                .as_deref()
                .is_some_and(|d| d.contains("connection refused")),
            "{reported:?}"
        );
        pool.embedder(&other).await.expect("second attempt builds");
        assert_eq!(flaky_calls.load(Ordering::SeqCst), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn switching_the_default_is_validated_and_takes_effect_at_once() {
        let (build, _) = flaky(0);
        let pool = EmbedderPool::boot(&config(""), &HashMap::new(), build)
            .await
            .expect("boot");
        assert_eq!(pool.default_alias().as_str(), "embedder");
        pool.set_default("other").expect("known");
        assert_eq!(pool.resolve(None).expect("default").as_str(), "other");
        let err = pool.set_default("harier").expect_err("unknown");
        assert_eq!(err.code(), "model_unknown");
        assert!(err.to_string().contains("embedder, other, twin"), "{err}");
        assert_eq!(pool.default_alias().as_str(), "other", "nothing changed");
    }

    /// Two aliases over the same weights are one vector space.
    #[tokio::test]
    async fn two_aliases_of_one_identity_resolve_to_one_recipe_and_tag() {
        let (build, _) = flaky(0);
        let pool = EmbedderPool::boot(&config(""), &HashMap::new(), build)
            .await
            .expect("boot");
        let other = pool
            .recipe_of(&ModelAlias::new("other").expect("alias"))
            .expect("recipe");
        let twin = pool
            .recipe_of(&ModelAlias::new("twin").expect("alias"))
            .expect("recipe");
        assert_eq!(other, twin);
        assert_eq!(pool.recipe_for_tag(&other.tag()), Some(other));
    }

    /// The width a bucket remembers makes the recipe computable without a
    /// runtime, which is what a read on a profile nobody started needs.
    #[tokio::test]
    async fn a_remembered_width_names_the_recipe_of_an_unbuilt_profile() {
        let cfg = Config::from_toml_str(
            "[embedder]\nbackend = \"mock\"\ndim = 16\n\
             [models.remote]\nbackend = \"openai_compat\"\nmodel = \"far\"\ndim = 0\n",
        )
        .expect("config");
        let (build, _) = flaky(0);
        let remembered = HashMap::from([("far".to_string(), 1024)]);
        let pool = EmbedderPool::boot(&cfg, &remembered, build)
            .await
            .expect("boot");
        let remote = ModelAlias::new("remote").expect("alias");
        assert_eq!(
            pool.recipe_of(&remote).expect("remembered").as_str(),
            "far:1024"
        );

        let (build, _) = flaky(0);
        let forgetful = EmbedderPool::boot(&cfg, &HashMap::new(), build)
            .await
            .expect("boot");
        assert_eq!(
            forgetful.recipe_of(&remote).map_err(|e| e.code()),
            Err("config"),
            "no width anywhere"
        );
    }
}

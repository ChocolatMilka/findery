//! The intent library each project's searches resolve against: the built-in
//! one, the daemon's `[intents.*]` over it, the project's manifest layer on top.
//!
//! INTENT: layering re-validates the whole library, which is too much work for
//! every search. A project's library is kept with the overrides it was built
//! from and reused only while the spec still carries the same ones — so a
//! re-registration that changed them can never be answered from a stale copy.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use parking_lot::Mutex;

use findery_core::Result;
use findery_core::api::IntentInfo;
use findery_core::config::Config;
use findery_core::ids::ProjectId;
use findery_core::intent::{IntentLibrary, IntentOverride};
use findery_core::model::Project;

use crate::Indexer;
use crate::gates::Access;

type Overrides = BTreeMap<String, IntentOverride>;

/// One project's layered library, and the layer it was built from.
struct Layered {
    overrides: Overrides,
    library: Arc<IntentLibrary>,
}

/// The daemon's library, and each project's layer over it.
pub(crate) struct IntentCache {
    daemon: Arc<IntentLibrary>,
    projects: Mutex<HashMap<ProjectId, Layered>>,
}

impl IntentCache {
    /// The built-in library with the daemon's layer, validated once at boot.
    pub(crate) fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            daemon: Arc::new(cfg.intent_library()?),
            projects: Mutex::new(HashMap::new()),
        })
    }

    /// Refuses a manifest layer before anything is written (CONV-023).
    pub(crate) fn validate(&self, overrides: &Overrides) -> Result<()> {
        self.daemon.merged(overrides).map(|_| ())
    }

    /// The library `project`'s searches resolve against.
    pub(crate) fn for_project(&self, project: &Project) -> Result<Arc<IntentLibrary>> {
        let overrides = &project.spec.intents;
        if overrides.is_empty() {
            return Ok(self.daemon.clone());
        }
        // Held while building, so two searches never build the same layer twice.
        let mut projects = self.projects.lock();
        let fresh = projects
            .get(&project.id)
            .filter(|cached| cached.overrides == *overrides);
        if let Some(cached) = fresh {
            return Ok(cached.library.clone());
        }
        let library = Arc::new(self.daemon.merged(overrides)?);
        let entry = Layered {
            overrides: overrides.clone(),
            library: library.clone(),
        };
        projects.insert(project.id.clone(), entry);
        Ok(library)
    }

    /// Drops what a registration may have changed.
    pub(crate) fn forget(&self, id: &ProjectId) {
        self.projects.lock().remove(id);
    }
}

impl Indexer {
    /// The intents a search can name: the daemon's library, or — for a
    /// project — that library with the project's manifest layer on top.
    pub async fn intents(&self, project: Option<&ProjectId>) -> Result<Vec<IntentInfo>> {
        let library = match project {
            Some(id) => {
                let lease = self.lease(id, Access::Read).await?;
                self.intent_cache.for_project(&lease.project)?
            }
            None => self.intent_cache.daemon.clone(),
        };
        Ok(library.iter().map(IntentInfo::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use findery_core::model::ProjectSpec;

    use super::*;

    fn project(layer: &str) -> Project {
        Project {
            id: ProjectId::new("demo").expect("id"),
            name: "demo".into(),
            bucket: findery_core::model::DEFAULT_BUCKET.into(),
            model: None,
            spec: ProjectSpec {
                intents: toml::from_str(layer).expect("a layer"),
                ..ProjectSpec::default()
            },
            host_root: String::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    const IMPORTERS: &str = "[usage]\ninstruction = \"Given an API, retrieve its importers.\"\n";
    const SPECS: &str = "[usage]\ninstruction = \"Given an API, retrieve its specs.\"\n";

    fn usage_sentence(library: &IntentLibrary) -> String {
        let usage = library.get("usage").expect("shipped");
        usage.neutral_instruction().to_string()
    }

    #[test]
    fn a_project_layer_is_reused_until_the_spec_carries_another() {
        let cache = IntentCache::new(&Config::default()).expect("cache");
        let first = cache.for_project(&project(IMPORTERS)).expect("layered");
        let again = cache.for_project(&project(IMPORTERS)).expect("layered");
        assert!(Arc::ptr_eq(&first, &again), "the same layer is not rebuilt");

        let changed = cache.for_project(&project(SPECS)).expect("layered");
        assert_eq!(
            usage_sentence(&changed),
            "Given an API, retrieve its specs."
        );

        let plain = cache.for_project(&project("")).expect("daemon");
        assert!(Arc::ptr_eq(&plain, &cache.daemon));
    }

    #[test]
    fn a_broken_manifest_layer_is_refused() {
        let cache = IntentCache::new(&Config::default()).expect("cache");
        let layer: Overrides = toml::from_str("[usage.hints]\ncobol = [\"x\"]\n").expect("parses");
        assert_eq!(cache.validate(&layer).expect_err("bad").code(), "config");
    }
}

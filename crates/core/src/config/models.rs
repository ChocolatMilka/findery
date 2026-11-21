//! Model profiles: `[embedder]` as the implicit profile `embedder`, and every
//! `[models.<alias>]` merged over it.
//!
//! ```toml
//! default_model = "harrier"          # or FINDERY_MODEL; absent = `embedder`
//!
//! [embedder]                         # the implicit profile, as it always was
//! base_url = "http://localhost:1234"
//! model = "text-embedding-qwen3-embedding-0.6b"
//!
//! [models.harrier]                   # only what differs
//! model = "harrier-oss-v1-0.6b"
//! ```
//!
//! INTENT: a profile is written as the difference from `[embedder]`, because
//! most profiles of one machine share a runtime, a batch size and a timeout,
//! and a copy of eighteen keys is eighteen keys that drift. Two families of
//! keys do NOT travel, because they belong to something other than the
//! runtime's tuning:
//!
//! - **secrets travel with the server.** `api_key`, `extra_headers`,
//!   `auth_header` and `auth_prefix` are dropped as soon as a profile names a
//!   different `base_url` — a DashScope key must never be sent to LM Studio,
//!   or to whatever else listens on the profile's address;
//! - **facts about the weights travel with the model.** `weights`,
//!   `query_template` and `document_template` are dropped as soon as a profile
//!   names a different `model` — inheriting another model's `weights` would
//!   make two models share one vector space, which is a poisoned cache.
//!
//! A profile is re-read through `EmbedderConfig`'s own `deny_unknown_fields`,
//! so a typo under `[models.x]` is refused exactly like one under `[embedder]`.

use std::collections::BTreeMap;

use super::sections::EmbedderConfig;
use super::{Config, user_config_path};
use crate::embedding::prompt::{DocumentTemplate, QueryTemplate};
use crate::ids::ModelAlias;
use crate::{Error, Result};

/// Keys that belong to the server a profile talks to.
const SERVER_BOUND: &[&str] = &["api_key", "extra_headers", "auth_header", "auth_prefix"];

/// Keys that belong to the weights a profile serves.
const WEIGHT_BOUND: &[&str] = &["weights", "query_template", "document_template"];

impl Config {
    /// Every profile: `[embedder]` as `embedder`, then each `[models.<alias>]`
    /// merged over it.
    pub fn profiles(&self) -> Result<BTreeMap<ModelAlias, EmbedderConfig>> {
        let mut out = BTreeMap::new();
        out.insert(ModelAlias::implicit(), self.embedder.clone());
        for (name, overlay) in &self.models {
            let alias = ModelAlias::for_profile(name.as_str())
                .map_err(|e| Error::Config(format!("[models.{name}]: {}", e.detail())))?;
            let profile = merge(&self.embedder, &alias, overlay)?;
            out.insert(alias, profile);
        }
        Ok(out)
    }

    /// The profile named by `alias`; `embedder` is `[embedder]` itself.
    pub fn profile(&self, alias: &str) -> Result<(ModelAlias, EmbedderConfig)> {
        let wanted = ModelAlias::new(alias).map_err(|_| self.unknown_model(alias))?;
        if wanted.is_implicit() {
            return Ok((wanted, self.embedder.clone()));
        }
        let overlay = self
            .models
            .get(wanted.as_str())
            .ok_or_else(|| self.unknown_model(alias))?;
        let profile = merge(&self.embedder, &wanted, overlay)?;
        Ok((wanted, profile))
    }

    /// The profile the daemon embeds with: `default_model` when it is set,
    /// `[embedder]` otherwise.
    pub fn default_profile(&self) -> Result<(ModelAlias, EmbedderConfig)> {
        match self.default_model.as_deref().map(str::trim) {
            None | Some("") => Ok((ModelAlias::implicit(), self.embedder.clone())),
            Some(alias) => self.profile(alias),
        }
    }

    /// Every profile validated, and the default one resolvable.
    pub(super) fn validate_profiles(&self) -> Result<()> {
        for (alias, profile) in self.profiles()? {
            let section = if alias.is_implicit() {
                "embedder".to_string()
            } else {
                format!("models.{alias}")
            };
            profile.validate(&section)?;
        }
        self.default_profile().map(|_| ())
    }

    fn unknown_model(&self, alias: &str) -> Error {
        let mut known = vec![ModelAlias::IMPLICIT];
        known.extend(self.models.keys().map(String::as_str));
        Error::model_unknown(alias, &known, &user_config_path())
    }
}

impl EmbedderConfig {
    /// The recipe identity: `weights` when the profile names them, else the
    /// model name the runtime serves.
    ///
    /// `served` is what the embedder reports, which for the mock backend is
    /// not `model` — it is the embedder's word that has always built the recipe.
    pub fn identity<'a>(&'a self, served: &'a str) -> &'a str {
        match self.weights.as_deref().map(str::trim) {
            Some(weights) if !weights.is_empty() => weights,
            _ => served,
        }
    }

    /// The parsed query template.
    pub fn query_template(&self) -> Result<QueryTemplate> {
        match &self.query_template {
            Some(raw) => QueryTemplate::parse(raw),
            None => Ok(QueryTemplate::default()),
        }
    }

    /// The parsed document template.
    pub fn document_template(&self) -> Result<DocumentTemplate> {
        match &self.document_template {
            Some(raw) => DocumentTemplate::parse(raw),
            None => Ok(DocumentTemplate::default()),
        }
    }

    /// Rejects a profile that would fail later; `section` names it in the
    /// message (`embedder`, `models.harrier`).
    fn validate(&self, section: &str) -> Result<()> {
        let bad = |why: String| Error::Config(format!("{section}.{why}"));
        if self.base_url.trim().is_empty() {
            return Err(bad("base_url must not be empty".into()));
        }
        if self.model.trim().is_empty() {
            return Err(bad("model must not be empty".into()));
        }
        if self.batch_size == 0 {
            return Err(bad("batch_size must be > 0".into()));
        }
        if self.concurrency == 0 {
            return Err(bad("concurrency must be > 0".into()));
        }
        if self.probe_timeout_secs == 0 {
            return Err(bad("probe_timeout_secs must be > 0".into()));
        }
        if let Some(weights) = &self.weights
            && (weights.trim().is_empty() || weights.contains('|'))
        {
            return Err(bad(format!(
                "weights `{weights}` must be a non-empty name without `|`"
            )));
        }
        self.query_template()
            .map_err(|e| bad(format!("query_template: {}", e.detail())))?;
        self.document_template()
            .map_err(|e| bad(format!("document_template: {}", e.detail())))?;
        Ok(())
    }
}

/// `overlay` over `base`, re-read through `EmbedderConfig`'s strict schema.
fn merge(
    base: &EmbedderConfig,
    alias: &ModelAlias,
    overlay: &toml::Table,
) -> Result<EmbedderConfig> {
    let at = |e: &dyn std::fmt::Display| Error::Config(format!("[models.{alias}]: {e}"));
    let mut table = match toml::Value::try_from(base).map_err(|e| at(&e))? {
        toml::Value::Table(table) => table,
        _ => return Err(at(&"the [embedder] section is not a table")),
    };
    let differs = |key: &str, normalize: fn(&str) -> &str| match overlay.get(key) {
        None => false,
        Some(theirs) => match (theirs.as_str(), table.get(key).and_then(|v| v.as_str())) {
            (Some(theirs), Some(ours)) => normalize(theirs) != normalize(ours),
            _ => true,
        },
    };
    let other_server = differs("base_url", |url| url.trim().trim_end_matches('/'));
    let other_model = differs("model", str::trim);
    if other_server {
        for key in SERVER_BOUND {
            table.remove(*key);
        }
    }
    if other_model {
        for key in WEIGHT_BOUND {
            table.remove(*key);
        }
    }
    for (key, value) in overlay {
        table.insert(key.clone(), value.clone());
    }
    toml::Value::Table(table)
        .try_into::<EmbedderConfig>()
        .map_err(|e| at(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(src: &str) -> Config {
        Config::from_toml_str(src).expect("parses")
    }

    const LOCAL: &str = r#"
[embedder]
base_url = "http://localhost:1234"
model = "text-embedding-qwen3-embedding-0.6b"
batch_size = 7
api_key = "local-key"

[models.harrier]
model = "harrier-oss-v1-0.6b"
"#;

    #[test]
    fn a_profile_inherits_what_it_does_not_say_from_the_embedder_section() {
        let cfg = config(LOCAL);
        cfg.validate().expect("valid");
        let profiles = cfg.profiles().expect("profiles");
        assert_eq!(profiles.len(), 2);
        let harrier = &profiles[&ModelAlias::new("harrier").expect("alias")];
        assert_eq!(harrier.model, "harrier-oss-v1-0.6b");
        assert_eq!(harrier.base_url, "http://localhost:1234");
        assert_eq!(harrier.batch_size, 7);
        assert_eq!(
            harrier.api_key.as_deref(),
            Some("local-key"),
            "same server, same secret"
        );
        let implicit = &profiles[&ModelAlias::implicit()];
        assert_eq!(implicit.model, "text-embedding-qwen3-embedding-0.6b");
    }

    #[test]
    fn a_secret_never_follows_a_profile_to_another_server() {
        let cfg = config(
            r#"
[embedder]
api_key = "sk-dashscope"
auth_header = "api-key"
auth_prefix = ""
[embedder.extra_headers]
"X-Tenant" = "acme"

[models.local]
base_url = "http://localhost:1234"
model = "harrier-oss-v1-0.6b"

[models.trailing-slash]
base_url = "https://dashscope-intl.aliyuncs.com/compatible-mode/"
"#,
        );
        let (_, local) = cfg.profile("local").expect("profile");
        assert_eq!(local.api_key, None);
        assert!(local.extra_headers.is_empty());
        assert_eq!(local.auth_header, "Authorization");
        assert_eq!(local.auth_prefix, "Bearer ");

        let (_, same) = cfg.profile("trailing-slash").expect("profile");
        assert_eq!(same.api_key.as_deref(), Some("sk-dashscope"));
        assert_eq!(same.auth_header, "api-key");
    }

    #[test]
    fn weights_never_follow_a_profile_to_another_model() {
        let cfg = config(
            r#"
[embedder]
model = "text-embedding-qwen3-embedding-0.6b"
weights = "Qwen/Qwen3-Embedding-0.6B"
document_template = "doc: {text}"

[models.harrier]
model = "harrier-oss-v1-0.6b"

[models.faster]
batch_size = 64
"#,
        );
        let (_, harrier) = cfg.profile("harrier").expect("profile");
        assert_eq!(harrier.weights, None);
        assert_eq!(harrier.document_template, None);
        let (_, faster) = cfg.profile("faster").expect("profile");
        assert_eq!(faster.weights.as_deref(), Some("Qwen/Qwen3-Embedding-0.6B"));
        assert_eq!(faster.document_template.as_deref(), Some("doc: {text}"));
    }

    #[test]
    fn a_typo_under_a_profile_is_refused() {
        let cfg = config("[models.harrier]\nmodle = \"harrier-oss-v1-0.6b\"\n");
        let err = cfg.validate().expect_err("typo");
        assert_eq!(err.code(), "config");
        assert!(err.to_string().contains("models.harrier"), "{err}");
        assert!(err.to_string().contains("modle"), "{err}");
    }

    #[test]
    fn a_profile_cannot_take_the_reserved_or_a_hostile_name() {
        for name in ["embedder", "\"../x\""] {
            let cfg = config(&format!("[models.{name}]\nmodel = \"m\"\n"));
            let err = cfg.validate().expect_err(name);
            assert_eq!(err.code(), "config", "{name}");
        }
    }

    #[test]
    fn the_default_model_selects_the_profile() {
        let mut cfg = config(LOCAL);
        let (alias, profile) = cfg.default_profile().expect("implicit");
        assert!(alias.is_implicit());
        assert_eq!(profile.model, cfg.embedder.model);

        cfg.default_model = Some("harrier".into());
        let (alias, profile) = cfg.default_profile().expect("harrier");
        assert_eq!(alias.as_str(), "harrier");
        assert_eq!(profile.model, "harrier-oss-v1-0.6b");

        cfg.default_model = Some("embedder".into());
        assert!(cfg.default_profile().expect("implicit").0.is_implicit());
        cfg.default_model = Some("  ".into());
        assert!(
            cfg.default_profile()
                .expect("blank is unset")
                .0
                .is_implicit()
        );
    }

    #[test]
    fn an_unknown_default_model_names_the_known_ones() {
        let mut cfg = config(LOCAL);
        cfg.default_model = Some("harier".into());
        let err = cfg.validate().expect_err("unknown");
        assert_eq!(err.code(), "model_unknown");
        let text = err.to_string();
        assert!(text.contains("harier"), "{text}");
        assert!(text.contains("embedder, harrier"), "{text}");
        assert!(text.contains("[models.harier]"), "{text}");
    }

    #[test]
    fn a_template_without_its_slot_is_refused_in_any_profile() {
        let doc = config("[embedder]\ndocument_template = \"search_document:\"\n");
        let err = doc.validate().expect_err("no {text}");
        assert!(
            err.to_string().contains("embedder.document_template"),
            "{err}"
        );

        let query = config("[models.nomic]\nmodel = \"n\"\nquery_template = \"search_query:\"\n");
        let err = query.validate().expect_err("no {query}");
        assert!(
            err.to_string().contains("models.nomic.query_template"),
            "{err}"
        );

        let fine = config(
            "[models.nomic]\nmodel = \"n\"\nquery_template = \"search_query: {query}\"\n\
             document_template = \"search_document: {text}\"\n",
        );
        fine.validate().expect("both slots present");
    }

    #[test]
    fn weights_name_the_identity_and_are_refused_when_ambiguous() {
        let mut cfg = EmbedderConfig::default();
        assert_eq!(cfg.identity("served"), "served");
        cfg.weights = Some("microsoft/harrier-oss-v1-0.6b".into());
        assert_eq!(cfg.identity("served"), "microsoft/harrier-oss-v1-0.6b");
        cfg.weights = Some("  ".into());
        assert_eq!(cfg.identity("served"), "served");
        assert!(
            cfg.validate("embedder").is_err(),
            "blank weights are a typo"
        );
        cfg.weights = Some("a|b".into());
        assert!(cfg.validate("embedder").is_err());
    }
}

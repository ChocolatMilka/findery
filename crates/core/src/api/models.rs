//! The vocabulary of model profiles as a client observes them: which ones the
//! daemon knows, which one it embeds with by default, and whether each answers.
//!
//! Split from the rest of the wire vocabulary because a profile has two views —
//! the cheap listing and the probed one `/v1/health` carries — and the second is
//! the first plus a probe, which reads best side by side.

use serde::{Deserialize, Serialize};

use crate::ids::ModelAlias;

/// One model profile, as the daemon resolves it without probing anything.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// `embedder` for the `[embedder]` section, else the `[models.<alias>]` key.
    pub alias: ModelAlias,
    /// The model name the runtime is asked for.
    pub model: String,
    /// The vector width, when it is known: served by a connected embedder,
    /// configured, or remembered from a bucket built earlier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dim: Option<usize>,
    /// The vector space this profile writes into, once its width is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// True for the profile a project with no pin uses.
    pub is_default: bool,
    /// True once an embedder was built for this profile. Profiles other than
    /// the default are built on first use, so `false` is not a failure.
    pub connected: bool,
}

/// One model profile with the outcome of a liveness probe, as `/v1/health`
/// reports it. On the wire its fields sit flat beside [`ModelInfo`]'s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelHealth {
    #[serde(flatten)]
    pub profile: ModelInfo,
    /// True when the embedder answered the probe. A profile never built is not
    /// probed — health does not dial a runtime nobody asked for — so it reads
    /// `false` with a `detail` saying why.
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `POST /v1/models/default` — switch the profile unpinned projects use.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDefaultModel {
    pub alias: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ModelInfo {
        ModelInfo {
            alias: ModelAlias::new("harrier").expect("alias"),
            model: "harrier-oss-v1-0.6b".into(),
            dim: Some(1024),
            recipe: Some("harrier-oss-v1-0.6b:1024".into()),
            is_default: true,
            connected: false,
        }
    }

    /// The probed view is the listing plus a verdict, flat on the wire.
    #[test]
    fn a_model_health_is_flat_on_the_wire_and_reads_back() {
        let health = ModelHealth {
            profile: info(),
            ok: false,
            detail: Some("not used yet".into()),
        };
        let value = serde_json::to_value(&health).expect("serialize");
        assert_eq!(value["alias"], "harrier");
        assert_eq!(value["recipe"], "harrier-oss-v1-0.6b:1024");
        assert_eq!(value["ok"], false);
        let back: ModelHealth = serde_json::from_value(value).expect("deserialize");
        assert_eq!(back, health);
    }

    #[test]
    fn an_alias_that_could_be_a_path_does_not_deserialize() {
        let mut value = serde_json::to_value(info()).expect("serialize");
        value["alias"] = "../x".into();
        assert!(serde_json::from_value::<ModelInfo>(value).is_err());
    }
}

//! The vocabulary of buckets as a client observes them: a bucket, the files it
//! holds (one per model), and what a removal swept away.
//!
//! INTENT: split from the rest of the wire vocabulary because a bucket has two
//! levels — the family a manifest names, and its member files — and a reader
//! looking for one needs the other beside it.

use serde::{Deserialize, Serialize};

/// One bucket, as `GET /v1/buckets` reports it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    /// The bucket's base path — what manifests and `buckets.toml` name. The
    /// files themselves are its [`members`](Self::members).
    pub path: String,
    /// The recipe of the default model's file; `None` while it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
    /// Why the default model's file cannot serve a vector query, when it
    /// cannot.
    ///
    /// INTENT: a recipe mismatch is reported here rather than raised at boot,
    /// so the daemon that owns the bucket stays up and can still delete it —
    /// a conflict you can read about and repair, not a conflict that locks the
    /// door you would repair it through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
    /// Projects with a row in any member.
    pub projects: u64,
    /// Chunks in the default model's file.
    pub chunks: u64,
    /// Every member, journals included.
    pub disk_bytes: u64,
    /// True when a project registering without naming one lands here.
    pub is_default: bool,
    /// One entry per file, one file per model. Absent from a daemon that
    /// predates per-model files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<BucketMember>,
}

/// One file of a bucket: the vectors of one recipe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketMember {
    /// The database file, with `/` separators.
    pub path: String,
    /// The recipe tag the file is found under: 8 characters of `a-z2-7`.
    pub tag: String,
    /// The vector space the file is expected to hold.
    pub recipe: String,
    /// True for the bucket's untagged base file, kept in place because it
    /// could not be renamed yet. It serves its recipe all the same.
    #[serde(default)]
    pub legacy: bool,
    /// True for the file of the default model: what a project without a pin
    /// reads and writes right now.
    #[serde(default)]
    pub serving: bool,
    pub projects: u64,
    pub chunks: u64,
    /// The file and its journals.
    pub disk_bytes: u64,
    /// Why this file cannot serve a vector query, with the repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
}

/// `POST /v1/buckets` — create a bucket.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CreateBucket {
    pub name: String,
}

/// `DELETE /v1/buckets/{name}` — what the removal swept away.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BucketRemoved {
    pub removed: bool,
    /// Projects that were registered in it, and went with it.
    pub projects_removed: u64,
    /// Their ids, so a caller holding per-project state — a watch, a job — can
    /// let go of it.
    #[serde(default)]
    pub projects: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listing() -> serde_json::Value {
        serde_json::json!({
            "name": "axle", "path": "/p/.findery/bucket.db", "projects": 1,
            "chunks": 10, "disk_bytes": 4096, "is_default": false
        })
    }

    /// A daemon that predates members answers without them; that is a bucket
    /// listed at the family level, not a decoding failure.
    #[test]
    fn a_listing_without_members_still_decodes() {
        let info: BucketInfo = serde_json::from_value(listing()).expect("decode");
        assert!(info.members.is_empty());
        let back = serde_json::to_value(&info).expect("encode");
        assert!(back.get("members").is_none(), "{back}");
    }

    #[test]
    fn a_member_round_trips_and_a_missing_field_is_refused() {
        let member = BucketMember {
            path: "/p/.findery/bucket.harrier-oss-v1-0-6b-zvpmtsdf.db".into(),
            tag: "zvpmtsdf".into(),
            recipe: "harrier-oss-v1-0.6b:1024".into(),
            legacy: false,
            serving: true,
            projects: 1,
            chunks: 10,
            disk_bytes: 4096,
            conflict: None,
        };
        let mut value = listing();
        value["members"] = serde_json::json!([member]);
        let info: BucketInfo = serde_json::from_value(value.clone()).expect("decode");
        assert_eq!(info.members, vec![member]);

        value["members"][0]
            .as_object_mut()
            .expect("object")
            .remove("recipe");
        assert!(serde_json::from_value::<BucketInfo>(value).is_err());
    }
}

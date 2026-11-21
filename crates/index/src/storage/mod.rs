//! Which file an operation works in: the bucket families on disk, their
//! member files (one per model), how boot finds and opens them, and the
//! registry work moved off the async workers.
//!
//! INTENT: a bucket is its files. Everything that names, opens, closes or
//! counts them is here, so the rest of the engine asks for a member and never
//! spells a path.

pub(crate) mod blocking;
pub(crate) mod boot;
pub mod buckets;
pub mod families;
pub(crate) mod member;

//! Which files make up a bucket on disk, and how they move: the file set
//! SQLite keeps beside a database, the family/member layout, the registry of
//! buckets that live elsewhere, and the migrations of older layouts.

pub mod adopt;
pub(crate) mod journal;
pub mod layout;
pub(crate) mod migrate;
pub mod registry;

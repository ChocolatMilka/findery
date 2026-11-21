//! Where vectors are written and searched: beside the bucket, or in Qdrant.
//! Both are rebuildable from the bucket (CONV-026).

pub mod qdrant;
pub mod sqlite;

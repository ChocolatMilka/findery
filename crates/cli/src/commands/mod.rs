//! One module per command, or per family of commands that share a noun.
//!
//! Each takes the [`findery_client::Client`] and the parsed arguments, makes
//! its calls, and renders through `ui`. None decides anything the server
//! decides (CONV-002): a command asks, and says what came back.

pub(crate) mod buckets;
pub(crate) mod config;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod lifecycle;
pub(crate) mod models;
pub(crate) mod projects;
pub(crate) mod search;
pub(crate) mod server;
pub(crate) mod sync;

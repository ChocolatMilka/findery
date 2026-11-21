//! `[sync]`: how a pass slices its work.
//!
//! INTENT: split from `sections.rs` (CONV-029). These bound the memory a first
//! index of a large tree holds at once, and how far reading may run ahead of
//! embedding — the two levers behind a sync's peak footprint.

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Files buffered before a flush embeds and writes them.
    pub flush_files: usize,
    /// Chunks buffered before a flush, whichever limit trips first.
    pub flush_chunks: usize,
    /// How far the scanner may read ahead of the writer, in flushes.
    pub scan_ahead_flushes: usize,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            flush_files: 96,
            flush_chunks: 512,
            scan_ahead_flushes: 2,
        }
    }
}

impl SyncConfig {
    /// How many scanned files may wait for the writer.
    pub fn scan_ahead_files(&self) -> usize {
        self.flush_files
            .saturating_mul(self.scan_ahead_flushes)
            .max(1)
    }

    /// A flush needs room for at least one file and one chunk.
    pub(crate) fn validate(&self) -> Result<()> {
        if self.flush_files == 0 || self.flush_chunks == 0 || self.scan_ahead_flushes == 0 {
            return Err(Error::Config(
                "sync.flush_files, sync.flush_chunks and sync.scan_ahead_flushes must be > 0"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_slicing_is_valid_and_reads_two_flushes_ahead() {
        let cfg = SyncConfig::default();
        cfg.validate().expect("defaults validate");
        assert_eq!(cfg.scan_ahead_files(), 192);
    }

    #[test]
    fn an_empty_flush_is_refused() {
        let cfg = SyncConfig {
            flush_files: 0,
            ..SyncConfig::default()
        };
        assert_eq!(cfg.validate().map_err(|e| e.code()), Err("config"));
    }
}

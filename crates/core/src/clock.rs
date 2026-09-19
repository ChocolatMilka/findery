//! Wall clock, in one place.
//!
//! INTENT: timestamps are stored as seconds since the epoch everywhere — in
//! SQLite, in the API, in reports. One helper means no module invents its own
//! unit, and tests have a single seam to freeze.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch. Returns 0 if the host clock predates 1970,
/// which is a broken machine, not a case worth propagating as an error.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Milliseconds since the epoch, for job timings.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_is_past_2020() {
        assert!(now_unix() > 1_577_836_800);
        assert!(now_millis() > 1_577_836_800_000);
    }
}

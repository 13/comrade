//! Upload-pause tracking. Stub; filled in by Task 5.

use std::time::Instant;

pub struct PauseTracker {}

impl PauseTracker {
    pub fn new() -> Self {
        Self {}
    }

    pub fn allows_auto_connect(&self, _now: Instant) -> bool {
        true
    }

    pub fn deadline(&self) -> Option<Instant> {
        None
    }
}

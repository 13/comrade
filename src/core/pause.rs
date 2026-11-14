//! Upload-pause tracking: which device nodes a foreign process holds, and
//! whether global auto-connect is currently allowed (spec §6.4).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::event::Holder;

/// Quiet period after the last foreign holder releases before auto-connect resumes.
pub const QUIET: Duration = Duration::from_millis(500);
/// A holder older than this no longer blocks auto-connect for other boards.
pub const SAFETY: Duration = Duration::from_secs(60);

pub struct PauseTracker {
    /// Current foreign holders per device node, with when they were first seen.
    holders: HashMap<PathBuf, (Holder, Instant)>,
    /// When the last holder released (only meaningful while `holders` is empty).
    last_release: Option<Instant>,
    /// Deadlines at or before this instant have been handled by `on_timer`.
    retired: Option<Instant>,
}

impl PauseTracker {
    pub fn new() -> Self {
        Self {
            holders: HashMap::new(),
            last_release: None,
            retired: None,
        }
    }

    /// Record a foreign holder. A repeat for the same node updates the label but
    /// keeps the original start time, so the safety net is not postponed.
    pub fn foreign_open(&mut self, devnode: &Path, holder: Holder, now: Instant) {
        self.holders
            .entry(devnode.to_path_buf())
            .and_modify(|e| e.0 = holder.clone())
            .or_insert((holder, now));
    }

    /// Forget the holder of `devnode`. The quiet period starts when the last
    /// holder goes; a release for a node nobody held changes nothing.
    pub fn foreign_released(&mut self, devnode: &Path, now: Instant) {
        if self.holders.remove(devnode).is_some() && self.holders.is_empty() {
            self.last_release = Some(now);
        }
    }

    /// Drop the holder of `devnode` without a quiet period: the user
    /// resumed over it (`p`), so it no longer pauses or blocks anything.
    pub fn forget(&mut self, devnode: &Path) {
        self.holders.remove(devnode);
    }

    pub fn is_held(&self, devnode: &Path) -> bool {
        self.holders.contains_key(devnode)
    }

    /// Mark every deadline at or before `now` as handled, so `deadline()` never
    /// returns an instant that has already passed.
    pub fn on_timer(&mut self, now: Instant) {
        self.retired = Some(self.retired.map_or(now, |r| r.max(now)));
    }

    pub fn allows_auto_connect(&self, now: Instant) -> bool {
        match self.oldest_since() {
            Some(since) => now >= since + SAFETY,
            None => self.last_release.is_none_or(|r| now >= r + QUIET),
        }
    }

    /// The next instant at which `allows_auto_connect` may flip to true.
    pub fn deadline(&self) -> Option<Instant> {
        let d = match self.oldest_since() {
            Some(since) => since + SAFETY,
            None => self.last_release? + QUIET,
        };
        match self.retired {
            Some(r) if d <= r => None,
            _ => Some(d),
        }
    }

    fn oldest_since(&self) -> Option<Instant> {
        self.holders.values().map(|(_, since)| *since).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn esptool() -> Holder {
        Holder::Named {
            pid: 4121,
            comm: "esptool".into(),
        }
    }

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn idle_tracker_allows_and_has_no_deadline() {
        let p = PauseTracker::new();
        assert!(p.allows_auto_connect(Instant::now()));
        assert_eq!(p.deadline(), None);
    }

    #[test]
    fn holder_blocks_until_release_plus_quiet() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        let n = Path::new("/dev/ttyACM0");
        p.foreign_open(n, esptool(), t0);
        assert!(p.is_held(n));
        assert!(!p.allows_auto_connect(ms(t0, 10)));
        assert_eq!(p.deadline(), Some(t0 + SAFETY));
        p.foreign_released(n, ms(t0, 1000));
        assert!(!p.is_held(n));
        assert!(!p.allows_auto_connect(ms(t0, 1499)));
        assert!(p.allows_auto_connect(ms(t0, 1500)));
        assert_eq!(p.deadline(), Some(ms(t0, 1500)));
    }

    #[test]
    fn on_timer_retires_elapsed_quiet_deadline() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        let n = Path::new("/dev/ttyACM0");
        p.foreign_open(n, esptool(), t0);
        p.foreign_released(n, ms(t0, 100));
        p.on_timer(ms(t0, 300));
        assert_eq!(p.deadline(), Some(ms(t0, 600)));
        p.on_timer(ms(t0, 600));
        assert_eq!(p.deadline(), None);
        assert!(p.allows_auto_connect(ms(t0, 600)));
    }

    #[test]
    fn safety_net_lifts_block_and_retires_deadline() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        let n = Path::new("/dev/ttyACM0");
        p.foreign_open(n, Holder::Unknown, t0);
        assert!(!p.allows_auto_connect(t0 + SAFETY - Duration::from_millis(1)));
        assert!(p.allows_auto_connect(t0 + SAFETY));
        p.on_timer(t0 + SAFETY);
        assert_eq!(p.deadline(), None);
        assert!(p.allows_auto_connect(t0 + SAFETY));
        assert!(p.is_held(n));
    }

    #[test]
    fn oldest_holder_drives_safety_and_last_release_drives_quiet() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        let a = Path::new("/dev/ttyACM0");
        let b = Path::new("/dev/ttyUSB0");
        p.foreign_open(a, esptool(), t0);
        p.foreign_open(b, Holder::Unknown, ms(t0, 2000));
        assert_eq!(p.deadline(), Some(t0 + SAFETY));
        p.foreign_released(a, ms(t0, 3000));
        // b still held: no quiet deadline, b's safety deadline instead.
        assert!(!p.allows_auto_connect(ms(t0, 4000)));
        assert_eq!(p.deadline(), Some(ms(t0, 2000) + SAFETY));
        p.foreign_released(b, ms(t0, 5000));
        assert_eq!(p.deadline(), Some(ms(t0, 5500)));
        assert!(p.allows_auto_connect(ms(t0, 5500)));
    }

    #[test]
    fn repeated_open_keeps_first_since() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        let n = Path::new("/dev/ttyACM0");
        p.foreign_open(n, esptool(), t0);
        p.foreign_open(n, esptool(), ms(t0, 5000));
        assert_eq!(p.deadline(), Some(t0 + SAFETY));
    }

    #[test]
    fn forget_drops_holder_without_quiet_period() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        let n = Path::new("/dev/ttyACM0");
        p.foreign_open(n, esptool(), t0);
        p.forget(n);
        assert!(!p.is_held(n));
        assert!(p.allows_auto_connect(ms(t0, 1)));
        assert_eq!(p.deadline(), None);
        // A later holder starts its own safety window.
        p.foreign_open(n, esptool(), ms(t0, 5000));
        assert_eq!(p.deadline(), Some(ms(t0, 5000) + SAFETY));
    }

    #[test]
    fn spurious_release_starts_no_quiet_period() {
        let t0 = Instant::now();
        let mut p = PauseTracker::new();
        p.foreign_released(Path::new("/dev/ttyACM0"), t0);
        assert!(p.allows_auto_connect(t0));
        assert_eq!(p.deadline(), None);
    }
}

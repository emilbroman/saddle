//! The batching policy: a batch closes after a quiet period with no new
//! changes, or when it has been open for the maximum window, whichever comes
//! first. Time is passed in, so the policy is testable without a clock.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Batch {
    pub paths: BTreeSet<PathBuf>,
    pub git_paths: BTreeSet<PathBuf>,
    pub window: Option<(SystemTime, SystemTime)>,
    /// The OS dropped events; the whole project must be compared against what
    /// Saddle last knew.
    pub rescan: bool,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.git_paths.is_empty() && !self.rescan
    }
}

#[derive(Debug)]
pub struct Batcher {
    quiet: Duration,
    max_window: Duration,
    /// When the open batch received its first and latest change.
    window: Option<(Instant, Instant)>,
    occurred: Option<Instant>,
    batch: Batch,
}

impl Batcher {
    pub fn new(quiet: Duration, max_window: Duration) -> Self {
        Self {
            quiet,
            max_window,
            window: None,
            occurred: None,
            batch: Batch::default(),
        }
    }

    pub fn add(&mut self, path: PathBuf, now: Instant) {
        self.batch.paths.insert(path);
        self.touch(now);
    }

    pub fn add_git(&mut self, path: PathBuf, now: Instant) {
        self.batch.git_paths.insert(path);
        self.touch(now);
    }

    pub fn add_rescan(&mut self, now: Instant) {
        self.batch.rescan = true;
        self.touch(now);
    }

    pub fn note_occurred(&mut self, at: Instant) {
        self.occurred = Some(self.occurred.map_or(at, |old| old.min(at)));
    }

    fn touch(&mut self, now: Instant) {
        let first = self.window.map_or(now, |(first, _)| first);
        self.window = Some((first, now));
    }

    /// When the open batch is due, if there is one.
    pub fn deadline(&self) -> Option<Instant> {
        self.window
            .map(|(first, last)| (last + self.quiet).min(first + self.max_window))
    }

    /// Takes the open batch if it is due at `now`.
    pub fn take_due(&mut self, now: Instant) -> Option<Batch> {
        if self.deadline()? > now {
            return None;
        }
        let (first, _) = self.window.take().unwrap();
        let first = self.occurred.take().map_or(first, |at| at.min(first));
        let end = SystemTime::now();
        self.batch.window = Some((end - now.saturating_duration_since(first), end));
        Some(std::mem::take(&mut self.batch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_secs(1);
    const MAX: Duration = Duration::from_secs(30);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn paths(batch: &Batch) -> Vec<&str> {
        batch.paths.iter().map(|p| p.to_str().unwrap()).collect()
    }

    #[test]
    fn empty_batcher_never_flushes() {
        let t0 = Instant::now();
        let mut b = Batcher::new(QUIET, MAX);
        assert_eq!(b.deadline(), None);
        assert_eq!(b.take_due(t0 + Duration::from_secs(3600)), None);
    }

    #[test]
    fn flushes_after_the_quiet_period() {
        let t0 = Instant::now();
        let mut b = Batcher::new(QUIET, MAX);
        b.add("a".into(), t0);
        assert_eq!(b.deadline(), Some(t0 + QUIET));
        assert_eq!(b.take_due(t0 + ms(999)), None);
        let batch = b.take_due(t0 + QUIET).unwrap();
        assert_eq!(paths(&batch), ["a"]);
        assert!(!batch.rescan);
        // Nothing left afterwards.
        assert_eq!(b.deadline(), None);
        assert_eq!(b.take_due(t0 + Duration::from_secs(10)), None);
    }

    #[test]
    fn each_change_resets_the_quiet_period() {
        let t0 = Instant::now();
        let mut b = Batcher::new(QUIET, MAX);
        b.add("a".into(), t0);
        b.add("b".into(), t0 + ms(800));
        assert_eq!(b.take_due(t0 + ms(1500)), None);
        b.add("a".into(), t0 + ms(1600));
        assert_eq!(b.deadline(), Some(t0 + ms(2600)));
        assert_eq!(b.take_due(t0 + ms(2599)), None);
        assert_eq!(paths(&b.take_due(t0 + ms(2600)).unwrap()), ["a", "b"]);
    }

    #[test]
    fn continuous_changes_flush_at_the_window_cap() {
        let t0 = Instant::now();
        let mut b = Batcher::new(QUIET, MAX);
        let mut now = t0;
        let mut flushed = None;
        // A save every 500 ms never leaves a 1 s gap.
        for i in 0..100 {
            if let Some(batch) = b.take_due(now) {
                flushed = Some((now, batch));
                break;
            }
            b.add(format!("f{}", i % 3).into(), now);
            now += ms(500);
        }
        let (at, batch) = flushed.expect("the cap forces a flush");
        assert_eq!(at, t0 + MAX);
        assert_eq!(batch.paths.len(), 3);

        // The next change opens a fresh window.
        b.add("g".into(), at + ms(10));
        assert_eq!(b.deadline(), Some(at + ms(10) + QUIET));
    }

    #[test]
    fn cap_counts_from_the_first_change() {
        let t0 = Instant::now();
        let mut b = Batcher::new(QUIET, Duration::from_secs(2));
        b.add("a".into(), t0);
        b.add("a".into(), t0 + ms(1900));
        assert_eq!(b.deadline(), Some(t0 + Duration::from_secs(2)));
    }

    #[test]
    fn metadata_shares_batch_without_using_stale_debounce_times_for_quiet_period() {
        let start = Instant::now();
        let mut b = Batcher::new(QUIET, MAX);
        b.add("a".into(), start);
        b.note_occurred(start - ms(100));
        b.add_git(".git/HEAD".into(), start + ms(800));
        b.note_occurred(start); // Deduplicated OS events can retain an older timestamp.
        assert_eq!(b.deadline(), Some(start + ms(1800)));
        assert!(b.take_due(start + ms(1000)).is_none());
        let batch = b.take_due(start + ms(1800)).unwrap();
        assert_eq!(paths(&batch), ["a"]);
        assert_eq!(batch.git_paths.len(), 1);
        assert!(!batch.is_empty());
        assert!(batch.window.is_some());
    }

    #[test]
    fn rescan_opens_a_batch() {
        let t0 = Instant::now();
        let mut b = Batcher::new(QUIET, MAX);
        b.add_rescan(t0);
        let batch = b.take_due(t0 + QUIET).unwrap();
        assert!(batch.rescan);
        assert!(!batch.is_empty());
        assert!(batch.paths.is_empty());
    }
}

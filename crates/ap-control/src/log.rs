//! Bounded action-history ring buffer for the bottom status panel.
//!
//! Automatically discards entries beyond `max_entries` (default `50`) so memory usage
//! and UI scroll height stay strictly bounded forever.

use std::collections::VecDeque;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default maximum number of recent action lines retained in memory and shown on screen.
pub const DEFAULT_MAX_LOG_ENTRIES: usize = 50;

/// Fixed-capacity ring buffer of timestamped player action messages.
#[derive(Debug, Clone)]
pub struct ActionLog {
    max_entries: usize,
    entries: VecDeque<String>,
}

impl ActionLog {
    pub fn new(max_entries: usize) -> Self {
        Self {
            max_entries: max_entries.max(1),
            entries: VecDeque::with_capacity(max_entries.min(256)),
        }
    }

    /// Push a new action message (prepended with `HH:MM:SS UTC` timestamp),
    /// discarding the oldest entry if capacity is exceeded.
    pub fn push(&mut self, message: impl AsRef<str>) {
        if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        let ts = format_utc_hms();
        self.entries.push_back(format!("[{ts}] {}", message.as_ref()));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn capacity_limit(&self) -> usize {
        self.max_entries
    }

    /// Snapshot entries in chronological order (oldest retained -> newest).
    pub fn snapshot(&self) -> Vec<String> {
        self.entries.iter().cloned().collect()
    }
}

impl Default for ActionLog {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LOG_ENTRIES)
    }
}

fn format_utc_hms() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    let h = day_secs / 3_600;
    let m = (day_secs % 3_600) / 60;
    let s = day_secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}

//! Per-terminal raw output replication log for the responsive local mirror.
//!
//! The log records the exact bytes read from a terminal's child PTY, plus resize
//! events, in a single durable sequence space. A mirror client replays these
//! entries into its own local terminal emulator, so scrollback, selection, and
//! search happen locally with no server round-trip; only live keystrokes need
//! the network.
//!
//! Design notes:
//! - Recording is *always on* for every terminal (bounded by [`MirrorLog`]'s
//!   byte cap). This is what lets a client that attaches later replay the exact
//!   retained history byte-for-byte, and makes the stream correct on the
//!   alternate screen where grid-reconstruction snapshots cannot be. The cost is
//!   a bounded buffer per terminal and a small lock in the PTY read path.
//! - Output and resize share one monotonic sequence space so a client applies
//!   them in strict order with no cross-type reordering.
//! - The log is the single source of truth for the stream, so there is no race
//!   between capturing a separate screen snapshot and reading the current
//!   sequence: [`MirrorLog::read_from`] returns a consistent view under one lock.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::protocol::MirrorEventKind;

/// Default maximum retained bytes per terminal output log.
///
/// This bounds how far back a freshly attached mirror client can replay. Older
/// output is evicted; a client resuming past the retained window is served a
/// fresh snapshot of what remains.
pub const DEFAULT_MIRROR_LOG_LIMIT_BYTES: usize = 1024 * 1024;

/// Accounting weight for a resize entry so resize churn cannot grow the ring
/// without bound even when it carries no output bytes.
const RESIZE_ENTRY_COST: usize = 8;

/// A single recorded entry in a terminal's output log.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogEntry {
    /// Raw bytes read from the child PTY.
    Output(Vec<u8>),
    /// The source terminal was resized to these dimensions.
    Resize { cols: u16, rows: u16 },
}

impl LogEntry {
    fn byte_cost(&self) -> usize {
        match self {
            LogEntry::Output(bytes) => bytes.len(),
            LogEntry::Resize { .. } => RESIZE_ENTRY_COST,
        }
    }

    fn to_kind(&self) -> MirrorEventKind {
        match self {
            LogEntry::Output(bytes) => MirrorEventKind::Output(bytes.clone()),
            LogEntry::Resize { cols, rows } => MirrorEventKind::Resize {
                cols: *cols,
                rows: *rows,
            },
        }
    }
}

/// A consistent view of the stream for a subscribing or catching-up client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorRead {
    /// The client must (re)establish state: reset a local emulator to
    /// `cols` x `rows` at `base_seq`, then apply `events` (whose sequence
    /// numbers begin at `base_seq + 1`).
    Snapshot {
        base_seq: u64,
        cols: u16,
        rows: u16,
        events: Vec<(u64, MirrorEventKind)>,
    },
    /// The client's requested sequence is still covered by the retained window;
    /// apply these events to continue from where it left off.
    Delta { events: Vec<(u64, MirrorEventKind)> },
}

#[derive(Debug)]
struct MirrorLogInner {
    /// Retained entries paired with their sequence numbers, oldest first.
    entries: VecDeque<(u64, LogEntry)>,
    /// Sequence number to assign to the next appended entry.
    next_seq: u64,
    /// Sum of `byte_cost` over retained entries.
    total_bytes: usize,
    /// Maximum retained bytes before eviction.
    max_bytes: usize,
    /// Size in effect for the oldest retained entry (spawn size, advanced as
    /// resize entries are evicted). This is the size a fresh replay starts at.
    size_at_oldest: (u16, u16),
    /// Size after the most recently appended resize (spawn size initially).
    current_size: (u16, u16),
}

impl MirrorLogInner {
    fn latest_seq(&self) -> u64 {
        self.next_seq.saturating_sub(1)
    }

    /// Sequence such that every entry with a strictly greater seq is retained.
    /// Equals `oldest_retained_seq - 1`, or `latest_seq` when empty.
    fn min_resumable_seq(&self) -> u64 {
        match self.entries.front() {
            Some((seq, _)) => seq.saturating_sub(1),
            None => self.latest_seq(),
        }
    }

    fn push(&mut self, entry: LogEntry) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.total_bytes += entry.byte_cost();
        self.entries.push_back((seq, entry));
        self.evict();
        seq
    }

    fn evict(&mut self) {
        while self.total_bytes > self.max_bytes && self.entries.len() > 1 {
            let Some((_, entry)) = self.entries.pop_front() else {
                break;
            };
            self.total_bytes = self.total_bytes.saturating_sub(entry.byte_cost());
            if let LogEntry::Resize { cols, rows } = entry {
                // The evicted resize now applies to all remaining entries.
                self.size_at_oldest = (cols, rows);
            }
        }
    }

    fn snapshot(&self) -> MirrorRead {
        MirrorRead::Snapshot {
            base_seq: self.min_resumable_seq(),
            cols: self.size_at_oldest.0,
            rows: self.size_at_oldest.1,
            events: self
                .entries
                .iter()
                .map(|(seq, entry)| (*seq, entry.to_kind()))
                .collect(),
        }
    }

    fn events_after(&self, after: u64) -> Vec<(u64, MirrorEventKind)> {
        self.entries
            .iter()
            .filter(|(seq, _)| *seq > after)
            .map(|(seq, entry)| (*seq, entry.to_kind()))
            .collect()
    }
}

/// A `Send + Sync` per-terminal raw output replication log.
#[derive(Debug)]
pub struct MirrorLog {
    inner: Mutex<MirrorLogInner>,
    /// Number of connected mirror clients. Used only to decide whether the PTY
    /// read path should wake the server event loop to flush; recording itself is
    /// unconditional.
    subscribers: AtomicUsize,
}

impl MirrorLog {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_limit(cols, rows, DEFAULT_MIRROR_LOG_LIMIT_BYTES)
    }

    pub fn with_limit(cols: u16, rows: u16, max_bytes: usize) -> Self {
        Self {
            inner: Mutex::new(MirrorLogInner {
                entries: VecDeque::new(),
                next_seq: 1,
                total_bytes: 0,
                max_bytes: max_bytes.max(1),
                size_at_oldest: (cols, rows),
                current_size: (cols, rows),
            }),
            subscribers: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MirrorLogInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records raw child PTY output. Empty writes are ignored. Returns the seq
    /// assigned, or the current latest seq when nothing was recorded.
    pub fn record_output(&self, bytes: &[u8]) -> u64 {
        if bytes.is_empty() {
            return self.lock().latest_seq();
        }
        self.lock().push(LogEntry::Output(bytes.to_vec()))
    }

    /// Records a resize. Redundant resizes (same size) are ignored so idle
    /// resize churn does not fill the ring. Returns the seq assigned, or the
    /// current latest seq when nothing was recorded.
    pub fn record_resize(&self, cols: u16, rows: u16) -> u64 {
        let mut inner = self.lock();
        if inner.current_size == (cols, rows) {
            return inner.latest_seq();
        }
        inner.current_size = (cols, rows);
        inner.push(LogEntry::Resize { cols, rows })
    }

    /// Highest sequence recorded so far (0 when nothing has been recorded).
    #[cfg(test)]
    pub fn latest_seq(&self) -> u64 {
        self.lock().latest_seq()
    }

    /// Produces the events a client needs to reach the current state.
    ///
    /// - `after = None` requests a fresh replay: a [`MirrorRead::Snapshot`] from
    ///   the oldest retained output.
    /// - `after = Some(seq)` requests continuation: a [`MirrorRead::Delta`] when
    ///   `seq` is still covered by the retained window, otherwise a fresh
    ///   [`MirrorRead::Snapshot`] (the requested point was evicted).
    pub(crate) fn read_from(&self, after: Option<u64>) -> MirrorRead {
        let inner = self.lock();
        match after {
            Some(seq) if seq >= inner.min_resumable_seq() && seq <= inner.latest_seq() => {
                MirrorRead::Delta {
                    events: inner.events_after(seq),
                }
            }
            _ => inner.snapshot(),
        }
    }

    /// Registers a mirror subscriber; see [`Self::has_subscribers`].
    pub fn add_subscriber(&self) {
        self.subscribers.fetch_add(1, Ordering::AcqRel);
    }

    /// Unregisters a mirror subscriber.
    pub fn remove_subscriber(&self) {
        // Saturating: never wrap below zero even under unexpected double-remove.
        let mut current = self.subscribers.load(Ordering::Acquire);
        while current > 0 {
            match self.subscribers.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    /// Whether any mirror client is currently attached. The PTY read path uses
    /// this to decide whether recording output should also wake the event loop.
    pub fn has_subscribers(&self) -> bool {
        self.subscribers.load(Ordering::Acquire) > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output_events(read: &MirrorRead) -> Vec<(u64, Vec<u8>)> {
        let events = match read {
            MirrorRead::Snapshot { events, .. } | MirrorRead::Delta { events } => events,
        };
        events
            .iter()
            .filter_map(|(seq, kind)| match kind {
                MirrorEventKind::Output(bytes) => Some((*seq, bytes.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_log_fresh_read_is_empty_snapshot_at_zero() {
        let log = MirrorLog::new(80, 24);
        match log.read_from(None) {
            MirrorRead::Snapshot {
                base_seq,
                cols,
                rows,
                events,
            } => {
                assert_eq!(base_seq, 0);
                assert_eq!((cols, rows), (80, 24));
                assert!(events.is_empty());
            }
            other => panic!("expected snapshot, got {other:?}"),
        }
    }

    #[test]
    fn output_gets_monotonic_contiguous_seqs() {
        let log = MirrorLog::new(80, 24);
        assert_eq!(log.record_output(b"a"), 1);
        assert_eq!(log.record_output(b"bc"), 2);
        assert_eq!(log.record_output(b"d"), 3);
        assert_eq!(log.latest_seq(), 3);

        let snapshot = log.read_from(None);
        assert_eq!(
            output_events(&snapshot),
            vec![(1, b"a".to_vec()), (2, b"bc".to_vec()), (3, b"d".to_vec())]
        );
    }

    #[test]
    fn empty_output_is_ignored() {
        let log = MirrorLog::new(80, 24);
        assert_eq!(log.record_output(b""), 0);
        assert_eq!(log.latest_seq(), 0);
        assert_eq!(log.record_output(b"x"), 1);
    }

    #[test]
    fn resize_shares_sequence_space_and_dedupes() {
        let log = MirrorLog::new(80, 24);
        log.record_output(b"a"); // seq 1
        assert_eq!(log.record_resize(100, 30), 2); // seq 2
        assert_eq!(log.record_resize(100, 30), 2); // dedup: no new seq
        log.record_output(b"b"); // seq 3

        let MirrorRead::Snapshot { events, .. } = log.read_from(None) else {
            panic!("expected snapshot");
        };
        assert_eq!(events.len(), 3);
        assert_eq!(
            events[1].1,
            MirrorEventKind::Resize {
                cols: 100,
                rows: 30
            }
        );
    }

    #[test]
    fn delta_read_returns_only_newer_events() {
        let log = MirrorLog::new(80, 24);
        log.record_output(b"a"); // 1
        log.record_output(b"b"); // 2
        log.record_output(b"c"); // 3

        let delta = log.read_from(Some(1));
        assert!(matches!(delta, MirrorRead::Delta { .. }));
        assert_eq!(
            output_events(&delta),
            vec![(2, b"b".to_vec()), (3, b"c".to_vec())]
        );

        // Caught up: delta with no events.
        let caught_up = log.read_from(Some(3));
        assert_eq!(output_events(&caught_up), Vec::new());
        assert!(matches!(caught_up, MirrorRead::Delta { .. }));
    }

    #[test]
    fn eviction_bounds_bytes_and_advances_base() {
        // Cap of 4 bytes forces eviction of the oldest output.
        let log = MirrorLog::with_limit(80, 24, 4);
        log.record_output(b"aa"); // seq 1, 2 bytes
        log.record_output(b"bb"); // seq 2, total 4
        log.record_output(b"cc"); // seq 3, total 6 -> evict seq1 -> total 4

        let MirrorRead::Snapshot {
            base_seq, events, ..
        } = log.read_from(None)
        else {
            panic!("expected snapshot");
        };
        // seq 1 evicted, so replay resumes after seq 1.
        assert_eq!(base_seq, 1);
        assert_eq!(
            output_events(&MirrorRead::Snapshot {
                base_seq,
                cols: 0,
                rows: 0,
                events: events.clone()
            }),
            vec![(2, b"bb".to_vec()), (3, b"cc".to_vec())]
        );
    }

    #[test]
    fn resume_past_evicted_window_falls_back_to_snapshot() {
        let log = MirrorLog::with_limit(80, 24, 4);
        log.record_output(b"aa"); // 1
        log.record_output(b"bb"); // 2
        log.record_output(b"cc"); // 3 -> evict seq1

        // Requesting resume from the evicted seq 1 falls back to a snapshot.
        assert!(matches!(
            log.read_from(Some(0)),
            MirrorRead::Snapshot { .. }
        ));
        // Resume from seq 1 is still the boundary (min_resumable = 1) -> delta.
        assert!(matches!(log.read_from(Some(1)), MirrorRead::Delta { .. }));
    }

    #[test]
    fn evicting_resize_advances_start_size() {
        // Small cap so the initial resize entry is evicted by later output.
        let log = MirrorLog::with_limit(80, 24, RESIZE_ENTRY_COST);
        log.record_resize(100, 30); // seq 1 (resize), cost RESIZE_ENTRY_COST
        log.record_output(b"aaaaaaaa"); // seq 2, forces eviction of the resize

        let MirrorRead::Snapshot {
            cols,
            rows,
            base_seq,
            ..
        } = log.read_from(None)
        else {
            panic!("expected snapshot");
        };
        // The evicted resize's size becomes the replay's starting size.
        assert_eq!((cols, rows), (100, 30));
        assert_eq!(base_seq, 1);
    }

    #[test]
    fn subscriber_tracking() {
        let log = MirrorLog::new(80, 24);
        assert!(!log.has_subscribers());
        log.add_subscriber();
        assert!(log.has_subscribers());
        log.add_subscriber();
        log.remove_subscriber();
        assert!(log.has_subscribers());
        log.remove_subscriber();
        assert!(!log.has_subscribers());
        // Never underflows.
        log.remove_subscriber();
        assert!(!log.has_subscribers());
    }
}

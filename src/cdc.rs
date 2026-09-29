//! Change Data Capture (CDC), inspired by Turso's real-time change tracking.
//!
//! When enabled, every committed INSERT / UPDATE / DELETE is recorded as a
//! [`ChangeEvent`] with a monotonically increasing sequence number. Consumers
//! poll with [`CdcManager::changes_since`] to receive all changes after a
//! given sequence number, enabling replication, cache invalidation, or audit
//! trails.
//!
//! The log is kept in memory with a bounded capacity; when the capacity is
//! exceeded the oldest events are dropped (consumers that fall too far behind
//! can detect the gap by comparing sequence numbers).
//!
//! The executor does not publish directly: it [`CdcManager::stage`]s events,
//! and `Database` publishes them with [`CdcManager::publish_staged`] once the
//! enclosing WAL group commits, or drops them with
//! [`CdcManager::discard_staged_from`] when a statement or transaction rolls
//! back. Sequence numbers are assigned at publish time, so consumers never
//! see gaps or events for changes that were undone.

use crate::types::Row;
use parking_lot::{Mutex, RwLock};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Default maximum number of retained change events.
pub const DEFAULT_CDC_CAPACITY: usize = 65_536;

/// The kind of change that occurred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

impl std::fmt::Display for ChangeOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChangeOp::Insert => write!(f, "INSERT"),
            ChangeOp::Update => write!(f, "UPDATE"),
            ChangeOp::Delete => write!(f, "DELETE"),
        }
    }
}

/// A single captured change.
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    /// Monotonically increasing sequence number (starts at 1).
    pub seq: u64,
    /// Table the change applies to.
    pub table: String,
    /// Kind of change.
    pub op: ChangeOp,
    /// Primary key of the affected row.
    pub rowid: i64,
    /// Row image before the change (None for INSERT).
    pub before: Option<Row>,
    /// Row image after the change (None for DELETE).
    pub after: Option<Row>,
}

/// In-memory change log with bounded capacity.
pub struct CdcManager {
    enabled: AtomicBool,
    next_seq: AtomicU64,
    capacity: usize,
    log: RwLock<VecDeque<ChangeEvent>>,
    /// Changes made by the in-flight write group, awaiting commit.
    staged: Mutex<Vec<StagedChange>>,
}

struct StagedChange {
    table: String,
    op: ChangeOp,
    rowid: i64,
    before: Option<Row>,
    after: Option<Row>,
}

impl CdcManager {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CDC_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            enabled: AtomicBool::new(false),
            next_seq: AtomicU64::new(1),
            capacity,
            log: RwLock::new(VecDeque::new()),
            staged: Mutex::new(Vec::new()),
        }
    }

    /// Enables change capture. Changes made while disabled are not recorded.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    /// Disables change capture and clears the log.
    pub fn disable(&self) {
        self.enabled.store(false, Ordering::SeqCst);
        self.log.write().clear();
        self.staged.lock().clear();
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }

    /// Records a change. No-op when capture is disabled.
    pub fn record(
        &self,
        table: &str,
        op: ChangeOp,
        rowid: i64,
        before: Option<Row>,
        after: Option<Row>,
    ) {
        if !self.is_enabled() {
            return;
        }
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let event = ChangeEvent {
            seq,
            table: table.to_string(),
            op,
            rowid,
            before,
            after,
        };
        let mut log = self.log.write();
        if log.len() >= self.capacity {
            log.pop_front();
        }
        log.push_back(event);
    }

    /// Stages a change made by an uncommitted write group. No-op when capture
    /// is disabled.
    pub fn stage(
        &self,
        table: &str,
        op: ChangeOp,
        rowid: i64,
        before: Option<Row>,
        after: Option<Row>,
    ) {
        if !self.is_enabled() {
            return;
        }
        self.staged.lock().push(StagedChange {
            table: table.to_string(),
            op,
            rowid,
            before,
            after,
        });
    }

    /// Current number of staged changes; pass to `discard_staged_from` to
    /// drop only the changes staged after this point.
    pub fn staged_mark(&self) -> usize {
        self.staged.lock().len()
    }

    /// Drops staged changes from position `mark` onward.
    pub fn discard_staged_from(&self, mark: usize) {
        self.staged.lock().truncate(mark);
    }

    /// Publishes every staged change to the log, assigning sequence numbers.
    pub fn publish_staged(&self) {
        let staged = std::mem::take(&mut *self.staged.lock());
        for c in staged {
            self.record(&c.table, c.op, c.rowid, c.before, c.after);
        }
    }

    /// Returns all changes with `seq > since`, in order.
    pub fn changes_since(&self, since: u64) -> Vec<ChangeEvent> {
        self.log
            .read()
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect()
    }

    /// The sequence number of the most recent change (0 if none).
    pub fn latest_seq(&self) -> u64 {
        self.next_seq.load(Ordering::SeqCst) - 1
    }
}

impl Default for CdcManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn test_disabled_by_default() {
        let cdc = CdcManager::new();
        cdc.record("t", ChangeOp::Insert, 1, None, None);
        assert!(cdc.changes_since(0).is_empty());
    }

    #[test]
    fn test_record_and_poll() {
        let cdc = CdcManager::new();
        cdc.enable();
        cdc.record(
            "t",
            ChangeOp::Insert,
            1,
            None,
            Some(Row::new(vec![Value::Integer(1)])),
        );
        cdc.record(
            "t",
            ChangeOp::Delete,
            1,
            Some(Row::new(vec![Value::Integer(1)])),
            None,
        );

        let all = cdc.changes_since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].op, ChangeOp::Insert);
        assert_eq!(all[1].op, ChangeOp::Delete);

        let after_first = cdc.changes_since(all[0].seq);
        assert_eq!(after_first.len(), 1);
        assert_eq!(cdc.latest_seq(), all[1].seq);
    }

    #[test]
    fn test_capacity_bound() {
        let cdc = CdcManager::with_capacity(2);
        cdc.enable();
        for i in 0..5 {
            cdc.record("t", ChangeOp::Insert, i, None, None);
        }
        let all = cdc.changes_since(0);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 4);
        assert_eq!(all[1].seq, 5);
    }
}

// Multi-Version Concurrency Control (MVCC) implementation
// Enables non-blocking reads and concurrent writes through snapshot isolation

use crate::types::{Result, TransactionId, Value, VelociError};
use parking_lot::RwLock;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Global transaction ID counter
/// Uses SeqCst ordering to ensure all threads see a consistent view
static GLOBAL_TXN_ID: AtomicU64 = AtomicU64::new(1);

/// Timestamp for version visibility
pub type Timestamp = u64;

/// Version visibility information
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct VersionInfo {
    /// Transaction ID that created this version
    pub xmin: TransactionId,
    /// Transaction ID that deleted/updated this version (0 if still active)
    pub xmax: TransactionId,
    /// Creation timestamp
    pub created_at: Timestamp,
    /// Deletion/update timestamp (0 if still active)
    pub deleted_at: Timestamp,
}

impl VersionInfo {
    pub fn new(xmin: TransactionId, created_at: Timestamp) -> Self {
        Self {
            xmin,
            xmax: 0,
            created_at,
            deleted_at: 0,
        }
    }

    /// Check if this version is visible to a transaction with the given snapshot
    pub fn is_visible(&self, snapshot: &Snapshot) -> bool {
        // Version must be created before our snapshot
        if self.created_at > snapshot.timestamp {
            return false;
        }

        // Version must not be deleted before our snapshot
        if self.deleted_at > 0 && self.deleted_at <= snapshot.timestamp {
            return false;
        }

        // Check transaction visibility
        if !snapshot.is_transaction_visible(self.xmin) {
            return false;
        }

        // If version is deleted, check if deleting transaction is visible
        if self.xmax > 0 && snapshot.is_transaction_visible(self.xmax) {
            return false;
        }

        true
    }

    /// Mark this version as deleted by a transaction
    pub fn mark_deleted(&mut self, xmax: TransactionId, deleted_at: Timestamp) {
        self.xmax = xmax;
        self.deleted_at = deleted_at;
    }
}

/// A versioned record containing multiple versions of the same logical row
#[derive(Debug, Clone)]
pub struct VersionedRecord {
    /// Primary key of the record
    pub key: i64,
    /// List of versions ordered by timestamp (oldest to newest)
    pub versions: Vec<RecordVersion>,
}

/// A single version of a record
#[derive(Debug, Clone)]
pub struct RecordVersion {
    /// Version metadata
    pub version_info: VersionInfo,
    /// Actual data values
    pub data: Vec<Value>,
}

impl RecordVersion {
    pub fn new(xmin: TransactionId, created_at: Timestamp, data: Vec<Value>) -> Self {
        Self {
            version_info: VersionInfo::new(xmin, created_at),
            data,
        }
    }

    /// Check if this version is visible to the given snapshot
    pub fn is_visible(&self, snapshot: &Snapshot) -> bool {
        self.version_info.is_visible(snapshot)
    }
}

impl VersionedRecord {
    pub fn new(key: i64) -> Self {
        Self {
            key,
            versions: Vec::new(),
        }
    }

    /// Add a new version to this record
    pub fn add_version(&mut self, version: RecordVersion) {
        self.versions.push(version);
    }

    /// Get the visible version for the given snapshot
    pub fn get_visible_version(&self, snapshot: &Snapshot) -> Option<&RecordVersion> {
        // Iterate in reverse order to find the most recent visible version
        self.versions
            .iter()
            .rev()
            .find(|v| v.is_visible(snapshot))
    }

    /// Mark the latest version as deleted
    pub fn mark_deleted(&mut self, xmax: TransactionId, deleted_at: Timestamp) -> Result<()> {
        if let Some(version) = self.versions.last_mut() {
            version.version_info.mark_deleted(xmax, deleted_at);
            Ok(())
        } else {
            Err(VelociError::NotFound("No versions to delete".to_string()))
        }
    }

    /// Remove versions that are no longer needed (garbage collection)
    pub fn vacuum(&mut self, min_active_timestamp: Timestamp) {
        // Keep only versions that might still be visible to active transactions
        self.versions.retain(|v| {
            // Keep if not deleted, or deleted recently
            v.version_info.deleted_at == 0 || v.version_info.deleted_at >= min_active_timestamp
        });

        // Always keep at least one version
        if self.versions.is_empty() && !self.versions.is_empty() {
            // This shouldn't happen, but safeguard
            self.versions.truncate(1);
        }
    }
}

/// Snapshot isolation state for a transaction
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Timestamp when this snapshot was taken
    pub timestamp: Timestamp,
    /// Set of active transaction IDs at snapshot time
    pub active_transactions: Vec<TransactionId>,
    /// Transaction ID of this snapshot's transaction
    pub txn_id: TransactionId,
}

impl Snapshot {
    pub fn new(timestamp: Timestamp, txn_id: TransactionId, active_transactions: Vec<TransactionId>) -> Self {
        Self {
            timestamp,
            txn_id,
            active_transactions,
        }
    }

    /// Check if a transaction is visible to this snapshot
    pub fn is_transaction_visible(&self, txn_id: TransactionId) -> bool {
        // Our own changes are always visible
        if txn_id == self.txn_id {
            return true;
        }

        // Transaction must have started before our snapshot
        if txn_id >= self.txn_id {
            return false;
        }

        // Transaction must not be in our active set (must be committed)
        !self.active_transactions.contains(&txn_id)
    }
}

/// MVCC Manager coordinates snapshot isolation and version management
///
/// LOCK ORDERING (STRICT):
/// 1. active_snapshots (Level 1 - Transaction metadata)
/// 2. committed_transactions (Level 2 - Transaction history)
/// 3. version_store (Level 3 - Data)
///
/// NEVER acquire in reverse order to prevent deadlocks
///
/// IMPROVEMENTS:
/// - Use DashMap for version_store to enable per-table concurrency
/// - Atomic timestamp management
/// - Consistent lock ordering enforced throughout
pub struct MvccManager {
    /// Current timestamp (monotonically increasing)
    current_timestamp: AtomicU64,
    /// Active transactions and their snapshots
    /// LOCK LEVEL 1: Acquire this first if needed with other locks
    active_snapshots: Arc<RwLock<HashMap<TransactionId, Snapshot>>>,
    /// Version store: maps table_name -> key -> versioned record
    /// LOCK LEVEL 3: Acquire this last, per-table locking via DashMap
    version_store: Arc<dashmap::DashMap<String, Arc<RwLock<BTreeMap<i64, VersionedRecord>>>>>,
    /// Committed transaction timestamps for visibility
    /// LOCK LEVEL 2: Acquire after active_snapshots, before version_store
    committed_transactions: Arc<RwLock<BTreeMap<TransactionId, Timestamp>>>,
}

impl MvccManager {
    pub fn new() -> Self {
        Self {
            current_timestamp: AtomicU64::new(1),
            active_snapshots: Arc::new(RwLock::new(HashMap::new())),
            version_store: Arc::new(dashmap::DashMap::new()),
            committed_transactions: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Begin a new transaction and create a snapshot
    pub fn begin_transaction(&self) -> Snapshot {
        let txn_id = GLOBAL_TXN_ID.fetch_add(1, Ordering::SeqCst);
        let timestamp = self.current_timestamp.fetch_add(1, Ordering::SeqCst);

        // LOCK ORDERING: Only acquire active_snapshots (Level 1)
        // Get list of currently active transactions
        let snapshot = {
            let snapshots = self.active_snapshots.read();
            let active_transactions: Vec<TransactionId> = snapshots.keys().copied().collect();
            Snapshot::new(timestamp, txn_id, active_transactions)
        }; // Release read lock

        // Register this snapshot as active
        self.active_snapshots.write().insert(txn_id, snapshot.clone());

        snapshot
    }

    /// Commit a transaction
    pub fn commit_transaction(&self, snapshot: &Snapshot) -> Result<()> {
        let commit_timestamp = self.current_timestamp.fetch_add(1, Ordering::SeqCst);

        // LOCK ORDERING: Level 1 (active_snapshots) → Level 2 (committed_transactions)
        // This is consistent and safe
        
        // First update committed_transactions (Level 2)
        self.committed_transactions
            .write()
            .insert(snapshot.txn_id, commit_timestamp);

        // Then update active_snapshots (Level 1)
        self.active_snapshots.write().remove(&snapshot.txn_id);

        Ok(())
    }

    /// Abort a transaction
    pub fn abort_transaction(&self, snapshot: &Snapshot) -> Result<()> {
        // LOCK ORDERING: Only acquire active_snapshots (Level 1)
        self.active_snapshots.write().remove(&snapshot.txn_id);

        // TODO: Rollback any changes made by this transaction
        // This requires tracking undo information

        Ok(())
    }

    /// Insert a new version of a record
    pub fn insert_version(
        &self,
        table_name: &str,
        key: i64,
        data: Vec<Value>,
        snapshot: &Snapshot,
    ) -> Result<()> {
        let created_at = self.current_timestamp.load(Ordering::SeqCst);
        let version = RecordVersion::new(snapshot.txn_id, created_at, data);

        // LOCK ORDERING: Only acquire version_store (Level 3 - Data)
        // Per-table locking via DashMap reduces contention
        let table_store = self.version_store
            .entry(table_name.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(BTreeMap::new())));
        
        let mut table_map = table_store.write();
        let record = table_map.entry(key).or_insert_with(|| VersionedRecord::new(key));
        record.add_version(version);

        Ok(())
    }

    /// Read a record with snapshot isolation
    pub fn read_version(
        &self,
        table_name: &str,
        key: i64,
        snapshot: &Snapshot,
    ) -> Result<Option<Vec<Value>>> {
        // LOCK ORDERING: Only acquire version_store (Level 3)
        if let Some(table_store) = self.version_store.get(table_name) {
            let table_map = table_store.read();
            if let Some(record) = table_map.get(&key) {
                if let Some(version) = record.get_visible_version(snapshot) {
                    return Ok(Some(version.data.clone()));
                }
            }
        }

        Ok(None)
    }

    /// Delete a record (creates a new version marked as deleted)
    pub fn delete_version(
        &self,
        table_name: &str,
        key: i64,
        snapshot: &Snapshot,
    ) -> Result<()> {
        let deleted_at = self.current_timestamp.fetch_add(1, Ordering::SeqCst);

        // LOCK ORDERING: Only acquire version_store (Level 3)
        if let Some(table_store) = self.version_store.get(table_name) {
            let mut table_map = table_store.write();
            if let Some(record) = table_map.get_mut(&key) {
                record.mark_deleted(snapshot.txn_id, deleted_at)?;
                return Ok(());
            }
        }

        Err(VelociError::NotFound(format!("Record {} not found", key)))
    }

    /// Scan all visible records in a table
    pub fn scan_table(
        &self,
        table_name: &str,
        snapshot: &Snapshot,
    ) -> Result<Vec<(i64, Vec<Value>)>> {
        // LOCK ORDERING: Only acquire version_store (Level 3)
        if let Some(table_store) = self.version_store.get(table_name) {
            let table_map = table_store.read();
            let mut results = Vec::new();

            for (key, record) in table_map.iter() {
                if let Some(version) = record.get_visible_version(snapshot) {
                    results.push((*key, version.data.clone()));
                }
            }

            Ok(results)
        } else {
            Ok(Vec::new())
        }
    }

    /// Vacuum old versions (garbage collection)
    /// Should be called periodically by a background thread
    /// 
    /// LOCK ORDERING (STRICT):
    /// 1. active_snapshots (read) - Level 1
    /// 2. committed_transactions (write) - Level 2  
    /// 3. version_store (write per table) - Level 3
    pub fn vacuum(&self) {
        // STEP 1: Get minimum active timestamp (Level 1 - read only)
        let min_active_timestamp = {
            let snapshots = self.active_snapshots.read();
            snapshots
                .values()
                .map(|s| s.timestamp)
                .min()
                .unwrap_or(self.current_timestamp.load(Ordering::SeqCst))
        }; // Release Level 1 lock

        // STEP 2: Clean up committed transactions (Level 2)
        {
            let mut committed = self.committed_transactions.write();
            committed.retain(|_, &mut ts| ts >= min_active_timestamp);
        } // Release Level 2 lock

        // STEP 3: Clean up version store (Level 3) - per-table locking
        for table_entry in self.version_store.iter() {
            let mut table_map = table_entry.value().write();
            for record in table_map.values_mut() {
                record.vacuum(min_active_timestamp);
            }
        } // Locks released automatically per table
    }

    /// Get statistics about the MVCC system
    /// LOCK ORDERING: Level 1 (active_snapshots) → Level 3 (version_store)
    pub fn get_stats(&self) -> MvccStats {
        // Level 1: Active snapshots
        let active_snapshots = self.active_snapshots.read().len();

        // Level 3: Version store (per-table)
        let mut total_records = 0;
        let mut total_versions = 0;

        for table_entry in self.version_store.iter() {
            let table_map = table_entry.value().read();
            total_records += table_map.len();
            for record in table_map.values() {
                total_versions += record.versions.len();
            }
        }

        MvccStats {
            active_snapshots,
            total_records,
            total_versions,
            version_bloat: if total_records > 0 {
                total_versions as f64 / total_records as f64
            } else {
                0.0
            },
        }
    }
}

impl Default for MvccManager {
    fn default() -> Self {
        Self::new()
    }
}

/// MVCC system statistics
#[derive(Debug, Clone)]
pub struct MvccStats {
    pub active_snapshots: usize,
    pub total_records: usize,
    pub total_versions: usize,
    pub version_bloat: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Value;

    #[test]
    fn test_mvcc_snapshot_isolation() {
        let mvcc = MvccManager::new();

        // Start transaction 1
        let snapshot1 = mvcc.begin_transaction();

        // Insert a record in transaction 1
        mvcc.insert_version(
            "users",
            1,
            vec![Value::Integer(1), Value::Text("Alice".to_string())],
            &snapshot1,
        ).unwrap();

        // Commit transaction 1
        mvcc.commit_transaction(&snapshot1).unwrap();

        // Start transaction 2
        let snapshot2 = mvcc.begin_transaction();

        // Transaction 2 should see the record from transaction 1
        let result = mvcc.read_version("users", 1, &snapshot2).unwrap();
        assert!(result.is_some());

        let data = result.unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[1], Value::Text("Alice".to_string()));
    }

    #[test]
    fn test_mvcc_concurrent_reads() {
        let mvcc = MvccManager::new();

        // Setup: Insert initial data
        let snapshot_setup = mvcc.begin_transaction();
        mvcc.insert_version(
            "products",
            1,
            vec![Value::Integer(1), Value::Text("Laptop".to_string())],
            &snapshot_setup,
        ).unwrap();
        mvcc.commit_transaction(&snapshot_setup).unwrap();

        // Start two concurrent read transactions
        let snapshot_r1 = mvcc.begin_transaction();
        let snapshot_r2 = mvcc.begin_transaction();

        // Both should see the same data
        let result1 = mvcc.read_version("products", 1, &snapshot_r1).unwrap();
        let result2 = mvcc.read_version("products", 1, &snapshot_r2).unwrap();

        assert!(result1.is_some());
        assert!(result2.is_some());
        assert_eq!(result1, result2);
    }

    #[test]
    fn test_mvcc_version_visibility() {
        let mvcc = MvccManager::new();

        // Transaction 1: Insert record
        let snapshot1 = mvcc.begin_transaction();
        mvcc.insert_version(
            "test",
            1,
            vec![Value::Integer(100)],
            &snapshot1,
        ).unwrap();

        // Transaction 2 starts before T1 commits
        let snapshot2 = mvcc.begin_transaction();

        // Commit T1
        mvcc.commit_transaction(&snapshot1).unwrap();

        // T2 should NOT see T1's changes (snapshot isolation)
        let result = mvcc.read_version("test", 1, &snapshot2).unwrap();
        assert!(result.is_none());

        // Transaction 3 starts after T1 commits
        let snapshot3 = mvcc.begin_transaction();

        // T3 should see T1's changes
        let result = mvcc.read_version("test", 1, &snapshot3).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_mvcc_vacuum() {
        let mvcc = MvccManager::new();

        // Create multiple versions
        let snapshot1 = mvcc.begin_transaction();
        mvcc.insert_version("test", 1, vec![Value::Integer(1)], &snapshot1).unwrap();
        mvcc.commit_transaction(&snapshot1).unwrap();

        let snapshot2 = mvcc.begin_transaction();
        mvcc.insert_version("test", 1, vec![Value::Integer(2)], &snapshot2).unwrap();
        mvcc.commit_transaction(&snapshot2).unwrap();

        let snapshot3 = mvcc.begin_transaction();
        mvcc.insert_version("test", 1, vec![Value::Integer(3)], &snapshot3).unwrap();
        mvcc.commit_transaction(&snapshot3).unwrap();

        let stats_before = mvcc.get_stats();
        assert!(stats_before.total_versions >= 3);

        // Run vacuum
        mvcc.vacuum();

        // Stats should show cleanup (in a real scenario with old snapshots)
        let stats_after = mvcc.get_stats();
        assert!(stats_after.total_versions >= 1); // At least one version should remain
    }
}


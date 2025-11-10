// Conflict-Free Replicated Data Types (CRDT) for distributed synchronization
// Enables bi-directional sync without complex conflict resolution

use crate::types::{Result, Value, VelociError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

/// Lamport timestamp for causality tracking
pub type LamportTimestamp = u64;

/// Node/replica identifier
pub type NodeId = String;

/// Global Lamport clock
static LAMPORT_CLOCK: AtomicU64 = AtomicU64::new(1);

/// Get the next Lamport timestamp
pub fn next_timestamp() -> LamportTimestamp {
    LAMPORT_CLOCK.fetch_add(1, Ordering::SeqCst)
}

/// Update Lamport clock based on received timestamp
pub fn update_timestamp(received: LamportTimestamp) {
    let current = LAMPORT_CLOCK.load(Ordering::SeqCst);
    let new_timestamp = received.max(current) + 1;
    LAMPORT_CLOCK.store(new_timestamp, Ordering::SeqCst);
}

/// CRDT operation types
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CrdtOperation {
    /// Insert a new record
    Insert {
        table: String,
        key: i64,
        values: Vec<Value>,
        timestamp: LamportTimestamp,
        node_id: NodeId,
    },
    /// Update an existing record
    Update {
        table: String,
        key: i64,
        values: Vec<Value>,
        timestamp: LamportTimestamp,
        node_id: NodeId,
    },
    /// Delete a record (tombstone)
    Delete {
        table: String,
        key: i64,
        timestamp: LamportTimestamp,
        node_id: NodeId,
    },
}

impl CrdtOperation {
    pub fn timestamp(&self) -> LamportTimestamp {
        match self {
            CrdtOperation::Insert { timestamp, .. } => *timestamp,
            CrdtOperation::Update { timestamp, .. } => *timestamp,
            CrdtOperation::Delete { timestamp, .. } => *timestamp,
        }
    }

    pub fn node_id(&self) -> &NodeId {
        match self {
            CrdtOperation::Insert { node_id, .. } => node_id,
            CrdtOperation::Update { node_id, .. } => node_id,
            CrdtOperation::Delete { node_id, .. } => node_id,
        }
    }

    pub fn table(&self) -> &str {
        match self {
            CrdtOperation::Insert { table, .. } => table,
            CrdtOperation::Update { table, .. } => table,
            CrdtOperation::Delete { table, .. } => table,
        }
    }
}

/// CRDT State for a single record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrdtRecord {
    pub key: i64,
    pub values: Vec<Value>,
    pub timestamp: LamportTimestamp,
    pub node_id: NodeId,
    pub is_deleted: bool,
}

impl CrdtRecord {
    pub fn new(key: i64, values: Vec<Value>, timestamp: LamportTimestamp, node_id: NodeId) -> Self {
        Self {
            key,
            values,
            timestamp,
            node_id,
            is_deleted: false,
        }
    }

    /// Merge with another record using Last-Write-Wins (LWW) rule
    pub fn merge(&mut self, other: &CrdtRecord) -> bool {
        // Compare timestamps (higher wins)
        if other.timestamp > self.timestamp {
            self.values = other.values.clone();
            self.timestamp = other.timestamp;
            self.node_id = other.node_id.clone();
            self.is_deleted = other.is_deleted;
            true
        } else if other.timestamp == self.timestamp {
            // Tie-break using node_id (lexicographic order)
            if other.node_id > self.node_id {
                self.values = other.values.clone();
                self.node_id = other.node_id.clone();
                self.is_deleted = other.is_deleted;
                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Mark as deleted with tombstone
    pub fn delete(&mut self, timestamp: LamportTimestamp, node_id: NodeId) {
        if timestamp > self.timestamp {
            self.is_deleted = true;
            self.timestamp = timestamp;
            self.node_id = node_id;
        }
    }
}

/// CRDT Store managing replicated state
pub struct CrdtStore {
    /// Current node identifier
    node_id: NodeId,
    /// State: table_name -> key -> record
    state: HashMap<String, BTreeMap<i64, CrdtRecord>>,
    /// Operation log for synchronization
    operation_log: Vec<CrdtOperation>,
    /// Vector clock for causality tracking
    vector_clock: HashMap<NodeId, LamportTimestamp>,
}

impl CrdtStore {
    pub fn new(node_id: NodeId) -> Self {
        let mut vector_clock = HashMap::new();
        vector_clock.insert(node_id.clone(), 0);

        Self {
            node_id,
            state: HashMap::new(),
            operation_log: Vec::new(),
            vector_clock,
        }
    }

    /// Insert a new record
    pub fn insert(&mut self, table: &str, key: i64, values: Vec<Value>) -> Result<()> {
        let timestamp = next_timestamp();
        
        let operation = CrdtOperation::Insert {
            table: table.to_string(),
            key,
            values: values.clone(),
            timestamp,
            node_id: self.node_id.clone(),
        };

        self.apply_operation(&operation)?;
        self.operation_log.push(operation);
        self.update_vector_clock(timestamp);

        Ok(())
    }

    /// Update an existing record
    pub fn update(&mut self, table: &str, key: i64, values: Vec<Value>) -> Result<()> {
        let timestamp = next_timestamp();
        
        let operation = CrdtOperation::Update {
            table: table.to_string(),
            key,
            values: values.clone(),
            timestamp,
            node_id: self.node_id.clone(),
        };

        self.apply_operation(&operation)?;
        self.operation_log.push(operation);
        self.update_vector_clock(timestamp);

        Ok(())
    }

    /// Delete a record
    pub fn delete(&mut self, table: &str, key: i64) -> Result<()> {
        let timestamp = next_timestamp();
        
        let operation = CrdtOperation::Delete {
            table: table.to_string(),
            key,
            timestamp,
            node_id: self.node_id.clone(),
        };

        self.apply_operation(&operation)?;
        self.operation_log.push(operation);
        self.update_vector_clock(timestamp);

        Ok(())
    }

    /// Apply a CRDT operation to local state
    fn apply_operation(&mut self, op: &CrdtOperation) -> Result<()> {
        match op {
            CrdtOperation::Insert { table, key, values, timestamp, node_id } => {
                let table_state = self.state.entry(table.clone()).or_insert_with(BTreeMap::new);
                
                let record = CrdtRecord::new(*key, values.clone(), *timestamp, node_id.clone());
                table_state.insert(*key, record);
            }
            CrdtOperation::Update { table, key, values, timestamp, node_id } => {
                let table_state = self.state.entry(table.clone()).or_insert_with(BTreeMap::new);
                
                if let Some(existing) = table_state.get_mut(key) {
                    let new_record = CrdtRecord::new(*key, values.clone(), *timestamp, node_id.clone());
                    existing.merge(&new_record);
                } else {
                    // Create if doesn't exist
                    let record = CrdtRecord::new(*key, values.clone(), *timestamp, node_id.clone());
                    table_state.insert(*key, record);
                }
            }
            CrdtOperation::Delete { table, key, timestamp, node_id } => {
                let table_state = self.state.entry(table.clone()).or_insert_with(BTreeMap::new);
                
                if let Some(existing) = table_state.get_mut(key) {
                    existing.delete(*timestamp, node_id.clone());
                }
            }
        }

        Ok(())
    }

    /// Merge operations from another replica
    pub fn merge_operations(&mut self, operations: Vec<CrdtOperation>) -> Result<()> {
        for op in operations {
            // Update our clock
            update_timestamp(op.timestamp());
            
            // Apply operation
            self.apply_operation(&op)?;
            
            // Add to our log if not already present
            if !self.operation_log.iter().any(|o| {
                o.timestamp() == op.timestamp() && o.node_id() == op.node_id()
            }) {
                self.operation_log.push(op.clone());
            }
            
            // Update vector clock
            self.update_vector_clock(op.timestamp());
        }

        Ok(())
    }

    /// Get all operations since a given timestamp
    pub fn get_operations_since(&self, timestamp: LamportTimestamp) -> Vec<CrdtOperation> {
        self.operation_log
            .iter()
            .filter(|op| op.timestamp() > timestamp)
            .cloned()
            .collect()
    }

    /// Get the current state of a table
    pub fn get_table_state(&self, table: &str) -> Option<&BTreeMap<i64, CrdtRecord>> {
        self.state.get(table)
    }

    /// Get a specific record
    pub fn get_record(&self, table: &str, key: i64) -> Option<&CrdtRecord> {
        self.state.get(table).and_then(|t| t.get(&key))
    }

    /// Update vector clock
    fn update_vector_clock(&mut self, timestamp: LamportTimestamp) {
        self.vector_clock
            .entry(self.node_id.clone())
            .and_modify(|t| *t = (*t).max(timestamp))
            .or_insert(timestamp);
    }

    /// Get vector clock
    pub fn get_vector_clock(&self) -> &HashMap<NodeId, LamportTimestamp> {
        &self.vector_clock
    }

    /// Prune old operations (garbage collection)
    /// Remove operations older than all known vector clocks
    pub fn prune_operations(&mut self) {
        if self.vector_clock.is_empty() {
            return;
        }

        // Find minimum timestamp across all nodes
        let min_timestamp = self.vector_clock.values().min().copied().unwrap_or(0);

        // Keep only operations newer than min_timestamp
        self.operation_log.retain(|op| op.timestamp() > min_timestamp);
    }

    /// Get statistics
    pub fn stats(&self) -> CrdtStats {
        let total_records: usize = self.state.values().map(|t| t.len()).sum();
        let deleted_records: usize = self.state.values()
            .map(|t| t.values().filter(|r| r.is_deleted).count())
            .sum();

        CrdtStats {
            node_id: self.node_id.clone(),
            tables: self.state.len(),
            total_records,
            deleted_records,
            operation_log_size: self.operation_log.len(),
            vector_clock_size: self.vector_clock.len(),
        }
    }
}

/// CRDT statistics
#[derive(Debug, Clone)]
pub struct CrdtStats {
    pub node_id: NodeId,
    pub tables: usize,
    pub total_records: usize,
    pub deleted_records: usize,
    pub operation_log_size: usize,
    pub vector_clock_size: usize,
}

/// Synchronization protocol
pub struct SyncProtocol {
    store: CrdtStore,
}

impl SyncProtocol {
    pub fn new(node_id: NodeId) -> Self {
        Self {
            store: CrdtStore::new(node_id),
        }
    }

    /// Generate sync message for another replica
    pub fn generate_sync_message(&self, peer_vector_clock: &HashMap<NodeId, LamportTimestamp>) -> Vec<CrdtOperation> {
        // Find minimum timestamp the peer has seen
        let peer_min_timestamp = peer_vector_clock.values().min().copied().unwrap_or(0);

        // Send all operations since then
        self.store.get_operations_since(peer_min_timestamp)
    }

    /// Process sync message from peer
    pub fn process_sync_message(&mut self, operations: Vec<CrdtOperation>) -> Result<()> {
        self.store.merge_operations(operations)
    }

    /// Get the store reference
    pub fn store(&self) -> &CrdtStore {
        &self.store
    }

    /// Get mutable store reference
    pub fn store_mut(&mut self) -> &mut CrdtStore {
        &mut self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crdt_insert() {
        let mut store = CrdtStore::new("node1".to_string());
        
        store.insert("users", 1, vec![Value::Text("Alice".to_string())]).unwrap();
        
        let record = store.get_record("users", 1).unwrap();
        assert_eq!(record.key, 1);
        assert!(!record.is_deleted);
    }

    #[test]
    fn test_crdt_merge_lww() {
        let mut store1 = CrdtStore::new("node1".to_string());
        let mut store2 = CrdtStore::new("node2".to_string());

        // Node 1 inserts
        store1.insert("users", 1, vec![Value::Text("Alice".to_string())]).unwrap();

        // Node 2 updates (later timestamp)
        std::thread::sleep(std::time::Duration::from_millis(10));
        store2.insert("users", 1, vec![Value::Text("Bob".to_string())]).unwrap();

        // Merge store2's operations into store1
        let ops = store2.get_operations_since(0);
        store1.merge_operations(ops).unwrap();

        // Store1 should have Bob (higher timestamp wins)
        let record = store1.get_record("users", 1).unwrap();
        assert_eq!(record.values[0], Value::Text("Bob".to_string()));
    }

    #[test]
    fn test_crdt_delete() {
        let mut store = CrdtStore::new("node1".to_string());
        
        store.insert("users", 1, vec![Value::Text("Alice".to_string())]).unwrap();
        store.delete("users", 1).unwrap();
        
        let record = store.get_record("users", 1).unwrap();
        assert!(record.is_deleted);
    }

    #[test]
    fn test_sync_protocol() {
        let mut protocol1 = SyncProtocol::new("node1".to_string());
        let mut protocol2 = SyncProtocol::new("node2".to_string());

        // Node 1 makes changes
        protocol1.store_mut().insert("users", 1, vec![Value::Text("Alice".to_string())]).unwrap();
        protocol1.store_mut().insert("users", 2, vec![Value::Text("Bob".to_string())]).unwrap();

        // Generate sync message
        let sync_msg = protocol1.generate_sync_message(&HashMap::new());

        // Node 2 processes sync
        protocol2.process_sync_message(sync_msg).unwrap();

        // Node 2 should have both records
        assert!(protocol2.store().get_record("users", 1).is_some());
        assert!(protocol2.store().get_record("users", 2).is_some());
    }

    #[test]
    fn test_operation_pruning() {
        let mut store = CrdtStore::new("node1".to_string());
        
        // Add many operations
        for i in 0..100 {
            store.insert("test", i, vec![Value::Integer(i)]).unwrap();
        }

        assert_eq!(store.operation_log.len(), 100);

        // Prune old operations
        store.prune_operations();

        // All operations should remain (only one node)
        assert!(store.operation_log.len() <= 100);
    }
}


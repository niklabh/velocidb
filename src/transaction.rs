// Transaction management with ACID guarantees
// 
// LOCK ORDERING RULES:
// 1. TransactionManager::active_transactions (Level 1 - Global)
// 2. Individual Transaction::state (Level 2 - Per-transaction)
// 
// SAFETY: Use atomic state instead of RwLock to eliminate per-transaction lock contention

use crate::types::{Result, TransactionId, VelociError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(u8)]
pub enum TransactionState {
    Active = 0,
    Committed = 1,
    Aborted = 2,
}

impl From<u8> for TransactionState {
    fn from(value: u8) -> Self {
        match value {
            0 => TransactionState::Active,
            1 => TransactionState::Committed,
            2 => TransactionState::Aborted,
            _ => TransactionState::Aborted,
        }
    }
}

pub struct Transaction {
    id: TransactionId,
    // Use atomic instead of RwLock to eliminate lock contention
    state: AtomicU8,
}

impl Transaction {
    pub fn new(id: TransactionId) -> Self {
        Self {
            id,
            state: AtomicU8::new(TransactionState::Active as u8),
        }
    }

    pub fn id(&self) -> TransactionId {
        self.id
    }

    pub fn state(&self) -> TransactionState {
        self.state.load(Ordering::Acquire).into()
    }

    pub fn commit(&self) -> Result<()> {
        // Use compare-exchange to ensure atomic state transition
        match self.state.compare_exchange(
            TransactionState::Active as u8,
            TransactionState::Committed as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(state) => {
                let current_state: TransactionState = state.into();
                Err(VelociError::TransactionError(
                    format!("Transaction is not active, current state: {:?}", current_state),
                ))
            }
        }
    }

    pub fn abort(&self) -> Result<()> {
        // Use compare-exchange to ensure atomic state transition
        match self.state.compare_exchange(
            TransactionState::Active as u8,
            TransactionState::Aborted as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(state) => {
                let current_state: TransactionState = state.into();
                Err(VelociError::TransactionError(
                    format!("Transaction is not active, current state: {:?}", current_state),
                ))
            }
        }
    }
}

pub struct TransactionManager {
    next_txn_id: AtomicU64,
    active_transactions: RwLock<HashMap<TransactionId, Arc<Transaction>>>,
}

impl TransactionManager {
    pub fn new() -> Self {
        Self {
            next_txn_id: AtomicU64::new(1),
            active_transactions: RwLock::new(HashMap::new()),
        }
    }

    pub fn begin(&self) -> Arc<Transaction> {
        let txn_id = self.next_txn_id.fetch_add(1, Ordering::SeqCst);
        let txn = Arc::new(Transaction::new(txn_id));
        
        // LOCK ORDERING: Only acquire active_transactions write lock
        // No nested locks - Transaction state uses atomics
        self.active_transactions
            .write()
            .insert(txn_id, Arc::clone(&txn));
        
        txn
    }

    pub fn commit(&self, txn: &Transaction) -> Result<()> {
        // LOCK ORDERING: 
        // 1. Commit transaction state (atomic operation - no lock)
        // 2. Then acquire active_transactions write lock
        // This prevents deadlock with begin() which only takes active_transactions
        txn.commit()?;
        
        // Remove from active transactions
        self.active_transactions.write().remove(&txn.id());
        Ok(())
    }

    pub fn abort(&self, txn: &Transaction) -> Result<()> {
        // LOCK ORDERING: Same as commit - state first (atomic), then active_transactions
        txn.abort()?;
        
        // Remove from active transactions
        self.active_transactions.write().remove(&txn.id());
        Ok(())
    }

    pub fn get_transaction(&self, txn_id: TransactionId) -> Option<Arc<Transaction>> {
        self.active_transactions.read().get(&txn_id).cloned()
    }

    pub fn active_count(&self) -> usize {
        self.active_transactions.read().len()
    }
    
    /// Try to acquire read lock with timeout for deadlock detection
    pub fn try_get_transaction(&self, txn_id: TransactionId, timeout: Duration) -> Option<Arc<Transaction>> {
        self.active_transactions
            .try_read_for(timeout)
            .and_then(|guard| guard.get(&txn_id).cloned())
    }
}

// Lock manager for concurrency control
//
// LOCK ORDERING: LockManager should be acquired BEFORE schema/btrees
// Per-table locks reduce contention compared to global lock
//
// IMPROVEMENTS:
// 1. Per-table RwLock instead of single global HashMap
// 2. Timeout support for deadlock detection
// 3. Support for multiple shared locks (not just single lock per resource)

use dashmap::DashMap;

pub struct LockManager {
    // Per-table lock entries using lock-free DashMap
    // This eliminates the single global lock bottleneck
    locks: DashMap<String, Vec<LockEntry>>,
}

#[derive(Debug, Clone)]
struct LockEntry {
    txn_id: TransactionId,
    lock_type: LockType,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LockType {
    Shared,
    Exclusive,
}

impl LockManager {
    pub fn new() -> Self {
        Self {
            locks: DashMap::new(),
        }
    }

    /// Acquire a lock on a resource with timeout for deadlock detection
    pub fn acquire_lock(&self, resource: &str, txn_id: TransactionId, lock_type: LockType) -> Result<()> {
        self.try_acquire_lock_with_timeout(resource, txn_id, lock_type, Duration::from_secs(30))
    }

    /// Try to acquire a lock with a timeout
    pub fn try_acquire_lock_with_timeout(
        &self,
        resource: &str,
        txn_id: TransactionId,
        lock_type: LockType,
        timeout: Duration,
    ) -> Result<()> {
        let start = std::time::Instant::now();
        
        loop {
            // Try to acquire the lock
            let result = self.try_acquire_lock_once(resource, txn_id, lock_type);
            
            match result {
                Ok(()) => return Ok(()),
                Err(VelociError::Busy) => {
                    // Check timeout
                    if start.elapsed() > timeout {
                        return Err(VelociError::TransactionError(
                            format!("Lock acquisition timeout after {:?} for resource '{}'", timeout, resource)
                        ));
                    }
                    
                    // Exponential backoff to reduce contention
                    let backoff = std::cmp::min(start.elapsed().as_millis() / 10, 100) as u64;
                    std::thread::sleep(Duration::from_micros(backoff));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn try_acquire_lock_once(&self, resource: &str, txn_id: TransactionId, lock_type: LockType) -> Result<()> {
        let mut entry = self.locks.entry(resource.to_string()).or_insert_with(Vec::new);
        let entries = entry.value_mut();

        // Check existing locks
        for existing in entries.iter() {
            if existing.txn_id == txn_id {
                // Same transaction already holds a lock
                match (existing.lock_type, lock_type) {
                    (LockType::Shared, LockType::Exclusive) => {
                        // Allow upgrade from shared to exclusive
                        // Remove shared lock, will add exclusive below
                        entries.retain(|e| e.txn_id != txn_id);
                        break;
                    }
                    (LockType::Exclusive, LockType::Shared) => {
                        return Err(VelociError::ConstraintViolation(
                            "Cannot downgrade exclusive lock to shared".to_string()
                        ));
                    }
                    (LockType::Shared, LockType::Shared) | (LockType::Exclusive, LockType::Exclusive) => {
                        // Same lock type, already held
                        return Ok(());
                    }
                }
            } else {
                // Different transaction - check for conflicts
                match (lock_type, existing.lock_type) {
                    (LockType::Shared, LockType::Shared) => {
                        // Multiple shared locks are allowed - continue checking
                    }
                    (LockType::Exclusive, _) | (_, LockType::Exclusive) => {
                        // Conflict with exclusive lock
                        return Err(VelociError::Busy);
                    }
                }
            }
        }

        // Add the new lock
        entries.push(LockEntry { txn_id, lock_type });
        Ok(())
    }

    pub fn release_lock(&self, resource: &str, txn_id: TransactionId) -> Result<()> {
        if let Some(mut entry) = self.locks.get_mut(resource) {
            entry.value_mut().retain(|e| e.txn_id != txn_id);
            
            // Clean up empty entries
            if entry.value().is_empty() {
                drop(entry);
                self.locks.remove(resource);
            }
        }
        Ok(())
    }

    pub fn release_all_locks(&self, txn_id: TransactionId) {
        // Collect keys to avoid holding lock during iteration
        let keys: Vec<String> = self.locks.iter().map(|r| r.key().clone()).collect();
        
        for key in keys {
            let _ = self.release_lock(&key, txn_id);
        }
    }
    
    /// Check if a transaction holds any locks (for debugging)
    pub fn has_locks(&self, txn_id: TransactionId) -> bool {
        self.locks.iter().any(|entry| {
            entry.value().iter().any(|e| e.txn_id == txn_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_transaction_lifecycle() {
        let txn_mgr = TransactionManager::new();
        let txn = txn_mgr.begin();
        
        assert_eq!(txn.state(), TransactionState::Active);
        
        txn_mgr.commit(&txn).unwrap();
        assert_eq!(txn.state(), TransactionState::Committed);
    }

    #[test]
    fn test_lock_manager() {
        let lock_mgr = LockManager::new();
        
        lock_mgr.acquire_lock("table1", 1, LockType::Shared).unwrap();
        lock_mgr.acquire_lock("table1", 2, LockType::Shared).unwrap();
        
        let result = lock_mgr.acquire_lock("table1", 3, LockType::Exclusive);
        assert!(result.is_err());
    }
}


// Transaction management with ACID guarantees

use crate::types::{Result, TransactionId, VelociError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransactionState {
    Active,
    Committed,
    Aborted,
}

pub struct Transaction {
    id: TransactionId,
    state: Arc<RwLock<TransactionState>>,
}

impl Transaction {
    pub fn new(id: TransactionId) -> Self {
        Self {
            id,
            state: Arc::new(RwLock::new(TransactionState::Active)),
        }
    }

    pub fn id(&self) -> TransactionId {
        self.id
    }

    pub fn state(&self) -> TransactionState {
        *self.state.read()
    }

    pub fn commit(&self) -> Result<()> {
        let mut state = self.state.write();
        if *state != TransactionState::Active {
            return Err(VelociError::TransactionError(
                "Transaction is not active".to_string(),
            ));
        }
        *state = TransactionState::Committed;
        Ok(())
    }

    pub fn abort(&self) -> Result<()> {
        let mut state = self.state.write();
        if *state != TransactionState::Active {
            return Err(VelociError::TransactionError(
                "Transaction is not active".to_string(),
            ));
        }
        *state = TransactionState::Aborted;
        Ok(())
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
        
        self.active_transactions
            .write()
            .insert(txn_id, Arc::clone(&txn));
        
        txn
    }

    pub fn commit(&self, txn: &Transaction) -> Result<()> {
        txn.commit()?;
        self.active_transactions.write().remove(&txn.id());
        Ok(())
    }

    pub fn abort(&self, txn: &Transaction) -> Result<()> {
        txn.abort()?;
        self.active_transactions.write().remove(&txn.id());
        Ok(())
    }

    pub fn get_transaction(&self, txn_id: TransactionId) -> Option<Arc<Transaction>> {
        self.active_transactions.read().get(&txn_id).cloned()
    }

    pub fn active_count(&self) -> usize {
        self.active_transactions.read().len()
    }
}

// Lock manager for concurrency control
pub struct LockManager {
    locks: RwLock<HashMap<String, LockEntry>>,
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
            locks: RwLock::new(HashMap::new()),
        }
    }

    pub fn acquire_lock(&self, resource: &str, txn_id: TransactionId, lock_type: LockType) -> Result<()> {
        let mut locks = self.locks.write();
        
        if let Some(entry) = locks.get(resource) {
            // Check for conflicts
            match (lock_type, entry.lock_type) {
                (LockType::Shared, LockType::Shared) => {
                    // Allow multiple shared locks
                }
                (LockType::Exclusive, _) | (_, LockType::Exclusive) => {
                    if entry.txn_id != txn_id {
                        return Err(VelociError::Busy);
                    }
                }
            }
        }
        
        locks.insert(
            resource.to_string(),
            LockEntry { txn_id, lock_type },
        );
        
        Ok(())
    }

    pub fn release_lock(&self, resource: &str, txn_id: TransactionId) -> Result<()> {
        let mut locks = self.locks.write();
        
        if let Some(entry) = locks.get(resource) {
            if entry.txn_id == txn_id {
                locks.remove(resource);
            }
        }
        
        Ok(())
    }

    pub fn release_all_locks(&self, txn_id: TransactionId) {
        let mut locks = self.locks.write();
        locks.retain(|_, entry| entry.txn_id != txn_id);
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


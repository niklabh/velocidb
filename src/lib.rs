// VelociDB library interface
// Modern embedded database with MVCC, async I/O, and vectorized execution

pub mod storage;
pub mod btree;
pub mod parser;
pub mod executor;
pub mod transaction;
pub mod types;

// Modern architecture modules
pub mod mvcc;            // Multi-Version Concurrency Control
pub mod async_io;        // Asynchronous I/O with Tokio/io_uring
pub mod lockfree;        // Lock-free data structures
pub mod simd;            // Vectorized execution with SIMD
pub mod btree_optimized; // Cache-conscious B-tree
pub mod crdt;            // CRDT-based synchronization
pub mod cloud_vfs;       // Cloud storage VFS
pub mod hybrid_storage;  // Hybrid row/columnar storage
pub mod pmem;            // Persistent memory (PMEM/DAX) support

// Re-export commonly used types
pub use storage::Database;
pub use types::{QueryResult, Value, Row, Column};

// Re-export modern features
pub use mvcc::{MvccManager, Snapshot, VersionedRecord};
pub use async_io::{AsyncPager, AsyncVfs, TokioVfs, BatchIoExecutor};
pub use lockfree::{LockFreePageCache, LockFreeIoQueue, LockFreeCounter};
pub use simd::{VectorBatch, VectorizedFilter, VectorizedAggregation};
pub use btree_optimized::{CacheOptimizedNode, CachePrefetcher};
pub use crdt::{CrdtStore, CrdtOperation, SyncProtocol};
pub use cloud_vfs::{CloudVfs, CloudVfsConfig};
pub use hybrid_storage::{HybridTable, StorageLayout, ColumnStorage};
pub use pmem::{DaxVfs, PmemTransactionLog};


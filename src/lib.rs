//! # VelociDB
//!
//! VelociDB is a high-performance, embedded database engine written in Rust.
//! It features a modern architecture designed for NVMe storage and multi-core systems.
//!
//! ## Key Features
//!
//! - **MVCC**: Multi-Version Concurrency Control for non-blocking reads.
//! - **Async I/O**: Built on `tokio` and `io_uring` (on Linux) for high throughput.
//! - **SIMD Acceleration**: Vectorized execution for query processing.
//! - **Persistent Memory**: Direct Access (DAX) support for PMEM.
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use velocidb::Database;
//!
//! # fn main() -> anyhow::Result<()> {
//! // Open a database (creates it if it doesn't exist)
//! let db = Database::open("my_database.db")?;
//!
//! // Create a table
//! db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")?;
//!
//! // Insert data
//! db.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
//! db.execute("INSERT INTO users VALUES (2, 'Bob', 25)")?;
//!
//! // Query data
//! let results = db.query("SELECT * FROM users WHERE age > 25")?;
//!
//! for row in results.rows {
//!     println!("Found user: {:?}", row.values);
//! }
//! # Ok(())
//! # }
//! ```

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


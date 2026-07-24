//! # VelociDB
//!
//! VelociDB is a high-performance, embedded database engine written in Rust.
//! It features a modern architecture designed for NVMe storage and multi-core systems.
//!
//! ## Key Features
//!
//! - **Vector search**: `F32_BLOB(n)` columns, `vector32('[...]')` literals and
//!   `vector_distance_cos/l2/dot` functions with exact, parallel KNN
//!   (`ORDER BY vector_distance_cos(...) LIMIT k`).
//! - **Async API**: Turso-style `Builder` / `AsyncDatabase` / `AsyncConnection`
//!   built on tokio (`async_api` module, `async-io` feature).
//! - **Parallel execution**: rayon-parallel WHERE filtering, ORDER BY sorting,
//!   and vector distance computation for larger row sets.
//! - **Change Data Capture**: real-time tracking of INSERT/UPDATE/DELETE with
//!   sequence numbers (`Database::enable_cdc` / `changes_since`).
//! - **Schema management**: `ALTER TABLE` ADD/DROP/RENAME COLUMN and RENAME TO.
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
pub mod wal;
pub mod vector;   // Vector search: distance metrics, literals, parallel KNN
pub mod cdc;      // Change Data Capture: real-time change tracking

#[cfg(feature = "async-io")]
pub mod async_api; // Turso-style async API (Builder / AsyncDatabase / AsyncConnection)

// EXPERIMENTAL MODULES
// ---------------------------------------------------------------------------
// The modules below are standalone implementations of advanced storage and
// concurrency techniques. They are exported for experimentation and to keep
// the engineering work visible, but they are NOT currently on the active
// SQL/storage path. The Database engine uses `storage`, `btree`, `executor`,
// `parser`, and `transaction` only.
pub mod mvcc;            // Multi-Version Concurrency Control (experimental)
pub mod async_io;        // Asynchronous I/O with Tokio/io_uring (experimental)
pub mod lockfree;        // Lock-free data structures (experimental)
pub mod simd;            // Vectorized execution with SIMD (experimental)
pub mod btree_optimized; // Cache-conscious B-tree (experimental)
pub mod crdt;            // CRDT-based synchronization (experimental)
pub mod cloud_vfs;       // Cloud storage VFS (experimental)
pub mod hybrid_storage;  // Hybrid row/columnar storage (experimental)
pub mod pmem;            // Persistent memory (PMEM/DAX) support (experimental)

// Re-export commonly used types
pub use storage::Database;
pub use types::{QueryResult, Value, Row, Column};
pub use vector::DistanceMetric;
pub use cdc::{CdcManager, ChangeEvent, ChangeOp};

#[cfg(feature = "async-io")]
pub use async_api::{AsyncConnection, AsyncDatabase, Builder};

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


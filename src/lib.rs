//! # VelociDB
//!
//! VelociDB is an embedded SQL database engine written in Rust, inspired by
//! Turso: page-based storage, a B-tree primary index, a write-ahead log with
//! crash recovery, and atomic `BEGIN` / `COMMIT` / `ROLLBACK`.
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

pub mod btree;
pub mod cdc;
pub mod executor;
pub mod parser;
pub mod storage;
pub mod transaction;
pub mod types;
pub mod vector; // Vector search: distance metrics, literals, parallel KNN
pub mod wal; // Change Data Capture: real-time change tracking

#[cfg(feature = "async-io")]
pub mod async_api; // Turso-style async API (Builder / AsyncDatabase / AsyncConnection)

// EXPERIMENTAL MODULES
// ---------------------------------------------------------------------------
// Standalone explorations of advanced storage and concurrency techniques.
// They are NOT on the SQL/storage path — the engine uses only the modules
// above — and are compiled only with `--features experimental`. See
// ROADMAP.md (P4) for whether each one graduates, is archived, or is removed.
#[cfg(feature = "experimental")]
pub mod async_io; // Asynchronous I/O with Tokio/io_uring
#[cfg(feature = "experimental")]
pub mod btree_optimized; // Cache-conscious B-tree
#[cfg(feature = "experimental")]
pub mod cloud_vfs; // Cloud storage VFS
#[cfg(feature = "experimental")]
pub mod crdt; // CRDT-based synchronization
#[cfg(feature = "experimental")]
pub mod hybrid_storage; // Hybrid row/columnar storage
#[cfg(feature = "experimental")]
pub mod lockfree; // Lock-free data structures
#[cfg(feature = "experimental")]
pub mod mvcc; // Multi-Version Concurrency Control
#[cfg(feature = "experimental")]
pub mod pmem;
#[cfg(feature = "experimental")]
pub mod simd; // Vectorized execution with SIMD // Persistent memory (PMEM/DAX) support

// Re-export commonly used types
pub use cdc::{CdcManager, ChangeEvent, ChangeOp};
pub use storage::Database;
pub use types::{Column, QueryResult, Row, Value};
pub use vector::DistanceMetric;

#[cfg(feature = "async-io")]
pub use async_api::{AsyncConnection, AsyncDatabase, Builder};

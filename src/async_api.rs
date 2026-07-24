//! Native async API, modeled on Turso's Rust binding.
//!
//! ```rust,no_run
//! use velocidb::async_api::Builder;
//!
//! # async fn example() -> velocidb::types::Result<()> {
//! let db = Builder::new_local("app.db").build().await?;
//! let conn = db.connect()?;
//!
//! conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)").await?;
//! conn.execute("INSERT INTO users VALUES (1, 'Alice')").await?;
//!
//! let rows = conn.query("SELECT * FROM users").await?;
//! println!("{} row(s)", rows.rows.len());
//! # Ok(())
//! # }
//! ```
//!
//! The engine itself is synchronous; this layer offloads every call onto the
//! tokio blocking thread pool so async tasks never stall the reactor. Because
//! readers run concurrently in the engine, multiple in-flight `query` futures
//! execute in parallel across blocking threads.

use crate::storage::Database;
use crate::types::{QueryResult, Result, Row, VelociError};
use crate::vector::DistanceMetric;
use std::path::PathBuf;
use std::sync::Arc;

/// Builder for opening a database asynchronously (Turso-style entry point).
pub struct Builder {
    path: PathBuf,
}

impl Builder {
    /// Creates a builder for a local database file.
    pub fn new_local<P: Into<PathBuf>>(path: P) -> Self {
        Self { path: path.into() }
    }

    /// Opens (or creates) the database without blocking the async runtime.
    pub async fn build(self) -> Result<AsyncDatabase> {
        let path = self.path;
        let db = run_blocking(move || Database::open(path)).await?;
        Ok(AsyncDatabase { inner: db })
    }
}

/// An asynchronously opened database. Cheap to clone; hand out one
/// [`AsyncConnection`] per task with [`AsyncDatabase::connect`].
#[derive(Clone)]
pub struct AsyncDatabase {
    inner: Arc<Database>,
}

impl AsyncDatabase {
    /// Creates a connection to this database.
    pub fn connect(&self) -> Result<AsyncConnection> {
        Ok(AsyncConnection {
            inner: Arc::clone(&self.inner),
        })
    }
}

/// A connection with async statement execution.
///
/// Connections are cheap clones of the shared database handle. Writes are
/// serialized by the engine; reads run concurrently.
#[derive(Clone)]
pub struct AsyncConnection {
    inner: Arc<Database>,
}

impl AsyncConnection {
    /// Executes a statement that returns no rows (DDL / DML).
    pub async fn execute(&self, sql: &str) -> Result<()> {
        let db = Arc::clone(&self.inner);
        let sql = sql.to_string();
        run_blocking(move || db.execute(&sql)).await
    }

    /// Executes a query and returns the result rows.
    pub async fn query(&self, sql: &str) -> Result<QueryResult> {
        let db = Arc::clone(&self.inner);
        let sql = sql.to_string();
        run_blocking(move || db.query(&sql)).await
    }

    /// Exact K-nearest-neighbour vector search (see [`Database::vector_search`]).
    pub async fn vector_search(
        &self,
        table: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: DistanceMetric,
    ) -> Result<Vec<(f64, Row)>> {
        let db = Arc::clone(&self.inner);
        let table = table.to_string();
        let column = column.to_string();
        let query = query.to_vec();
        run_blocking(move || db.vector_search(&table, &column, &query, k, metric)).await
    }

    /// Begins an explicit transaction.
    pub async fn begin(&self) -> Result<()> {
        let db = Arc::clone(&self.inner);
        run_blocking(move || db.begin()).await
    }

    /// Commits the current explicit transaction.
    pub async fn commit(&self) -> Result<()> {
        let db = Arc::clone(&self.inner);
        run_blocking(move || db.commit()).await
    }

    /// Rolls back the current explicit transaction.
    pub async fn rollback(&self) -> Result<()> {
        let db = Arc::clone(&self.inner);
        run_blocking(move || db.rollback()).await
    }

    /// Returns all CDC changes with sequence numbers greater than `since`.
    pub async fn changes_since(&self, since: u64) -> Vec<crate::cdc::ChangeEvent> {
        let db = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || db.changes_since(since))
            .await
            .unwrap_or_default()
    }

    /// Synchronous escape hatch to the underlying [`Database`].
    pub fn blocking(&self) -> &Database {
        &self.inner
    }
}

/// Runs `f` on the tokio blocking pool and flattens the join error.
async fn run_blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| VelociError::IoError(format!("blocking task failed: {}", e)))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_async_roundtrip() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Builder::new_local(tmp.path()).build().await.unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
            .await
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'Alice')").await.unwrap();

        let result = conn.query("SELECT * FROM t").await.unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[tokio::test]
    async fn test_concurrent_async_readers() {
        let tmp = NamedTempFile::new().unwrap();
        let db = Builder::new_local(tmp.path()).build().await.unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
            .await
            .unwrap();
        for i in 0..50 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, {})", i, i * 2))
                .await
                .unwrap();
        }

        // Many concurrent read futures.
        let mut handles = Vec::new();
        for _ in 0..8 {
            let c = db.connect().unwrap();
            handles.push(tokio::spawn(async move {
                c.query("SELECT * FROM t").await.unwrap().rows.len()
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), 50);
        }
    }
}

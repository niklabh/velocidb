# Changelog

All notable changes to VelociDB are documented in this file.

## [Unreleased]

### Added
- MVCC with snapshot isolation for non-blocking concurrent reads/writes.
- Async I/O layer with Tokio runtime and optional `io_uring` backend.
- Lock-free page cache and I/O queues via `crossbeam`.
- SIMD-accelerated filtering and aggregation (AVX2 / AVX-512 / NEON).
- Cache-optimised B-Tree with 64-byte-aligned nodes.
- Hybrid row/columnar storage with adaptive layout switching.
- CRDT-based offline-first synchronisation (`crdt-sync` feature).
- Cloud VFS backed by S3, Azure, or GCS (`cloud-vfs` feature).
- Persistent memory VFS with DAX support (`pmem-support` feature).
- Interactive REPL with SQL command history.
- Library API with `Database::open`, `execute`, and `query`.

### Known limitations
- B-Tree node splitting incomplete; capacity limited to ~4,096 records.
- SQL support limited to core subset (no JOINs, GROUP BY, ORDER BY, LIMIT).
- REPL lacks multi-line statements, command history, and tab completion.

## [0.1.0] — 2025-05-19

### Added
- Initial public release.
- SQL parser supporting CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, DROP TABLE.
- Query executor with WHERE clause filtering and COUNT(\*) aggregation.
- File-backed pager with 4 KB pages.
- B-Tree primary key index.
- ACID transaction manager with two-phase locking.
- Core type system: `Value`, `Row`, `Column`, `QueryResult`, `VelociError`.

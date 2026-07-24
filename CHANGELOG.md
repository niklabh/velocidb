# Changelog

All notable changes to VelociDB are documented in this file.

## [Unreleased]

### Added (Turso-inspired features)

- **Vector search** (`src/vector.rs`). Vector columns via `F32_BLOB(n)` /
  `VECTOR(n)`, `vector32('[...]')` literals in INSERT, and distance
  functions `vector_distance_cos`, `vector_distance_l2`,
  `vector_distance_dot` usable both in SELECT projections and in
  `ORDER BY ... LIMIT k` (exact KNN, top-k selection instead of a full
  sort). Dimension is enforced on INSERT and persisted in the schema.
  Also exposed as `Database::vector_search(table, column, query, k, metric)`.
- **Async API** (`src/async_api.rs`, `async-io` feature, on by default).
  Turso-style `Builder::new_local(path).build().await`,
  `AsyncDatabase::connect()`, and `AsyncConnection` with async `execute`,
  `query`, `vector_search`, `begin`/`commit`/`rollback`, and
  `changes_since`. Calls run on the tokio blocking pool so async tasks
  never stall the reactor; concurrent read futures execute in parallel.
- **Parallel query execution.** WHERE filtering, ORDER BY sorting and
  vector distance computation switch to rayon once a query touches
  ≥ 1024 rows.
- **Change Data Capture** (`src/cdc.rs`). `Database::enable_cdc()` starts
  recording every committed INSERT / UPDATE / DELETE as a `ChangeEvent`
  (sequence number, table, op, rowid, before/after row images).
  Poll with `Database::changes_since(seq)`. REPL: `.cdc on|off`, `.changes`.
- **ALTER TABLE**: `RENAME TO`, `RENAME COLUMN a TO b`, `ADD COLUMN`
  (existing rows padded with NULL), `DROP COLUMN` (rows rewritten;
  dropping the primary key is rejected). Schema changes persist across
  reopen.
- Integration test suite `tests/advanced_features_tests.rs` (13 tests)
  covering all of the above.
- **Agent skills** (`.claude/skills/`, modeled on Turso's): storage-format,
  transaction-correctness, async-io-model, vector-search, cdc, sql-parser,
  testing, code-quality, and debugging — codebase knowledge for AI coding
  agents (Claude Code, Cursor).

### Changed

- `src/main.rs` now builds against the `velocidb` library crate instead of
  re-declaring the module tree.
- `Executor::new` takes a `CdcManager`; `Statement` gained `AlterTable`.

### Added

- **Write-ahead log** (`src/wal.rs`) with CRC32 records, group-commit
  semantics, and replay on open. Each write statement runs as one atomic
  WAL group: pages are buffered in `Pager::pending`, fsynced via the WAL
  COMMIT record, applied to the data file, fsynced, then the WAL is
  truncated. Recovery skips torn / uncommitted records.
- **Crash-recovery integration tests** (`tests/recovery_tests.rs`) covering
  clean re-open, WAL truncation after clean commits, torn-tail handling,
  uncommitted-group skipping, and exact data-value persistence across many
  splits.
- `ORDER BY <col> [ASC|DESC]` and `LIMIT n` in `SELECT`.
- REPL: `rustyline`-backed line editor with persistent history,
  multi-line statements terminated by `;`, and `.schema [name]` meta
  command.
- B-tree internal-node underflow handling (`merge_internal`,
  `redistribute_internal`) — previously a no-op stub. New stress test
  (2,000 inserts + every-3rd delete) and a proptest of random
  insert/delete sequences validate the invariant.
- `Database::describe_table` for schema-as-SQL rendering.
- B-tree root-page tracking: schema is re-saved whenever an INSERT/UPDATE/
  DELETE causes a root change (e.g., split that promotes a new root), so
  the on-disk schema always points to the live root.

### Changed

- `Pager::write_page` no longer fsyncs the data file per call. Durability
  is provided by the WAL fsync on commit; the data file is fsynced once
  per WAL group commit. This is a significant write-amp reduction for any
  operation that touches multiple pages (B-tree splits in particular).
- Removed dual-write from the auto-commit executor path. The B-tree is
  the single source of truth for committed data; MVCC reads/writes are
  no longer interleaved with B-tree scans (the merge previously used a
  `HashMap`, which produced non-deterministic results on key collisions).
- The `mvcc`, `async_io`, `simd`, `lockfree`, `btree_optimized`, `crdt`,
  `cloud_vfs`, `hybrid_storage`, and `pmem` modules are now explicitly
  marked as experimental and not on the active SQL path in `src/lib.rs`
  and in the README.
- B-tree parent-pointer updates (`create_new_root`,
  `insert_into_internal`, `split_internal_node`, plus the new
  underflow paths) now go through `pager.write_page` via the
  `set_parent_pointer` helper so they land in the WAL and on disk.
  Previously they mutated the cache directly and were lost on eviction.
- The integration `test_persistence` is kept but a much stricter
  equivalent (`test_persistence_strong_assertions` in
  `tests/recovery_tests.rs`) asserts exact values for 500 rows across
  splits and reopens.

### Fixed

- B-tree deletes that trigger internal-node underflow no longer leave the
  tree silently unbalanced.
- Cache mutations during splits / underflow are now durable via the WAL,
  not just held in the read cache until eviction.

### Known limitations

- All writers serialize on a `Database`-level mutex.
- Explicit-transaction `ROLLBACK` only releases locks; storage mutations
  made by previous statements in the transaction are not undone (each
  statement is its own WAL group).
- No `JOIN`, `GROUP BY`, sub-queries, or composite primary keys.
- Vector search is exact (brute-force, parallel); approximate indexing
  (HNSW/DiskANN-style) is future work, mirroring Turso's roadmap.

## [0.1.0] — 2025-05-19

### Added

- Initial public release.
- SQL parser supporting CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, DROP TABLE.
- Query executor with WHERE clause filtering and COUNT(*) aggregation.
- File-backed pager with 4 KB pages.
- B-Tree primary key index.
- ACID transaction manager with two-phase locking.
- Core type system: `Value`, `Row`, `Column`, `QueryResult`, `VelociError`.

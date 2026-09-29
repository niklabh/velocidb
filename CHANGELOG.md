# Changelog

All notable changes to VelociDB are documented in this file.

## [Unreleased]

### Fixed (P0 correctness)

- **Recovery could strand later commits.** If the WAL contained only a torn
  or uncommitted tail, recovery left it in place and new commits were
  appended after it; a crash before those commits reached the data file
  lost them, because the next recovery stopped at the garbage. Recovery now always resets the WAL, and a failed WAL append is
  truncated back to the last committed group.

- **`ROLLBACK` now undoes storage.** `BEGIN` opens a single WAL group that
  spans every statement until `COMMIT`; `ROLLBACK` (or closing the database
  without committing) discards all of it, including schema changes. Pages
  are written to the WAL only at commit, so an uncommitted transaction never
  reaches disk.
- **Statement-level atomicity inside transactions.** Each statement runs
  under a pager savepoint; a failing statement undoes only its own writes
  and the transaction continues (previously the failure aborted the
  transaction object and released its locks mid-transaction).
- **Schema writes are atomic with the statement.** DDL and B-tree root
  changes are saved inside the statement's WAL group instead of a separate
  group afterwards. After any rollback the in-memory schema and B-tree roots
  are rebuilt from storage.
- **`UNIQUE` is enforced** on non-primary-key columns for INSERT and
  UPDATE (including several rows updated to the same value). NULLs never
  conflict.
- **CDC only publishes committed changes.** Events are staged per write
  group and published on commit; rolled-back or failed statements emit
  nothing, and sequence numbers have no gaps.
- **Table locks no longer leak on errors.** A failing auto-commit statement
  (e.g. `ORDER BY` an unknown column, missing primary key value) used to
  keep its table lock, stalling the next conflicting statement for the 30 s
  lock timeout. Lock/transaction lifecycle now lives in one wrapper.
- `SELECT` after a write inside an explicit transaction no longer fails
  with "Cannot downgrade exclusive lock to shared".
- An aborted write group now restores the pager's page count, so pages
  allocated by the aborted group are not left referenced past end-of-file.

### Performance

- **One fsync per commit (WAL checkpointing).** A commit appends its pages
  and COMMIT record to the WAL in one write and fsyncs once. Committed pages
  are served from memory until a checkpoint (at 4 MiB of WAL, and on close)
  applies them to the data file. Previously every commit fsynced the WAL,
  applied pages, fsynced the data file, then truncated and fsynced the WAL.
  Auto-commit INSERT on macOS: ~74 → ~245 rows/s.
- **Primary-key lookups.** `WHERE <pk> = <integer>` (alone or with other
  ANDed conditions) uses `BTree::search` instead of a full scan for SELECT,
  UPDATE and DELETE: ~850 → ~193,000 queries/s on a 2,000-row table.
- **Parser regexes are compiled once** instead of on every statement.
  Batched inserts went from ~4,600 to ~49,800 rows/s and non-key filtered
  SELECTs roughly doubled. Numbers and method are in `docs/performance.md`.

### Changed

- **Experimental modules are behind the `experimental` feature** (off by
  default): `mvcc`, `async_io`, `lockfree`, `simd`, `btree_optimized`,
  `crdt`, `cloud_vfs`, `hybrid_storage`, `pmem`. They are no longer compiled
  or re-exported from the crate root by default. **Breaking** for anyone
  importing e.g. `velocidb::MvccManager` — enable `experimental` and use the
  module path (`velocidb::mvcc::MvccManager`).
- Docs describe the engine that exists: `docs/architecture.md`,
  `docs/quickstart.md` and `docs/performance.md` are rewritten (the old
  versions presented experimental modules as integrated and quoted
  unmeasured speedups); `docs/implementation.md` is replaced by
  `docs/experimental.md`. `performance.md` now has a measured baseline.
- REPL / `--version` print the crate version instead of a hard-coded
  `v0.1.0`.
- Active-path code is clippy-clean (`cargo clippy --all-targets -- -D warnings`).

### Added

- CI (`.github/workflows/ci.yml`): tests on Linux and macOS, doc tests,
  clippy with `-D warnings`, and a build + unit-test job for
  `--features experimental`.
- Five crash tests in `tests/recovery_tests.rs` (simulated with
  `mem::forget`): uncheckpointed commits survive, uncommitted transactions
  are lost, the WAL stays bounded by checkpoints, commits after a torn-tail
  recovery survive, repeated crashes. Plus `append_group` unit tests.
- `tests/transaction_tests.rs` (12 tests): rollback of DML and DDL, commit
  and reopen, uncommitted-on-close, rollback across B-tree splits,
  savepoints, CDC publication, `UNIQUE` on insert/update/reopen.

## [0.3.0] — 2026-07-24

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
- No `JOIN`, `GROUP BY`, sub-queries, or composite primary keys.
- Vector search is exact (brute-force, parallel); approximate indexing
  (HNSW/DiskANN-style) is future work, mirroring Turso's roadmap.

## [0.2.0] — 2026-05-19

### Fixed

- B-tree delete underflow: leaves redistribute from or merge with a
  sibling (left or right); internal-node underflow promotes / demotes the
  root. `split_leaf_node` no longer rewrites the left page mid-split.
- `find_sibling_info` corruption is reported as `VelociError::Corruption`
  instead of silently leaving the tree underflowed.
- Page-cache size accounting races (double decrements, missing increments
  on `write_page`).
- Executor propagates transaction commit / abort errors instead of
  discarding them.

## [0.1.0] — 2025-11-19

### Added

- Initial public release.
- SQL parser supporting CREATE TABLE, INSERT, SELECT, UPDATE, DELETE, DROP TABLE.
- Query executor with WHERE clause filtering and COUNT(*) aggregation.
- File-backed pager with 4 KB pages.
- B-Tree primary key index.
- ACID transaction manager with two-phase locking.
- Core type system: `Value`, `Row`, `Column`, `QueryResult`, `VelociError`.

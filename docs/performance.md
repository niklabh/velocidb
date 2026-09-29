# Performance

This page describes what the engine actually does for performance today,
what it costs, and how to measure it. It makes no claims beyond what a
checked-in benchmark or the measurement below supports.

## Measured baseline

Measured on an Apple M4 Max (APFS, internal SSD) with a release build and
2,000 rows of `(id INTEGER PRIMARY KEY, v INTEGER, s TEXT)`:

| Workload | Result |
|----------|--------|
| INSERT, auto-commit (one statement per write group) | ~79 rows/s |
| INSERT, all 2,000 inside one `BEGIN` … `COMMIT` | ~49,800 rows/s |
| `SELECT * FROM t WHERE id = ?` (primary-key lookup) | ~193,000 queries/s |
| `SELECT * FROM t WHERE v = ?` (non-key column, scans 2,000 rows) | ~1,800 queries/s |

Before the fixes in the Unreleased changelog (regexes compiled on every parse,
no primary-key lookups), the same machine measured ~4,600 rows/s batched and
~850 queries/s for a primary-key lookup.

Treat these as a baseline, not a target. They are hardware- and
filesystem-dependent. macOS `fsync` (`F_FULLFSYNC`) is much slower than
Linux `fsync` on most SSDs.

### Why auto-commit writes are slow

Each commit issues **three fsyncs**: the WAL after the COMMIT record, the data
file after the pages are applied, and the WAL again after it is truncated
(truncation must be durable, or stale committed groups could be replayed
over newer data). Fsync latency dominates small writes.

**Batch writes in an explicit transaction.** All statements then share one
write group: one set of fsyncs, and each page is written to the WAL once
however many times the transaction touched it. That is the ~600× gap above.

Reducing per-commit fsyncs (checkpointing the WAL instead of truncating on
every commit, as SQLite's WAL mode does) is on the roadmap.

### Lookups and scans

A `WHERE` clause containing `<primary key> = <integer>` (alone or ANDed
with other conditions) is answered with a B-tree lookup, for SELECT, UPDATE
and DELETE. Any other `WHERE` scans the table and filters: there are no
secondary indexes yet (roadmap P1).

## What the engine does today

- **Cached SQL patterns.** The parser's regexes are compiled once per
  process (`regex!` in `src/parser.rs`), not per statement.
- **Primary-key lookups** for `WHERE pk = <integer>`.
- **Buffered writes.** Pages modified in a write group stay in memory
  (`Pager::pending`) and hit the WAL once, at commit.
- **Read cache.** A bounded `DashMap` of up to 1,024 pages (`CACHE_SIZE`).
  Eviction picks an arbitrary entry, not LRU.
- **Parallel execution.** For inputs of 1,024 rows or more
  (`PARALLEL_THRESHOLD`), WHERE filtering, ORDER BY sorting and vector
  distance computation run on rayon. Below that they run sequentially.
- **Top-k for KNN.** `ORDER BY vector_distance_*(…) LIMIT k` selects the k
  best rows instead of sorting everything.
- **Async offload.** The async API runs each call on tokio's blocking pool,
  so async callers never block the reactor on disk I/O.
- **Release profile.** `opt-level = 3`, fat LTO, `codegen-units = 1`.

The experimental modules (SIMD kernels, lock-free cache, io_uring, …) are
**not** used by the engine and have no measured impact. See
[experimental.md](experimental.md).

## Measuring

Criterion benchmarks live in `benches/benchmarks.rs` (insert, select,
select-with-where, update, delete, mixed workload):

```bash
cargo bench --bench benchmarks
```

When a change is motivated by performance, include before/after numbers from
these benchmarks (plus machine and OS) in the PR.

Profiling:

```bash
cargo build --release
# Linux
perf record -g target/release/velocidb && perf report
# macOS
cargo install samply && samply record target/release/velocidb
```

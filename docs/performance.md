# Performance

This page describes what the engine actually does for performance today,
what it costs, and how to measure it. It makes no claims beyond what a
checked-in benchmark or the measurement below supports.

## Measured baseline

Measured on an Apple M4 Max (APFS, internal SSD) with a release build and
2,000 rows of `(id INTEGER PRIMARY KEY, v INTEGER, s TEXT)`:

| Workload | Result |
|----------|--------|
| INSERT, auto-commit (one statement per write group) | ~245 rows/s |
| INSERT, all 2,000 inside one `BEGIN` … `COMMIT` | ~60,000–68,000 rows/s |
| `SELECT * FROM t WHERE id = ?` (primary-key lookup) | ~160,000–190,000 queries/s |
| `SELECT * FROM t WHERE v = ?` (non-key column, scans 2,000 rows) | ~1,800 queries/s |

Before the changes in the Unreleased changelog, the same machine measured
~74 rows/s auto-commit, ~4,600 rows/s batched and ~850 queries/s for a
primary-key lookup. The causes were three fsyncs per commit, regexes
compiled on every parse, and no primary-key lookups.

Treat these as a baseline, not a target. They are hardware- and
filesystem-dependent. macOS `fsync` (`F_FULLFSYNC`) is much slower than
Linux `fsync` on most SSDs.

### Commit cost: one fsync

A commit appends the group's page images and a COMMIT record to the WAL in a
single write and issues **one fsync**. Committed pages are served from
memory until a **checkpoint** copies them into the data file, fsyncs it, and
truncates the WAL. Checkpoints run when the WAL reaches 4 MiB
(`CHECKPOINT_WAL_BYTES`) and on close, so their fsyncs are amortized over
many commits. (Before checkpointing, every commit paid three fsyncs.)

That one fsync still dominates small writes. On macOS, Rust's
`File::sync_data` issues `F_FULLFSYNC`, which flushes the drive's write
cache: the only fsync macOS guarantees survives power loss, and it takes a
few milliseconds. VelociDB keeps this full durability. For comparison,
SQLite on macOS uses a plain `fsync` unless `PRAGMA fullfsync` is set.
Linux `fdatasync` is typically much cheaper.

**Batch writes in an explicit transaction.** All statements then share one
write group and one fsync, and each page goes to the WAL once however many
times the transaction touched it. That is the ~250× gap above.

### Lookups and scans

A `WHERE` clause containing `<primary key> = <integer>` (alone or ANDed
with other conditions) is answered with a B-tree lookup, for SELECT, UPDATE
and DELETE. Otherwise a condition `<indexed column> = <value>` is answered
from a secondary index; any other `WHERE` scans the table and filters.

20,000 rows `(id INTEGER PRIMARY KEY, email TEXT, grp INTEGER)`, `grp`
cycling through 100 values; release build, Apple Silicon, 200 queries each:

| Query | Scan | Indexed |
|-------|------|---------|
| `WHERE email = 'user12345@x'` (1 row) | 1,198 µs | 2.3 µs |
| `WHERE grp = 42` (200 rows) | 1,286 µs | 60 µs |

Building both indexes over the 20,000 rows took 182 ms. An index costs one
extra B-tree insert per INSERT and a delete + insert per UPDATE that changes
the indexed value.

## What the engine does today

- **Single-pass parsing.** The parser tokenizes each statement once and
  parses it by recursive descent; no regexes are involved.
- **Primary-key lookups** for `WHERE pk = <integer>`, and **secondary
  index lookups** for `WHERE <indexed col> = <value>`.
- **Buffered writes.** Pages modified in a write group stay in memory
  (`Pager::pending`) and hit the WAL once, at commit, in a single write.
- **Checkpointing.** Committed pages are served from memory
  (`Pager::committed`) until a checkpoint moves them into the data file.
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

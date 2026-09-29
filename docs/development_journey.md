# Development Journey & Architecture Decisions

## The Vision

VelociDB set out to be an embedded SQL database in Rust that keeps SQLite's
simplicity and can grow into features from Turso: vector search, change
data capture, and a native async API.

The early releases explored a lot of advanced techniques at once: MVCC,
io_uring, SIMD kernels, lock-free caches, CRDTs, cloud and persistent-memory
VFSs. Most of that work was never wired into the SQL engine. From 0.3 the
project draws a hard line: the **active path** (pager, WAL, B-tree, parser,
executor) must be correct and tested before anything else joins it. The
research code lives behind the `experimental` feature
([experimental.md](experimental.md)).

## Key Decisions

### Rust

Memory safety without a garbage collector, `parking_lot` locks, and rayon /
tokio for parallel and async execution. It lets a small codebase take on
concurrency without the usual class of C/C++ memory bugs.

### B-tree, not LSM

A B-tree keyed by the integer primary key gives predictable reads and
in-place updates, and maps directly onto 4 KB pages. An LSM tree would
favour write-heavy workloads, at the cost of compaction complexity that
isn't justified for an embedded engine.

### No-steal WAL with buffered write groups

Modified pages stay in memory until commit, then go to the WAL (fsync)
before the data file. This makes abort trivial: nothing on disk needs
undoing. Explicit transactions and statement-level savepoints come almost
for free. The cost is that a transaction's dirty pages must fit in memory,
and each commit pays several fsyncs (see [performance.md](performance.md)).

### Correctness before concurrency

Writers are serialized by one mutex, and a transaction is database-wide.
MVCC is the obvious next step for reader isolation, and there is an
experimental implementation. It will only graduate with integration and
crash-recovery tests behind it.

## Challenges & Lessons Learned

### B-tree splits and underflow

Leaf splits were straightforward. Propagating splits and underflow through
internal nodes, while keeping parent pointers durable, took several rounds.
Early versions mutated cached pages directly, and those changes were lost on
eviction. Every page mutation now goes through `Pager::write_page`, and a
proptest exercises random insert/delete sequences.

### ROLLBACK that only released locks

For several releases `ROLLBACK` released locks without undoing the earlier
statements in the transaction, because each statement was its own WAL group.
The fix buffers a whole transaction in one group and logs it only at commit.
It also exposed leaked table locks and in-memory schema drift after failed
statements.

### Docs that ran ahead of the code

Older docs described experimental modules as integrated and quoted speedups
that were never measured. The rule now: a feature is documented as working
only when it is on the active path with tests, and a performance claim needs
a checked-in benchmark.

## Future Directions

See [ROADMAP.md](../ROADMAP.md): a real SQL parser, secondary indexes,
JOIN / GROUP BY, fewer fsyncs per commit, durable CDC, and an approximate
vector index.

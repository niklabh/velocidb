# Experimental Modules

VelociDB's source tree contains nine research modules that explore advanced
storage and concurrency techniques. **None of them are used by the SQL
engine.** `Database`, the executor, the B-tree and the WAL work without
them, and no benchmark numbers exist for them.

They compile only with the `experimental` feature:

```bash
cargo test --lib --features experimental
```

Under that feature they are public (`velocidb::mvcc`, `velocidb::simd`, …),
but their APIs are unstable and may change or be deleted. CI checks that they
keep compiling and that their unit tests pass. Their warnings are tolerated.

| Module | What it contains | Unit tests | Status / direction |
|--------|------------------|-----------:|--------------------|
| `mvcc` | Versioned records, snapshots, vacuum | 5 | Strongest candidate to graduate for reader isolation, now that `ROLLBACK` is real |
| `simd` | AVX2 / NEON filter and aggregation kernels over column batches | 7 | Candidate for filter / aggregate hot paths once `GROUP BY` exists, behind a feature |
| `btree_optimized` | Cache-line-aligned node layout, SIMD key search | 4 | Merge useful ideas into `btree.rs`, or drop |
| `lockfree` | Lock-free page cache, MPSC I/O queue, counters | 6 | Prove with benchmarks or fold into `Pager` |
| `async_io` | `AsyncVfs` trait and a tokio-file implementation. The `io-uring` feature pulls in `tokio-uring`, but no io_uring backend is implemented | 0 | Park until needed; the async API already uses `spawn_blocking` |
| `hybrid_storage` | Row / columnar table layout with adaptive switching | 4 | Research |
| `crdt` | Operation-based CRDT store with Lamport timestamps | 5 | Research |
| `cloud_vfs` | `object_store`-backed VFS (feature `cloud-vfs`), stubs otherwise | 0 (placeholder) | Research |
| `pmem` | DAX / persistent-memory VFS and log (feature `pmem-support`) | 0 | Research |

## Graduation rules

A module moves onto the active path only when:

1. It is wired into `Database` with no path back to experimental code.
2. It has integration tests, and crash-recovery tests if it touches durability.
3. Any performance claim is backed by a checked-in criterion benchmark.
4. `src/lib.rs`, the README and [architecture.md](architecture.md) are
   updated in the same change.

See [ROADMAP.md](../ROADMAP.md), P4, for the per-module decision.

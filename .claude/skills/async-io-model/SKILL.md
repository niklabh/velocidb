---
name: async-io-model
description: How VelociDB's async API and parallel query execution work - the tokio spawn_blocking model in src/async_api.rs and the rayon thresholds in the executor. Use when modifying async_api.rs, adding async methods, tuning parallelism, or debugging blocked runtimes, stalled futures, or async test hangs.
---

# Async & Parallelism Model

## Architecture: sync engine, async facade

The engine (`Database`, `Pager`, `BTree`) is fully synchronous and uses
parking_lot locks. The async API (`src/async_api.rs`, feature `async-io`,
enabled by default) is a facade modeled on Turso's Rust binding:

```text
Builder::new_local(path).build().await  ->  AsyncDatabase
AsyncDatabase::connect()                ->  AsyncConnection (cheap Arc clone)
AsyncConnection::{execute, query, vector_search, begin/commit/rollback}.await
```

Every async method clones the `Arc<Database>`, moves owned arguments into a
closure, and runs it via `tokio::task::spawn_blocking` (the `run_blocking`
helper flattens the JoinError into `VelociError::IoError`).

Consequences:

- **Never call engine methods directly on the async runtime.** Any new async
  method must go through `run_blocking`. Blocking a reactor thread on
  parking_lot locks or file I/O stalls all futures.
- Arguments must be owned/`'static` (`String`, `Vec<f32>`) before moving into
  the closure.
- Concurrent `query` futures genuinely run in parallel (engine readers are
  concurrent). Concurrent `execute` futures serialize on the engine's writer
  mutex — that is correct, don't work around it.
- `AsyncConnection::blocking()` is the sync escape hatch; fine in tests.

## Parallel query execution (rayon)

Inside the executor (`src/executor.rs`) and vector module (`src/vector.rs`),
work switches from sequential to rayon when the row count reaches
`PARALLEL_THRESHOLD = 1024` (one constant in each file — keep them equal):

- WHERE filtering: `into_par_iter().filter(...)`
- ORDER BY column sort: `par_sort_by`
- Vector distance computation: `par_iter().map(distance)`

Rules:

- Closures used in `par_iter` must not take pager/btree locks — all
  parallelism operates on rows already scanned out of the B-tree.
- KNN (`ORDER BY vector_distance_* ... LIMIT k`, ascending) uses
  `select_nth_unstable_by` top-k, not a full sort. Keep that property.

## Feature gating

`async_api` and its re-exports (`Builder`, `AsyncDatabase`, `AsyncConnection`)
are behind `#[cfg(feature = "async-io")]` in `src/lib.rs`. New async-only code
must be gated the same way; `cargo check --no-default-features` should still
compile.

## Testing async code

Use `#[tokio::test]` (tokio "full" is available through the default feature).
See `tests/turso_features_tests.rs::test_async_concurrent_writers_and_readers`
for the concurrency-test pattern: spawn tasks with their own connections,
join all, then assert counts.

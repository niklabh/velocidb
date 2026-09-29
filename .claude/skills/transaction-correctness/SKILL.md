---
name: transaction-correctness
description: Durability and concurrency invariants for VelociDB - write groups, commit protocol, lock ordering, and the writer mutex. Use when modifying any write path (executor INSERT/UPDATE/DELETE/ALTER, Pager, WalManager, Database::execute), adding statements, or investigating deadlocks, lost writes, or recovery failures.
---

# Transaction Correctness

## The commit protocol (never reorder these steps)

Every auto-commit write statement, and every explicit `BEGIN` … `COMMIT`
transaction, is one atomic WAL group (`src/storage.rs`):

1. `Database::execute` takes the `writer` mutex (serializes all writers).
2. Auto-commit: `pager.begin_group()`. Inside an explicit transaction the
   group was opened by `Database::begin`; the statement instead runs under
   `pager.begin_savepoint()`, which records each touched page's prior
   `pending` state. `write_page` only buffers into `Pager::pending` — nothing
   touches the WAL or data file yet.
3. Executor runs the statement; `save_schema` runs **inside the same group**
   if the statement was DDL or changed a B-tree root.
4. On success: auto-commit → `pager.commit_group()`:
   a. PAGE_WRITE record for every pending page + COMMIT record, **fsync WAL**
      (durability point; if this fails the group is aborted),
   b. apply pending pages to the data file,
   c. **fsync data file**,
   d. truncate WAL.
   In a transaction → `release_savepoint()`; `Database::commit` later runs
   `commit_group()` for the whole transaction.
5. On error: `abort_group()` (auto-commit) or `rollback_to_savepoint()` (in a
   transaction), then `reload_schema()` — the statement may have mutated
   in-memory B-tree roots / columns before failing. `Database::rollback`
   is `abort_group()` + `reload_schema()`.
6. CDC events are `stage`d by the executor and published by `Database` only
   after `commit_group` succeeds; rollbacks `discard_staged_from(mark)`.

Invariants:

- The WAL fsync in 4a MUST happen before any data-file mutation.
- The WAL is truncated only after the data file is fsynced.
- Only one write group can be active; the `writer` mutex guarantees it.
  `Pager::begin_group` errors if a group is already active — if you see this,
  a code path is writing without holding the writer mutex.

## Locking rules

- **Never hold `pager.write()` across an executor call** — the executor takes
  the pager lock internally many times; parking_lot locks are not reentrant
  and this deadlocks. The active group survives lock release because it is
  Pager state, not a guard.
- Lock ordering: `LockManager` (per-table, `src/transaction.rs`) before
  `schema` / `btrees`. `TransactionManager::active_transactions` before any
  per-transaction state (which is atomic anyway).
- Table locks: readers take `LockType::Shared`, writers `LockType::Exclusive`.
  Acquisition has a 30 s timeout with backoff as crude deadlock detection.
- Clone what you need out of `schema.read()` and drop the guard before doing
  B-tree work (the existing executor methods follow this pattern).

## Adding a new write statement

Checklist:

- Route it through `Database::execute` so it runs inside a WAL group and the
  writer mutex.
- If it can change a B-tree root or the schema, make sure
  `needs_schema_save` in `Database::execute` matches it (CREATE/DROP/ALTER
  already do) or that root-diffing catches it.
- Use `Executor::with_table_lock` for lock + transaction lifecycle; it
  ends auto-commit transactions and releases their locks on every path.
- Stage CDC events with `cdc.stage(...)` only after the statement succeeded
  (see cdc skill); `Database` publishes them on commit.

## Known gaps (do not "fix" silently — they are documented behavior)

- A transaction is database-wide (one session per `Database`); other threads'
  statements join it and readers see its uncommitted pages.
- A transaction's dirty pages live in `Pager::pending` until COMMIT, so a very
  large transaction is bounded by memory.
- MVCC (`src/mvcc.rs`) is experimental and NOT on the active path. The B-tree
  is the single source of truth for committed data.

## Tests to run after touching write paths

```bash
cargo test --test recovery_tests
cargo test --test transaction_tests
cargo test --lib btree
cargo test --test integration_tests
```

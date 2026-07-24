---
name: transaction-correctness
description: Durability and concurrency invariants for VelociDB - write groups, commit protocol, lock ordering, and the writer mutex. Use when modifying any write path (executor INSERT/UPDATE/DELETE/ALTER, Pager, WalManager, Database::execute), adding statements, or investigating deadlocks, lost writes, or recovery failures.
---

# Transaction Correctness

## The commit protocol (never reorder these steps)

Every write statement is one atomic WAL group (`src/storage.rs`):

1. `Database::execute` takes the `writer` mutex (serializes all writers).
2. `pager.begin_group()` — allocates a group id; all subsequent `write_page`
   calls append PAGE_WRITE records to the WAL and buffer pages in
   `Pager::pending` (no-steal: nothing touches the data file yet).
3. Executor runs the statement.
4. On success `pager.commit_group()`:
   a. WAL COMMIT record + **fsync WAL** (durability point),
   b. apply pending pages to the data file,
   c. **fsync data file**,
   d. truncate WAL.
5. On error `pager.abort_group()` — drops `pending` and evicts those pages
   from the cache. Orphan WAL records are ignored by recovery (no COMMIT).

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
- Release table locks and abort the transaction on every error path (see
  `execute_insert` for the pattern).
- Record CDC events only after the statement succeeded (see cdc skill).

## Known gaps (do not "fix" silently — they are documented behavior)

- Explicit `ROLLBACK` releases locks but does NOT undo storage mutations of
  earlier statements in the transaction; each statement is its own WAL group.
- MVCC (`src/mvcc.rs`) is experimental and NOT on the active path. The B-tree
  is the single source of truth for committed data.

## Tests to run after touching write paths

```bash
cargo test --test recovery_tests
cargo test --lib btree
cargo test --test integration_tests
```

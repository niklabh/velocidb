# VelociDB Architecture

This document describes the engine as it exists in the source tree today —
the **active SQL path**. Research modules that are not wired into the engine
(MVCC, SIMD kernels, lock-free cache, io_uring, CRDT, cloud VFS, PMEM, hybrid
storage) are covered separately in [experimental.md](experimental.md); nothing
below depends on them.

## Layers

```text
            REPL (src/main.rs)        Library users
                    │                   │        │
                    ▼                   ▼        ▼
            ┌──────────────────────────────┐  ┌───────────────────────────┐
            │ Database  (src/storage.rs)   │◄─┤ AsyncConnection           │
            │  execute / query / begin /   │  │ (src/async_api.rs, tokio  │
            │  commit / rollback, writer   │  │  spawn_blocking)          │
            │  mutex, schema, CDC          │  └───────────────────────────┘
            └──────────────┬───────────────┘
                           │  Statement (AST)
     Parser (src/parser.rs)│
                           ▼
            ┌──────────────────────────────┐   ┌──────────────────────────┐
            │ Executor (src/executor.rs)   │──►│ LockManager /            │
            │  DML, DDL, SELECT, UNIQUE,   │   │ TransactionManager       │
            │  rayon filter/sort, KNN      │   │ (src/transaction.rs)     │
            └──────────────┬───────────────┘   └──────────────────────────┘
                           │ rows by primary key
                           ▼
            ┌──────────────────────────────┐
            │ BTree  (src/btree.rs)        │  one per table
            └──────────────┬───────────────┘
                           │ pages
                           ▼
            ┌──────────────────────────────┐   ┌──────────────────────────┐
            │ Pager  (src/storage.rs)      │──►│ WalManager (src/wal.rs)  │
            │  pending buffer, savepoints, │   │  <db>-wal, CRC32 records │
            │  DashMap read cache          │   └──────────────────────────┘
            └──────────────┬───────────────┘
                           ▼
                     <db> data file (4 KB pages)
```

## Storage: pages and the pager

- The data file is an array of 4 KB pages (`PAGE_SIZE`). Page 0 is reserved,
  pages 1..N hold the schema (a chained byte stream), and every other page is
  one B-tree node. Exact byte layouts are in
  [`.claude/skills/storage-format`](../.claude/skills/storage-format/SKILL.md).
- `Pager::read_page` looks in the active write group's `pending` buffer
  first, then the read cache (a bounded `DashMap`, evicting an arbitrary
  entry when full), then `committed` (pages committed to the WAL but not yet
  checkpointed), then the file. A cached page always equals its newest
  version, because aborts and savepoint rollbacks evict what they discard.
- `Pager::write_page` never touches the file: it buffers the page in
  `pending` and refreshes the cache.

## Durability: write groups and the WAL

Every change is part of a **write group**:

| Caller | Group spans |
|--------|-------------|
| Auto-commit statement (`db.execute(...)` outside a transaction) | that one statement |
| Explicit transaction (`BEGIN` … `COMMIT`) | every statement until COMMIT / ROLLBACK |

Commit (`Pager::commit_group`) is the only place the WAL is written:

1. Append one PAGE_WRITE record per pending page plus a COMMIT record, in a
   single write at the end of the last committed group.
2. **fsync the WAL** — the durability point, and the only fsync on the
   commit path. If the write or fsync fails, the WAL is cut back to its
   previous length and the group is aborted.
3. Move the pages from `pending` to `committed`. Reads are served from there
   (and the cache) until a checkpoint.

A **checkpoint** (`Pager::checkpoint`) writes every committed page into the
data file, fsyncs it, and truncates the WAL (also fsynced). It runs when the
WAL reaches `CHECKPOINT_WAL_BYTES` (4 MiB) and when the database is closed.
A failed checkpoint keeps the pages in `committed` and retries later; the
commit itself already succeeded.

On open, `Pager::recover` replays every group in the WAL that has a COMMIT
record, discards a torn tail or uncommitted records, and always resets the
WAL, so leftover garbage can never sit in front of later commits. The data
file is only written from pages already durable in the WAL, so a crash at
any point leaves either the old state or a replayable committed group.

**Abort** (`abort_group`) drops `pending`, evicts those pages from the cache,
and restores the page count. Since nothing was logged, there is nothing to
undo on disk.

### Savepoints and statement atomicity

Inside an explicit transaction each statement runs under a pager savepoint
that records the prior `pending` image of every page the statement touches.
If the statement fails, `rollback_to_savepoint` restores those images: the
statement is undone and the transaction continues.

After any rollback `Database::reload_schema` rebuilds the in-memory schema
and B-tree root handles from the pager, since a failed statement may have
split a root or altered columns before erroring.

The schema is re-saved **inside the statement's group** whenever the
statement is DDL or changes a B-tree root, so table definitions and data
always commit together.

## Concurrency

- **One writer at a time.** `Database::execute`, `begin`, `commit` and
  `rollback` take a database-wide `writer` mutex.
- **Readers** (`Database::query`) do not take the writer mutex and can run
  concurrently with each other.
- **Table locks.** `Executor::with_table_lock` takes a shared (SELECT) or
  exclusive (INSERT/UPDATE/DELETE) lock per table. Auto-commit statements
  release it on every path; explicit transactions hold it until COMMIT /
  ROLLBACK. Acquisition times out after 30 s as crude deadlock protection.
- **One transaction per `Database`.** An explicit transaction is database-wide:
  statements from other threads join it, and readers see its uncommitted
  pages. There is no snapshot isolation yet — MVCC is a candidate
  (see [ROADMAP.md](../ROADMAP.md), P4).

Lock ordering and the full commit protocol are documented in
[`.claude/skills/transaction-correctness`](../.claude/skills/transaction-correctness/SKILL.md).

## B-tree

Each table is a B-tree keyed by its single `INTEGER PRIMARY KEY`
(`BTREE_ORDER = 64` keys per node). Leaves hold serialized rows; internal
nodes hold separator keys and child page ids. Inserts split leaves and
internal nodes; deletes merge or redistribute on underflow and collapse the
root when it empties. A proptest checks the invariants over random
insert/delete sequences.

A `WHERE` containing `<pk> = <integer>` is answered by `BTree::search`
(`candidate_rows` in `src/executor.rs`) for SELECT, UPDATE and DELETE; the
full clause is still evaluated on the result. There are no secondary
indexes: any other `WHERE` scans the table, and `UNIQUE` on a non-key column
is checked by a scan.

## SQL

`src/parser.rs` is a regex- and string-splitting parser (a real lexer/parser
is roadmap item P1). It produces a `Statement` enum:

- DDL: `CREATE TABLE`, `DROP TABLE`, `ALTER TABLE` (rename table, rename /
  add / drop column)
- DML: `INSERT` (optional column list), `UPDATE`, `DELETE`
- `SELECT` with `*`, columns, `COUNT(*)`, or `vector_distance_*` projections;
  `WHERE` with comparison operators and `LIKE` joined by `AND`;
  `ORDER BY` a column or distance expression; `LIMIT`
- `BEGIN` / `COMMIT` / `ROLLBACK`

Constraints: `PRIMARY KEY` (required, single integer column), `NOT NULL`,
`UNIQUE` (NULLs never conflict), and vector dimension checks.

## Parallel execution

Once a query touches at least 1024 rows (`PARALLEL_THRESHOLD`), WHERE
filtering, ORDER BY sorting, and vector distance computation use rayon.
Smaller inputs run sequentially to avoid thread-pool overhead.

## Vector search

`F32_BLOB(n)` / `VECTOR(n)` columns store `f32` vectors with a fixed dimension.
`vector_distance_cos`, `_l2` and `_dot` work in projections and in
`ORDER BY … LIMIT k`, which uses top-k selection. `Database::vector_search`
exposes the same exact KNN. There is no approximate index yet.

## Change Data Capture

`src/cdc.rs` keeps a bounded in-memory log (default 65,536 events). The
executor *stages* events; `Database` publishes them, assigning sequence
numbers, only after the write group commits. Rolled-back statements and
transactions publish nothing. The log is not persisted across restarts.

## Async API

`Builder` → `AsyncDatabase` → `AsyncConnection` (feature `async-io`, on by
default) wraps the synchronous `Database`. Each call runs on tokio's blocking
pool via `spawn_blocking`, so the reactor never blocks on disk I/O.

## Known limitations

See the README's *Limitations* section and [ROADMAP.md](../ROADMAP.md) for
the prioritized list (single writer, no JOIN / GROUP BY / secondary indexes,
exact-only vector search, in-memory CDC).

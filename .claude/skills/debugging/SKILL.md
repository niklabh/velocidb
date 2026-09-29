---
name: debugging
description: Debugging VelociDB - REPL-based reproduction, tracing, inspecting db/WAL files, and a map of common failure modes to their causes. Use when investigating bugs, corruption errors, deadlocks/hangs, wrong query results, or data that disappears after reopen.
---

# Debugging VelociDB

## Fast reproduction via the REPL

The quickest end-to-end repro is piping SQL into the binary (non-interactive
mode reads stdin):

```bash
printf 'CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER);\nINSERT INTO t VALUES (1, 2);\nSELECT * FROM t;\n.exit\n' \
  | cargo run --quiet -- /tmp/repro.db
```

Useful meta commands: `.tables`, `.schema [name]`, `.cdc on` + `.changes`
(watch what write paths actually record). Clean up `/tmp/repro.db` and
`/tmp/repro.db-wal` between runs — stale files are a classic source of
"unreproducible" behavior.

Tracing: the binary initializes `tracing_subscriber` at WARN. For deeper
logs, temporarily raise the level in `src/main.rs` or add `tracing::debug!`
in the module under suspicion.

## Inspecting files

```bash
ls -la mydb.db mydb.db-wal     # WAL: 0 bytes after a clean close; up to ~4 MiB while open
xxd -l 64 mydb.db              # page 0; schema starts at offset 4096 (page 1)
xxd -s 4096 -l 128 mydb.db     # schema page: [chunk_len: u32][chunk...]
```

Formats are documented in the storage-format skill.

## Failure mode → likely cause

| Symptom | Look at |
|---------|---------|
| `Corruption: Invalid type tag` | row (de)serialization mismatch in `src/btree.rs` — writer and reader tags out of sync |
| `Corruption: Schema truncated...` | `save_schema`/`load_schema` format drift in `src/storage.rs` (e.g. forgot the vector dim bytes) |
| `TransactionError: Write group already active` | a write path bypassed `Database::execute`'s writer mutex, or a previous group leaked (missing commit/abort on an error path) |
| Hang / 30 s `Lock acquisition timeout` | table lock leaked — some error path returned without `release_lock`; check every early `return Err` in the executor method |
| Deadlock (no timeout) | `pager.write()` held across a call that re-locks the pager; parking_lot is not reentrant |
| Data missing after reopen | schema not re-saved after a root change — check `needs_schema_save` / root diffing in `Database::execute` |
| Wrong rows only on large tables (≥1024) | rayon parallel path diverged from the sequential path in `src/executor.rs` — the two branches must be behaviorally identical |
| Vector query returns row in wrong order | distance convention violated (lower = more similar for ALL metrics, incl. dot which is negated) or the top-k path in `vector::knn` |
| WAL grows past ~4 MiB / never truncates | checkpoint failing (look for the `checkpoint failed` tracing warning) — data-file write or fsync error? |

## Bisecting a layer

Reproduce at the lowest layer that fails:

1. SQL level: REPL script (above) or a `Database::open` + `execute`/`query` test.
2. Executor level: unit test calling `Executor` methods with a temp DB.
3. B-tree level: `BTree::new(pager)` + insert/search/scan directly.
4. WAL level: `WalManager::open` + `log_page_write`/`log_commit` /
   `read_committed_groups`; simulate crashes by truncating the `-wal` file
   (see `test_wal_tolerates_torn_tail`).

Crash-recovery bugs almost always reproduce as: write, kill before/after a
specific fsync, reopen, compare. `tests/recovery_tests.rs` has the harness
patterns to copy.

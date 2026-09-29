---
name: cdc
description: VelociDB Change Data Capture - the in-memory change log in src/cdc.rs and its executor hooks. Use when modifying cdc.rs, adding CDC hooks to new write paths, changing ChangeEvent fields, or debugging missing/duplicate/out-of-order change events.
---

# Change Data Capture (CDC)

Turso-inspired real-time change tracking. Opt-in, in-memory, bounded.

## Design (`src/cdc.rs`)

- `CdcManager` holds a `VecDeque<ChangeEvent>` behind a `parking_lot::RwLock`,
  a monotonic `AtomicU64` sequence counter (starts at 1), and an enabled flag.
- Capacity-bounded (`DEFAULT_CDC_CAPACITY = 65_536`); oldest events are
  dropped when full. Consumers detect gaps by comparing sequence numbers.
- Disabled by default. `disable()` also clears the log.
- NOT persisted — the log is lost on restart. This is documented behavior.

`ChangeEvent`: `seq`, `table`, `op` (Insert/Update/Delete), `rowid`
(primary key), `before: Option<Row>`, `after: Option<Row>`.
Insert has no `before`; Delete has no `after`.

## Public API

- `Database::{enable_cdc, disable_cdc, cdc_enabled, changes_since(seq), cdc_latest_seq}`
- `AsyncConnection::changes_since(seq).await`
- REPL: `.cdc on|off`, `.changes [seq]`

## Hook rules (executor)

One `Arc<CdcManager>` lives on `Database` and is passed into `Executor::new`.
Hooks exist in `execute_insert`, `execute_update`, `execute_delete` in
`src/executor.rs`. When adding a hook to a new write path:

1. **Stage, never record.** The executor calls `cdc.stage(...)`; `Database`
   publishes staged events (assigning sequence numbers) only after the WAL
   group commits — at the end of an auto-commit statement, or at `COMMIT`
   for an explicit transaction. Rollbacks call `discard_staged_from(mark)`.
   Still stage only after the statement's B-tree work succeeded (UPDATE /
   DELETE collect `cdc_events` locally first). `cdc.record(...)` publishes
   immediately and is for direct `CdcManager` use only.
2. Guard row-image cloning with `self.cdc.is_enabled()` to keep the disabled
   path zero-cost.
3. For primary-key-changing UPDATEs, `rowid` is the OLD key; the new key is
   visible in `after`.
4. ALTER TABLE / DDL is intentionally not captured (matches "data" capture).

## Tests

Unit tests in `src/cdc.rs` (disabled-by-default, ordering, capacity bound);
end-to-end in `tests/advanced_features_tests.rs::test_cdc_capture_and_poll`
(asserts op sequence, before/after images, incremental polling) and
`tests/transaction_tests.rs::test_cdc_publishes_only_committed_changes`
(nothing published before COMMIT or after ROLLBACK / failed statements).

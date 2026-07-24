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

1. **Record only after the statement succeeded.** UPDATE/DELETE collect
   `cdc_events` into a local Vec inside the B-tree block and call
   `cdc.record(...)` only after the `result` is `Ok` — a failed statement's
   pages are rolled back by `abort_group`, so recording early would emit
   phantom events.
2. Guard row-image cloning with `self.cdc.is_enabled()` to keep the disabled
   path zero-cost.
3. For primary-key-changing UPDATEs, `rowid` is the OLD key; the new key is
   visible in `after`.
4. ALTER TABLE / DDL is intentionally not captured (matches "data" capture).

## Tests

Unit tests in `src/cdc.rs` (disabled-by-default, ordering, capacity bound);
end-to-end in `tests/turso_features_tests.rs::test_cdc_capture_and_poll`
(asserts op sequence, before/after images, incremental polling).

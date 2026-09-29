---
name: testing
description: How to run and write VelociDB tests - test suite layout, commands, and patterns for unit, integration, crash-recovery, property-based, and async tests. Use when adding features, fixing bugs, writing new tests, or deciding which suites must pass before a change is done.
---

# Testing VelociDB

## Suite layout

| Suite | Location | Covers |
|-------|----------|--------|
| Unit tests | `#[cfg(test)] mod tests` in each `src/*.rs` | parser, btree (incl. proptest), wal, vector, cdc, async_api, storage |
| `tests/parser_golden.rs` | parser | AST of ~180 statements vs `tests/golden/parser.golden` (`UPDATE_GOLDEN=1` to regenerate) |
| `tests/parser_proptest.rs` | parser | AST → SQL → AST round-trip, `split_statements`, no panics |
| `tests/index_tests.rs` | secondary indexes | DDL, maintenance, reopen/crash, rollback, differential proptest (indexed vs unindexed table) |
| `tests/integration_tests.rs` | full SQL path | CRUD, WHERE, ORDER BY, LIMIT, constraints |
| `tests/recovery_tests.rs` | crash safety | WAL replay, torn tails, uncommitted groups, reopen persistence |
| `tests/transaction_tests.rs` | transactions + constraints | ROLLBACK of DML/DDL, uncommitted-on-close, savepoints, CDC publication, `UNIQUE` |
| `tests/advanced_features_tests.rs` | Turso-inspired features | vector search, KNN, async API, CDC, ALTER TABLE, parallel paths |
| `benches/benchmarks.rs` | criterion | perf only, not part of `cargo test` |

## Commands

```bash
cargo test                                 # everything (~2 min)
cargo test --test advanced_features_tests    # one integration suite
cargo test --lib vector                    # unit tests of one module
cargo test test_cdc_capture_and_poll      # single test by name
cargo check --all-targets                  # fast type-check incl. tests
cargo clippy --all-targets -- -D warnings  # CI gate (active path)
cargo test --lib --features experimental   # experimental modules' unit tests
```

CI (`.github/workflows/ci.yml`) runs all of the above on Linux and macOS for
every pull request. Most suite time is fsync latency from auto-commit
statements; wrap bulk setup in `db.begin()` / `db.commit()` to keep new tests
fast.

The full `cargo test` takes ~2 minutes (lib proptest + integration suites are
the slow parts). All suites must pass before a change is complete.

## Patterns

- **Temp databases**: always `tempfile::NamedTempFile::new()` then
  `Database::open(tmp.path())`. Never write test DBs into the repo.
- **Reopen tests** (required for any on-disk format change): create + write in
  one scope, `db.close()`, drop, reopen, assert exact values. See
  `test_vector_schema_survives_reopen` and `recovery_tests.rs`.
- **Crash simulation**: write via `WalManager` directly, then truncate the
  `-wal` file by a few bytes to simulate a torn record
  (`test_wal_tolerates_torn_tail`).
- **Parallel-path tests**: insert > 1024 rows so the rayon paths in the
  executor actually run (`test_parallel_filter_and_sort_large_set`).
- **Async tests**: `#[tokio::test]`; for concurrency, spawn tasks each with
  their own `db.connect()` connection and join all before asserting.
- **Property tests**: btree uses proptest for random insert/delete sequences;
  prefer extending those when touching split/merge/redistribute logic.

## What to test for each kind of change

- Parser change → golden case in `tests/parser_golden.rs` (+ lexer unit test if tokens change) + one end-to-end query test.
- Write-path / executor change → integration test + `recovery_tests` must
  still pass.
- On-disk format change → reopen test + all of `recovery_tests`.
- New SQL feature → REPL smoke test too:
  `printf 'SQL...;\n.exit\n' | cargo run --quiet -- /tmp/smoke.db`

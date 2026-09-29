---
name: code-quality
description: VelociDB code conventions - error handling, module boundaries, the experimental-modules policy, locking style, and documentation expectations. Use when writing or reviewing any VelociDB code, deciding where new code lives, or cleaning up warnings.
---

# Code Quality Conventions

## Active path vs experimental modules

The SQL engine uses ONLY: `storage`, `btree`, `parser`, `executor`,
`transaction`, `types`, `wal`, `vector`, `cdc`, `async_api`.

`mvcc`, `async_io`, `lockfree`, `simd`, `btree_optimized`, `crdt`,
`cloud_vfs`, `hybrid_storage`, `pmem` are **experimental**: compiled only with
`--features experimental`, never re-exported from the crate root, not wired
in. Do not add dependencies from active-path modules to experimental ones,
and do not "fix" experimental-module warnings as part of unrelated changes.
Graduation rules are in `docs/experimental.md`; a graduating module is
un-gated in `src/lib.rs` and documented in the README and
`docs/architecture.md` in the same change.

## Error handling

- Library code returns `crate::types::Result<T>` (`VelociError`). Pick the
  matching variant: `ParseError`, `ConstraintViolation`, `NotFound`,
  `Corruption` (on-disk invariant broken), `TransactionError`, `TypeMismatch`,
  `StorageError`, `Busy`.
- No `unwrap()`/`expect()` on the active path except for logically infallible
  cases with a justifying message (e.g. `expect("group active after begin_group")`).
  Tests and the REPL binary may unwrap freely.
- Error strings include the offending identifier: `"Table '{}' not found"`.

## Locking style

- parking_lot `RwLock`/`Mutex` everywhere; locks are NOT reentrant.
- Keep guard scopes minimal: clone schema/rows out, drop the guard, then
  compute. Comment the release point when non-obvious
  (`// schema lock released`).
- Document lock ordering when a function takes more than one lock (see the
  header of `src/transaction.rs`).

## Style

- Module-level `//!` docs stating purpose; `///` docs with `# Example` on
  public `Database` methods (they compile as doctests — keep them building).
- Comments explain invariants and non-obvious "why", never restate the code.
- On-disk formats and thresholds are named constants (`PAGE_SIZE`,
  `BTREE_ORDER`, `PARALLEL_THRESHOLD`, `DEFAULT_CDC_CAPACITY`).
- New feature = update README ("What works today" / "Limitations") and
  CHANGELOG in the same change. Known limitations are documented, not hidden.
- Performance claims in docs need a measurement (criterion bench or a stated
  machine + method). No projected or aspirational numbers.
- `src/main.rs` builds against the `velocidb` library crate — never re-add
  `mod` declarations there.

## Warnings

CI enforces `cargo fmt --check` on the whole tree. The active path is
clippy-clean and CI enforces
`cargo clippy --all-targets -- -D warnings` (default features, i.e. without
experimental modules). Keep it that way; prefer fixing a lint over
`#[allow]`, and justify any `#[allow]` in a comment. Experimental-module
warnings are tolerated.

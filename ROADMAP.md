# VelociDB Roadmap

Prioritized work for the **active SQL path** (`storage`, `btree`, `parser`,
`executor`, `transaction`, `wal`, `vector`, `cdc`, `async_api`). Experimental
modules stay off this path until explicitly graduated (see
[code-quality skill](.claude/skills/code-quality/SKILL.md)).

Status reflects the tree as of the latest README / CHANGELOG. Check boxes as
items land; update README Limitations and CHANGELOG in the same change.

---

## P0 — Correctness & trust

Ship before expanding the SQL surface or advertising full ACID.

- [x] **Real multi-statement `ROLLBACK`** — fold an explicit `BEGIN`…`COMMIT`
      into a single WAL group (or track undo) so `ROLLBACK` undoes storage
      mutations, not only locks
- [x] **Enforce `UNIQUE`** on non-primary-key columns (INSERT / UPDATE); add
      regression tests; keep `.schema` / README in sync
- [x] **CI on every PR** — `cargo test --all-targets`, `cargo clippy` on the
      active path (docs workflow alone is not enough)
- [x] **Docs / API honesty** — align older docs
      (`docs/architecture.md`, `docs/performance.md`, `docs/implementation.md`
      → `docs/experimental.md`)
      with README; stop implying experimental modules are integrated
- [x] **Stop crate-root re-exports of experimental types** (or gate them
      behind an `experimental` feature) so the public API matches the engine

---

## P1 — Foundations

Unblock almost every later SQL and concurrency feature.

- [ ] **Replace the regex parser** with a lexer + recursive-descent (or
      pest/lalrpop) AST; keep current SQL as golden tests
- [ ] **Typed expression AST** — stop carrying distance calls / predicates as
      opaque strings
- [ ] **Secondary indexes** — `CREATE INDEX` / `DROP INDEX` on one column;
      maintain on write; equality probe in the executor (range scans later)
- [ ] **Parser fuzzing** (and/or property tests) for statement splitting and
      value parsing
- [ ] **Concurrent-writer / deadlock stress tests** beyond the 30s lock timeout
- [x] **Primary-key point lookups** — use `BTree::search` for `WHERE pk = …`
      instead of a full scan
- [x] **Fewer fsyncs per commit** — one fsync per commit; the WAL is
      checkpointed at 4 MiB and on close (auto-commit INSERT ~79 → ~245 rows/s
      on macOS, see `docs/performance.md`)
- [ ] **Durability level option** — e.g. a `synchronous = NORMAL`-style mode
      (plain `fsync` instead of `F_FULLFSYNC` on macOS, or fsync only at
      checkpoint) for callers that accept losing the last commits on power loss
- [ ] **Group commit** — let concurrent auto-commit writers share one WAL
      fsync

---

## P2 — SQL surface

Assumes a real parser (or at least a typed expression layer) is underway.

- [ ] `WHERE` with `OR` and parenthesized predicates
- [ ] `INNER JOIN` (nested-loop + PK / index probe)
- [ ] `LEFT JOIN`
- [ ] `GROUP BY` with `SUM` / `AVG` / `MIN` / `MAX` (generalize beyond
      `COUNT(*)`)
- [ ] Prepared statements / bind parameters
- [ ] Composite `PRIMARY KEY` and multi-column `UNIQUE`
- [ ] Quoted identifiers
- [ ] Subqueries / `IN (SELECT …)` (non-correlated first)
- [ ] `ORDER BY` / `SELECT` expressions beyond vector distance helpers

---

## P3 — Differentiation

Lean into Turso-inspired strengths once the core is trustworthy.

### Vector search

- [x] Exact parallel KNN (`vector_distance_*`, `Database::vector_search`)
- [ ] Approximate index (HNSW first; DiskANN-style later)
- [ ] `vector_distance_*` usable in `WHERE`, not only `ORDER BY` / projection

### Change Data Capture

- [x] In-memory bounded change log with sequence numbers and before/after images
- [ ] Durable CDC (WAL-backed or sidecar file; survives restart)
- [ ] Watch / subscribe API (beyond `changes_since` polling)
- [ ] Optional JSON / stream export for replication demos

### Async & embedding

- [x] Tokio blocking-pool `Builder` / `AsyncDatabase` / `AsyncConnection`
- [ ] Streaming query results
- [ ] Cancellation of in-flight queries
- [ ] Connection-local state (session pragmas, etc.)
- [ ] Optional thin network server (HTTP or WebSocket) — only after P0/P1

---

## P4 — Experimental modules

Graduate deliberately or cut. Do not add active-path dependencies until a
module is promoted in `src/lib.rs` and the README.

| Module | Direction |
|--------|-----------|
| `mvcc` | Candidate after real `ROLLBACK` — reader concurrency |
| `simd` | Candidate for filter / aggregate hot paths (feature-gated) |
| `btree_optimized` | Merge useful ideas into `btree.rs` or drop |
| `async_io` | Park until sync path is correct; optimize later |
| `lockfree` | Prove with benches or fold cache ideas into `Pager` |
| `crdt` / `cloud_vfs` / `pmem` / `hybrid_storage` | Keep as research or remove from default build |

- [ ] Decide per module: **graduate**, **archive**, or **delete**
- [ ] If graduating MVCC: wire into `Database`, document lock ordering, add
      integration + crash-recovery tests
- [ ] Live benchmarks (criterion) with numbers checked into docs — no invented
      “10–50×” claims

---

## Done (active path highlights)

Kept here so the roadmap does not re-litigate shipped work. Details live in
CHANGELOG.

- [x] Page-based storage + DashMap read cache
- [x] WAL with CRC32, group commit, torn-tail-tolerant recovery
- [x] B-tree primary index (leaf + internal split / merge / redistribute)
- [x] Core SQL: CREATE / DROP / ALTER TABLE, INSERT / SELECT / UPDATE / DELETE
- [x] `BEGIN` / `COMMIT` / `ROLLBACK` as one WAL group; statement savepoints
- [x] Parallel WHERE / ORDER BY / vector distance (rayon, ≥ 1024 rows)
- [x] Vector columns + exact KNN
- [x] In-memory CDC + REPL `.cdc` / `.changes`
- [x] Async API facade
- [x] Interactive REPL (history, multi-line, meta commands)
- [x] Recovery + advanced-feature integration tests; B-tree proptest

---

## Suggested sequence

1. ~~CI + enforce `UNIQUE`~~ (done)
2. ~~Multi-statement WAL groups + real `ROLLBACK`~~ (done)
3. ~~Primary-key point lookups + fewer fsyncs per commit~~ (done)
4. Lexer / parser + golden tests
5. Secondary indexes + equality probe
6. `OR` → `INNER JOIN` → `GROUP BY` aggregates
7. Durable CDC + approximate vector index
8. Graduate or archive experimental modules

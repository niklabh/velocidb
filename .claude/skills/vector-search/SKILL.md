---
name: vector-search
description: VelociDB vector search implementation - F32_BLOB(n)/VECTOR(n) columns, vector32 literals, distance functions, and exact parallel KNN. Use when modifying src/vector.rs, adding distance metrics, extending vector SQL syntax, or debugging dimension-mismatch, KNN-ordering, or vector-parsing issues.
---

# Vector Search

Turso/libSQL-compatible exact (brute-force) vector search. There is no ANN
index; every query scans candidate rows, in parallel above 1024 rows.

## Data flow

| Stage | Where | Detail |
|-------|-------|--------|
| Column type | `DataType::Vector(u32)` in `src/types.rs` | parsed from `F32_BLOB(n)` / `VECTOR(n)` in `DataType::from_str` |
| Value | `Value::Vector(Vec<f32>)` | row-format tag 5 in `src/btree.rs` |
| Literal | `parse_vector_constructor` in `src/vector.rs` | accepts `vector32('[...]')`, `vector('[...]')`, `vector64('[...]')`, quoted `'[...]'`, bare `[...]` |
| Insert validation | `execute_insert` in `src/executor.rs` | dimension must equal the declared `n`; NULL allowed |
| Schema persistence | `save_schema`/`load_schema` in `src/storage.rs` | type byte 5 followed by `dim: u32` |

## SQL surface

```sql
CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(3));
INSERT INTO docs VALUES (1, vector32('[1.0, 0.0, 0.0]'));

SELECT id, vector_distance_cos(embedding, vector32('[1,0,0]'))
FROM docs
ORDER BY vector_distance_cos(embedding, vector32('[1,0,0]'))
LIMIT 5;
```

- Distance functions: `vector_distance_cos` (1 - cosine similarity),
  `vector_distance_l2` (euclidean), `vector_distance_dot` (negated dot
  product so lower = more similar for ALL metrics; keep that convention).
- Distance expressions are parsed by `parse_distance_expr` in `src/vector.rs`
  — the parser (`src/parser.rs`) deliberately keeps them as opaque strings in
  `OrderBy.column` and the SELECT column list; the executor interprets them.
  If you add a function name, register it in
  `DistanceMetric::from_function_name`.
- ORDER BY ascending + LIMIT k triggers the top-k KNN path
  (`vector::knn`, `select_nth_unstable_by`); descending does a full sort.
- Rows with NULL or wrong-dimension vectors get distance `f64::INFINITY`
  (sort last) rather than erroring mid-scan.

## Programmatic APIs

- Sync: `Database::vector_search(table, column, &[f32], k, DistanceMetric)`
- Async: `AsyncConnection::vector_search(...)`
- Both return `Vec<(f64 distance, Row)>` ascending.

## Parser pitfalls (learned the hard way)

- `vector32('[1, 2]')` contains commas and parens. The parser skips it as
  a balanced token run (`Cursor::skip_balanced`) and hands the exact source
  slice to `vector::parse_vector_constructor`; never re-split the SQL text.
- SELECT items and the ORDER BY expression are also source slices, so the
  executor sees them exactly as written (including inner whitespace).

## Tests

Unit tests live in `src/vector.rs`; end-to-end coverage in
`tests/advanced_features_tests.rs` (roundtrip, dimension enforcement, KNN
ordering, projection distances, reopen persistence, 1500-row parallel KNN).

```bash
cargo test --lib vector && cargo test --test advanced_features_tests
```

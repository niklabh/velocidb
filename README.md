# VelociDB

An embedded SQL database written in Rust, inspired by
[Turso](https://github.com/tursodatabase/turso). Page-based storage with a
B-tree primary index, write-ahead log for crash safety, native **async API**,
**parallel query execution**, **vector search**, **change data capture**, and
an interactive REPL.

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

## Documentation

- **[Quick Start](docs/quickstart.md)**
- **[Architecture](docs/architecture.md)**
- **[REPL Usage](docs/repl_usage.md)**
- **[Contributing](docs/contributing.md)**

## Quick Start

### Interactive REPL

```bash
cargo run --release
```

```
VelociDB v0.1.0
Database: veloci.db
Type '.help' for help, '.exit' to quit. Statements end with ';'.

velocidb> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
OK
velocidb> INSERT INTO users VALUES (1, 'Alice', 30);
OK
velocidb> SELECT * FROM users WHERE age > 25 ORDER BY name LIMIT 10;
id | name | age
-----------+-----------+-----------
1 | Alice | 30

1 row(s) returned
velocidb> .schema users
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
```

REPL niceties: arrow-key history, persistent history file
(`~/.velocidb_history`, or `$VELOCIDB_HISTORY`), multi-line statements
terminated by `;`, and `.help`, `.tables`, `.schema [name]`, `.cdc on|off`,
`.changes [seq]`, `.exit`.

### As a Library (sync)

```rust
use velocidb::Database;

fn main() -> anyhow::Result<()> {
    let db = Database::open("my_database.db")?;

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice')")?;

    let results = db.query("SELECT * FROM users ORDER BY id")?;
    println!("Found {} users", results.rows.len());
    Ok(())
}
```

### As a Library (async, Turso-style)

```rust
use velocidb::Builder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = Builder::new_local("my_database.db").build().await?;
    let conn = db.connect()?;

    conn.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)").await?;
    conn.execute("INSERT INTO users VALUES (1, 'Alice')").await?;

    let results = conn.query("SELECT * FROM users ORDER BY id").await?;
    println!("Found {} users", results.rows.len());
    Ok(())
}
```

Every call is offloaded to the tokio blocking pool, so async tasks never
stall the reactor; concurrent read futures execute in parallel.

### Vector search

Exact (brute-force) K-nearest-neighbour search with Turso/libSQL-style
syntax. Distance computation, filtering, and sorting are parallelized with
rayon on larger tables.

```sql
CREATE TABLE docs (id INTEGER PRIMARY KEY, title TEXT, embedding F32_BLOB(3));
INSERT INTO docs VALUES (1, 'alpha', vector32('[1.0, 0.0, 0.0]'));
INSERT INTO docs VALUES (2, 'beta',  vector32('[0.0, 1.0, 0.0]'));

-- KNN: nearest 5 documents by cosine distance
SELECT id, title, vector_distance_cos(embedding, vector32('[1, 0, 0]'))
FROM docs
ORDER BY vector_distance_cos(embedding, vector32('[1, 0, 0]'))
LIMIT 5;
```

Supported metrics: `vector_distance_cos`, `vector_distance_l2`,
`vector_distance_dot`. The same search is available programmatically:

```rust
use velocidb::{Database, DistanceMetric};

let db = Database::open("my_database.db")?;
let neighbors = db.vector_search("docs", "embedding", &[1.0, 0.0, 0.0], 5, DistanceMetric::Cosine)?;
for (distance, row) in neighbors {
    println!("{distance:.4}: {:?}", row.values);
}
```

### Change Data Capture

```rust
let db = Database::open("my_database.db")?;
db.enable_cdc();

db.execute("INSERT INTO users VALUES (2, 'Bob')")?;
db.execute("DELETE FROM users WHERE id = 1")?;

for change in db.changes_since(0) {
    println!("#{} {} {} rowid={}", change.seq, change.op, change.table, change.rowid);
}
```

Every committed INSERT / UPDATE / DELETE is captured with a monotonically
increasing sequence number and before/after row images — useful for
replication, cache invalidation, or audit trails. In the REPL: `.cdc on`,
`.changes [seq]`.

## What works today

Storage and durability

- 4 KB page-based storage on a single data file.
- **Write-ahead log** (`<db>-wal`) with CRC32-checked records. Each write
  statement runs as one atomic group: the WAL is fsynced on commit, then the
  modified pages are applied to the data file, fsynced, and the WAL truncated.
- Crash recovery on open: committed groups in the WAL are replayed; partial
  / torn / uncommitted records are discarded.
- DashMap-backed read cache with bounded capacity.

Indexing

- B-tree primary key index with full split + merge + redistribute paths for
  both leaf and internal nodes (proptest covers random insert/delete sequences).

SQL surface

- `CREATE TABLE` (INTEGER / REAL / TEXT / BLOB / `F32_BLOB(n)` / `VECTOR(n)`
  columns; `PRIMARY KEY`, `NOT NULL`, `UNIQUE` constraints)
- `DROP TABLE`
- `ALTER TABLE t RENAME TO new | RENAME COLUMN a TO b | ADD COLUMN c type
  | DROP COLUMN c`
- `INSERT INTO ... VALUES (...)` with optional explicit column list;
  vector literals via `vector32('[...]')`, `vector('[...]')` or bare `[...]`
- `SELECT [* | cols | COUNT(*) | vector_distance_*(col, vec)] FROM t
  [WHERE ...] [ORDER BY col | vector_distance_*(col, vec) [ASC|DESC]] [LIMIT n]`
- `UPDATE t SET col = val [, ...] [WHERE ...]`
- `DELETE FROM t [WHERE ...]`
- `WHERE` supports `=`, `!=`, `<>`, `>`, `<`, `>=`, `<=`, `LIKE`, with `AND`
- `BEGIN` / `COMMIT` / `ROLLBACK` (see limitations below)

Vector search (Turso-inspired)

- `F32_BLOB(n)` / `VECTOR(n)` column type with dimension enforcement.
- Distance metrics: cosine, euclidean (L2), dot product.
- Exact KNN via `ORDER BY vector_distance_*(...) LIMIT k` (top-k selection,
  not a full sort) or `Database::vector_search` / `AsyncConnection::vector_search`.

Async and parallel (Turso-inspired)

- Native async API: `Builder` → `AsyncDatabase` → `AsyncConnection`
  (`async-io` feature, enabled by default).
- WHERE filtering, ORDER BY sorting and vector distance computation run on
  the rayon thread pool once a query touches ≥ 1024 rows.

Change Data Capture (Turso-inspired)

- Opt-in change log of committed INSERT / UPDATE / DELETE with sequence
  numbers and before/after row images; poll with `changes_since(seq)`.

Concurrency

- Writers serialized at the `Database` level so the pager only ever has one
  active WAL group. Readers run concurrently with each other.
- Lock-manager-based per-table shared/exclusive locking for in-flight
  transactions.

## Limitations (please read)

- **Single-writer.** All write statements take a global writer mutex. Reads
  can be concurrent with each other but a single in-flight write blocks
  other writes for its duration.
- **Multi-statement transaction rollback is incomplete.** `BEGIN` and
  `COMMIT` release/acquire locks correctly, but `ROLLBACK` does not undo
  storage mutations made by earlier statements in the transaction. (Each
  statement is its own WAL group; a future change can fold an explicit
  transaction into a single WAL group.)
- **No `JOIN`, `GROUP BY`, sub-queries**, no indexes other than the primary
  key.
- **Single primary key column.** Composite primary keys are not supported.
- **Vector search is exact.** Every query scans all candidate rows
  (in parallel). Approximate indexing (HNSW/DiskANN-style) is future work,
  mirroring Turso's own roadmap.
- **Embedded only.** The async API runs in-process; there is no network
  server.
- **CDC is in-memory.** The change log is bounded (default 65,536 events)
  and not persisted across restarts.

## Experimental modules (not on the active path)

The crate exports several modules that explore advanced storage and
concurrency techniques. They are **not** used by the SQL engine today and
are exported only so the experimentation is visible:

- `mvcc` — Multi-version concurrency control
- `async_io` — Tokio / `io_uring` page I/O
- `lockfree` — Lock-free page cache and queues
- `simd` — Vectorized filter / aggregation kernels
- `btree_optimized` — Cache-conscious B-tree node layout
- `crdt` — CRDT synchronization primitives
- `cloud_vfs` — S3 / Azure / GCS-backed VFS
- `hybrid_storage` — Row/columnar hybrid table layout
- `pmem` — Persistent-memory / DAX VFS

## Installation

```bash
git clone https://github.com/niklabh/velocidb.git
cd velocidb
cargo build --release
```

## License

MIT — see [LICENSE](LICENSE).

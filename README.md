# VelociDB

An embedded SQL database written in Rust. Single-writer, page-based storage
with a B-tree primary index, write-ahead log for crash safety, and an
interactive REPL.

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
terminated by `;`, and `.help`, `.tables`, `.schema [name]`, `.exit`.

### As a Library

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

- `CREATE TABLE` (INTEGER / REAL / TEXT / BLOB columns; `PRIMARY KEY`,
  `NOT NULL`, `UNIQUE` constraints)
- `DROP TABLE`
- `INSERT INTO ... VALUES (...)` with optional explicit column list
- `SELECT [* | cols | COUNT(*)] FROM t [WHERE ...] [ORDER BY col [ASC|DESC]] [LIMIT n]`
- `UPDATE t SET col = val [, ...] [WHERE ...]`
- `DELETE FROM t [WHERE ...]`
- `WHERE` supports `=`, `!=`, `<>`, `>`, `<`, `>=`, `<=`, `LIKE`, with `AND`
- `BEGIN` / `COMMIT` / `ROLLBACK` (see limitations below)

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
  key, no `ALTER TABLE`.
- **Single primary key column.** Composite primary keys are not supported.
- **No async or networked access.** The library is embedded only.

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

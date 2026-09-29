# VelociDB Quick Start

This guide covers everything the engine supports today. For internals see
[architecture.md](architecture.md); for REPL details see
[repl_usage.md](repl_usage.md).

## Install

```bash
git clone https://github.com/niklabh/velocidb.git
cd velocidb
cargo build --release
```

As a dependency:

```toml
[dependencies]
velocidb = "0.3"
```

Feature flags:

| Feature | Default | Enables |
|---------|:-------:|---------|
| `async-io` | yes | `Builder` / `AsyncDatabase` / `AsyncConnection` (tokio) |
| `experimental` | no | Research modules not used by the engine ([experimental.md](experimental.md)) |

## 1. The REPL

```bash
cargo run --release -- mydata.db
```

```
velocidb> CREATE TABLE users (id INTEGER PRIMARY KEY, email TEXT UNIQUE, age INTEGER);
velocidb> INSERT INTO users VALUES (1, 'alice@example.com', 30);
velocidb> SELECT * FROM users WHERE age > 25 ORDER BY email LIMIT 10;
velocidb> .schema users
velocidb> .tables
```

Statements end with `;` and may span lines. Meta commands: `.help`,
`.tables`, `.schema [name]`, `.cdc on|off`, `.changes [seq]`, `.exit`.

## 2. Library (sync)

```rust
use velocidb::Database;

fn main() -> anyhow::Result<()> {
    let db = Database::open("mydata.db")?;

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, age INTEGER)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
    db.execute("INSERT INTO users (id, name) VALUES (2, 'Bob')")?; // age = NULL

    let result = db.query("SELECT name, age FROM users WHERE age >= 18 ORDER BY name")?;
    for row in &result.rows {
        println!("{:?}", row.values);
    }
    Ok(())
}
```

`Database::open` returns an `Arc<Database>`, so it can be shared across
threads. Writes are serialized; reads run concurrently.

## 3. Transactions

Outside a transaction each statement commits on its own. Wrap related
changes — and bulk loads — in a transaction:

```rust
db.begin()?;                         // or db.execute("BEGIN")?
db.execute("UPDATE accounts SET balance = 50 WHERE id = 1")?;
db.execute("UPDATE accounts SET balance = 150 WHERE id = 2")?;
db.commit()?;                        // both changes land atomically
```

- `COMMIT` makes every change durable at once, with a single WAL write and
  fsync. Batching thousands of inserts this way is tens of times faster than
  auto-commit (see [performance.md](performance.md)).
- `ROLLBACK` discards every change since `BEGIN`, including `CREATE` /
  `ALTER` / `DROP TABLE`.
- If a statement inside a transaction fails (e.g. a duplicate key), only that
  statement is undone. The transaction stays open.
- Closing the database without `COMMIT` discards the transaction.

A transaction belongs to the whole `Database` handle: statements issued from
other threads while it is open join it.

## 4. Schema changes

```sql
ALTER TABLE users ADD COLUMN city TEXT;          -- existing rows get NULL
ALTER TABLE users RENAME COLUMN city TO town;
ALTER TABLE users DROP COLUMN town;              -- the primary key cannot be dropped
ALTER TABLE users RENAME TO members;
DROP TABLE members;
```

## 5. Constraints

- `PRIMARY KEY` — required, a single `INTEGER` column.
- `NOT NULL`
- `UNIQUE` — enforced on INSERT and UPDATE. Multiple NULLs are allowed.
  Without secondary indexes the check scans the table.
- Vector columns enforce their dimension.

Violations return `VelociError::ConstraintViolation`.

## 6. Vector search

```sql
CREATE TABLE docs (id INTEGER PRIMARY KEY, title TEXT, embedding F32_BLOB(3));
INSERT INTO docs VALUES (1, 'alpha', vector32('[1.0, 0.0, 0.0]'));
INSERT INTO docs VALUES (2, 'beta',  vector32('[0.0, 1.0, 0.0]'));

SELECT id, title, vector_distance_cos(embedding, vector32('[1, 0, 0]'))
FROM docs
ORDER BY vector_distance_cos(embedding, vector32('[1, 0, 0]'))
LIMIT 5;
```

Metrics: `vector_distance_cos`, `vector_distance_l2`, `vector_distance_dot`.
From Rust:

```rust
use velocidb::DistanceMetric;
let hits = db.vector_search("docs", "embedding", &[1.0, 0.0, 0.0], 5, DistanceMetric::Cosine)?;
```

Search is exact: every row is scanned, in parallel for large tables.

## 7. Change Data Capture

```rust
db.enable_cdc();
db.execute("INSERT INTO users VALUES (3, 'Carol', 41)")?;
for c in db.changes_since(0) {
    println!("#{} {} {} rowid={} before={:?} after={:?}",
             c.seq, c.op, c.table, c.rowid, c.before, c.after);
}
```

Only committed changes appear: nothing inside an open transaction, and
nothing that was rolled back. The log is in memory, bounded to 65,536
events, and reset on restart.

## 8. Async API

```rust
use velocidb::Builder;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = Builder::new_local("mydata.db").build().await?;
    let conn = db.connect()?;

    conn.begin().await?;
    conn.execute("INSERT INTO users VALUES (4, 'Dan', 22)").await?;
    conn.commit().await?;

    let rows = conn.query("SELECT * FROM users").await?;
    println!("{} rows", rows.rows.len());
    Ok(())
}
```

Each call runs on tokio's blocking pool.

## What's not supported yet

`JOIN`, `GROUP BY` / aggregates other than `COUNT(*)`, `OR` and parentheses
in `WHERE`, subqueries, secondary indexes, composite keys, and prepared
statements. See [ROADMAP.md](../ROADMAP.md).

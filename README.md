# VelociDB

A next-generation embedded database with modern architecture: MVCC, async I/O, SIMD vectorization, CRDT sync, and persistent memory support.

> **🚀 High-Performance**: Built from the ground up for multi-core systems, NVMe storage, and distributed edge computing.

[![Rust](https://img.shields.io/badge/rust-1.70%2B-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Architecture](https://img.shields.io/badge/architecture-modern-green.svg)](docs/architecture.md)
[![Docs](https://img.shields.io/badge/docs-online-blue.svg)](https://niklabh.github.io/velocidb/)

## 📚 Documentation

- **[Quick Start](docs/quickstart.md)**: Get up and running in minutes.
- **[Architecture](docs/architecture.md)**: Deep dive into MVCC, Async I/O, and storage layout.
- **[Performance](docs/performance.md)**: Benchmarks and optimization guide.
- **[Implementation](docs/implementation.md)**: Detailed design notes.
- **[Development Journey](docs/development_journey.md)**: The story behind our design choices.
- **[REPL Usage](docs/repl_usage.md)**: Guide to the interactive shell.
- **[Contributing](docs/contributing.md)**: How to get involved.
- **[Publishing](docs/publishing.md)**: Guide to publishing on crates.io.

## 🚀 Quick Start

### Interactive REPL

```bash
cargo run --release
```

```
VelociDB v0.1.0
Type 'help' for help, 'exit' or 'quit' to exit

velocidb> CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)
OK
velocidb> INSERT INTO users VALUES (1, 'Alice', 30)
OK
velocidb> SELECT * FROM users WHERE age > 25
...
```

### As a Library

```rust
use velocidb::Database;

fn main() -> anyhow::Result<()> {
    let db = Database::open("my_database.db")?;
    
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO users VALUES (1, 'Alice')")?;
    
    let results = db.query("SELECT * FROM users")?;
    println!("Found {} users", results.rows.len());
    
    Ok(())
}
```

## Features

- **Modern Core**: MVCC, Async I/O (Tokio/io_uring), Lock-Free structures.
- **High Performance**: SIMD vectorization, Cache-conscious B-Tree, NVMe optimization.
- **Storage Flexibility**: Hybrid Row/Columnar storage, PMEM/DAX support, Cloud VFS.
- **Sync Ready**: CRDT synchronization for edge/mobile.

## Installation

```bash
git clone https://github.com/niklabh/velocidb.git
cd velocidb
cargo build --release
```

## Limitations

- **B-Tree Capacity**: Currently limited to ~4,096 records due to pending internal node splitting implementation.
- **SQL Support**: Core subset only (No JOINs, GROUP BY yet).

## License

MIT License. See [LICENSE](LICENSE) for details.

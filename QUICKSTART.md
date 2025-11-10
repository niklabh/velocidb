# VelociDB Quick Start Guide

## Getting Started with Modern Features

This guide shows you how to use VelociDB's modern architecture features including MVCC, async I/O, SIMD vectorization, and more.

## Installation

```bash
git clone https://github.com/niklabh/velocidb.git
cd velocidb
cargo build --release --all-features
```

## Feature Flags

```bash
# Default build (async-io + lock-free)
cargo build --release

# With all features
cargo build --release --features "async-io,io-uring,pmem-support,crdt-sync,cloud-vfs,simd"

# Minimal build (traditional mode)
cargo build --release --no-default-features
```

---

## 1. MVCC (Multi-Version Concurrency Control)

### Non-Blocking Concurrent Transactions

```rust
use velocidb::MvccManager;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mvcc = MvccManager::new();
    
    // Start multiple concurrent transactions
    let txn1 = mvcc.begin_transaction();
    let txn2 = mvcc.begin_transaction();
    
    // Insert in transaction 1
    mvcc.insert_version(
        "users",
        1,
        vec![Value::Text("Alice".to_string())],
        &txn1
    )?;
    
    // Read in transaction 2 (non-blocking!)
    let data = mvcc.read_version("users", 1, &txn2)?;
    
    // Commit both
    mvcc.commit_transaction(&txn1)?;
    mvcc.commit_transaction(&txn2)?;
    
    Ok(())
}
```

### Scan with Snapshot Isolation

```rust
// Get all visible records for a snapshot
let snapshot = mvcc.begin_transaction();
let records = mvcc.scan_table("users", &snapshot)?;

for (key, values) in records {
    println!("User {}: {:?}", key, values);
}
```

### Garbage Collection

```rust
// Periodically clean up old versions
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        mvcc.vacuum();
    }
});
```

---

## 2. Async I/O

### Basic Async Operations

```rust
use velocidb::{AsyncPager, AsyncVfs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create async pager
    let pager = AsyncPager::new("database.db", 1024).await?;
    
    // Read a page asynchronously
    let page = pager.read_page(0).await?;
    
    // Write a page
    let mut new_page = Page::new();
    new_page.data_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
    pager.write_page(1, new_page).await?;
    
    // Flush to disk
    pager.flush().await?;
    
    Ok(())
}
```

### Batch I/O for Maximum Throughput

```rust
use velocidb::BatchIoExecutor;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let pager = Arc::new(AsyncPager::new("database.db", 1024).await?);
    let executor = BatchIoExecutor::new(Arc::clone(&pager));
    
    // Read multiple pages in parallel
    let page_ids = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let pages = executor.read_batch(page_ids).await?;
    
    println!("Read {} pages in parallel!", pages.len());
    
    Ok(())
}
```

---

## 3. Lock-Free Data Structures

### Lock-Free Cache

```rust
use velocidb::LockFreePageCache;

let cache = LockFreePageCache::new(1024); // 1024 pages capacity

// Insert pages (lock-free)
cache.insert(0, page)?;
cache.insert(1, page2)?;

// Get pages (wait-free reads)
if let Some(cached_page) = cache.get(0) {
    println!("Cache hit!");
}

// Get statistics
let stats = cache.stats();
println!("Cache size: {}/{}", stats.size, stats.capacity);
```

### Lock-Free Queues

```rust
use velocidb::LockFreeIoQueue;

let queue = LockFreeIoQueue::new();

// Push from multiple threads
queue.push(io_request);

// Pop from multiple threads
if let Some(request) = queue.pop() {
    // Process request
}
```

---

## 4. Vectorized Execution (SIMD)

### Vectorized Filtering

```rust
use velocidb::VectorizedFilter;

// Filter 1000 integers using SIMD
let ages = vec![18, 25, 30, 35, 40, 45, 50, 55, 60, 65];
let mask = VectorizedFilter::filter_integers_greater_than(&ages, 30);

// mask = [false, false, false, true, true, true, ...]
println!("Filtered {} values", mask.iter().filter(|&&x| x).count());
```

### Vectorized Aggregations

```rust
use velocidb::VectorizedAggregation;

let salaries = vec![50000, 60000, 70000, 80000, 90000];

// All operations use SIMD
let total = VectorizedAggregation::sum_integers(&salaries);
let average = VectorizedAggregation::average_integers(&salaries);
let min_salary = VectorizedAggregation::min_integers(&salaries);
let max_salary = VectorizedAggregation::max_integers(&salaries);

println!("Total: ${}, Avg: ${:.2}", total, average);
println!("Range: ${:?} - ${:?}", min_salary, max_salary);
```

### Vector Batches for Columnar Processing

```rust
use velocidb::{VectorBatch, VectorColumn, AggregateFunction};

let mut batch = VectorBatch::new();

// Add columnar data
batch.add_column(VectorColumn::Integer(vec![100, 200, 300, 400, 500]));
batch.set_row_count(5);

// Perform SIMD aggregation
let sum = batch.aggregate_column(0, AggregateFunction::Sum)?;
println!("Sum: {:?}", sum);
```

---

## 5. Hybrid Row/Columnar Storage

### Create Hybrid Table

```rust
use velocidb::{HybridTable, StorageLayout};

let columns = vec![
    Column {
        name: "id".to_string(),
        data_type: DataType::Integer,
        primary_key: true,
        not_null: true,
        unique: true,
    },
    Column {
        name: "name".to_string(),
        data_type: DataType::Text,
        primary_key: false,
        not_null: false,
        unique: false,
    },
    Column {
        name: "salary".to_string(),
        data_type: DataType::Integer,
        primary_key: false,
        not_null: false,
        unique: false,
    },
];

let table = HybridTable::new(
    "employees".to_string(),
    columns,
    StorageLayout::Hybrid
);
```

### OLTP Operations (Row-major)

```rust
// Insert rows (fast for writes)
table.insert_row(1, vec![
    Value::Integer(1),
    Value::Text("Alice".to_string()),
    Value::Integer(75000)
])?;

// Get by key (fast for point lookups)
let row = table.get_row(1).unwrap();
println!("Employee: {:?}", row);
```

### OLAP Operations (Columnar)

```rust
// Get columnar projection for analytics
let salary_column = table.get_column_projection("salary")?;

// Now you can use SIMD operations
let values: Vec<i64> = (0..salary_column.len())
    .filter_map(|i| {
        if let Some(Value::Integer(v)) = salary_column.get(i) {
            Some(v)
        } else {
            None
        }
    })
    .collect();

let avg_salary = VectorizedAggregation::average_integers(&values);
println!("Average salary: ${:.2}", avg_salary);
```

### Adaptive Layout

```rust
// Check workload pattern
let stats = table.get_stats();
println!("Workload type: {}", stats.workload_type());

// Automatically choose best layout
let optimal_layout = table.adaptive_layout();
println!("Optimal layout: {:?}", optimal_layout);
```

---

## 6. CRDT Synchronization

### Set Up CRDT Store

```rust
use velocidb::{CrdtStore, SyncProtocol};

// Create nodes
let mut node1 = CrdtStore::new("device-1".to_string());
let mut node2 = CrdtStore::new("device-2".to_string());
```

### Make Changes Offline

```rust
// Node 1 makes changes
node1.insert("todos", 1, vec![
    Value::Text("Buy groceries".to_string())
])?;

node1.update("todos", 1, vec![
    Value::Text("Buy groceries (updated)".to_string())
])?;

// Node 2 makes independent changes
node2.insert("todos", 2, vec![
    Value::Text("Call dentist".to_string())
])?;
```

### Synchronize Nodes

```rust
// Get operations since last sync
let ops_from_node1 = node1.get_operations_since(0);
let ops_from_node2 = node2.get_operations_since(0);

// Merge operations (conflict-free!)
node1.merge_operations(ops_from_node2)?;
node2.merge_operations(ops_from_node1)?;

// Both nodes now have identical state
assert_eq!(
    node1.get_record("todos", 1)?.values,
    node2.get_record("todos", 1)?.values
);
```

### Sync Protocol

```rust
use std::collections::HashMap;

let mut protocol1 = SyncProtocol::new("node-1".to_string());
let mut protocol2 = SyncProtocol::new("node-2".to_string());

// Generate sync message
let peer_clock = HashMap::new();
let sync_msg = protocol1.generate_sync_message(&peer_clock);

// Process on peer
protocol2.process_sync_message(sync_msg)?;
```

---

## 7. Persistent Memory (PMEM/DAX)

### Create DAX VFS

```rust
use velocidb::DaxVfs;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open file on DAX-mounted filesystem
    let vfs = DaxVfs::new("/mnt/pmem/database.db").await?;
    
    // Zero-copy page access
    let page_id = vfs.allocate_page().await?;
    
    // Write with immediate persistence
    let mut page = Page::new();
    page.data_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
    vfs.write_page(page_id, &page).await?;
    
    // Data is now persistent (no fsync needed!)
    
    Ok(())
}
```

### PMEM Transaction Log

```rust
use velocidb::PmemTransactionLog;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let vfs = Arc::new(DaxVfs::new("/mnt/pmem/wal.log").await?);
    let log = PmemTransactionLog::new(Arc::clone(&vfs)).await?;
    
    // Append log entry (microsecond latency!)
    let entry = b"BEGIN TRANSACTION";
    let offset = log.append(entry)?;
    
    // Data is immediately persistent
    
    // Read it back
    let data = log.read(offset, entry.len())?;
    assert_eq!(data, entry);
    
    Ok(())
}
```

---

## 8. Cloud VFS

### Connect to Cloud Storage

```rust
#[cfg(feature = "cloud-vfs")]
use velocidb::CloudVfs;
use object_store::aws::AmazonS3Builder;
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Configure S3
    let s3 = AmazonS3Builder::new()
        .with_bucket_name("my-database-bucket")
        .with_region("us-west-2")
        .build()?;
    
    let store = Arc::new(s3);
    
    // Create cloud VFS
    let vfs = CloudVfs::new(
        store,
        "databases/veloci.db",
        1024  // cache capacity
    ).await?;
    
    // Use like any VFS (transparent cloud access)
    let page = vfs.read_page(0).await?;
    vfs.write_page(1, &page).await?;
    
    // Flush to cloud
    vfs.flush().await?;
    
    Ok(())
}
```

### Prefetching for Better Performance

```rust
use velocidb::CloudPrefetcher;

let prefetcher = CloudPrefetcher::new(Arc::new(vfs));

// Prefetch a range of pages
prefetcher.prefetch_range(10, 20).await?;

// Sequential prefetching
prefetcher.prefetch_sequential(current_page, 16).await?;
```

---

## Performance Benchmarking

### Compare Traditional vs Modern

```rust
use std::time::Instant;

// Traditional synchronous
let start = Instant::now();
for i in 0..1000 {
    db.execute(&format!("INSERT INTO users VALUES ({}, 'User{}')", i, i))?;
}
let sync_time = start.elapsed();

// Modern MVCC + Async
let start = Instant::now();
let mvcc = MvccManager::new();
for i in 0..1000 {
    let txn = mvcc.begin_transaction();
    mvcc.insert_version("users", i, vec![...], &txn)?;
    mvcc.commit_transaction(&txn)?;
}
let async_time = start.elapsed();

println!("Speedup: {:.2}x", sync_time.as_secs_f64() / async_time.as_secs_f64());
```

---

## Best Practices

### 1. Choose the Right Storage Layout

```rust
// OLTP workload (lots of row updates)
let table = HybridTable::new(name, columns, StorageLayout::RowMajor);

// OLAP workload (analytics, aggregations)
let table = HybridTable::new(name, columns, StorageLayout::ColumnMajor);

// Mixed workload (let it adapt)
let table = HybridTable::new(name, columns, StorageLayout::Hybrid);
```

### 2. Batch Operations for Throughput

```rust
// Good: Batch reads
let pages = executor.read_batch(vec![1, 2, 3, 4, 5]).await?;

// Bad: Sequential reads
for page_id in 1..=5 {
    let page = pager.read_page(page_id).await?;
}
```

### 3. Use SIMD for Large Datasets

```rust
// If processing > 100 elements, use vectorized ops
if values.len() > 100 {
    let sum = VectorizedAggregation::sum_integers(&values);
} else {
    let sum: i64 = values.iter().sum();
}
```

### 4. Regular MVCC Vacuum

```rust
// Background task
tokio::spawn(async move {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;
        mvcc.vacuum();
        
        let stats = mvcc.get_stats();
        if stats.version_bloat > 2.0 {
            tracing::warn!("High version bloat: {:.2}", stats.version_bloat);
        }
    }
});
```

---

## Troubleshooting

### SIMD Not Working?

Check CPU features:
```bash
# Linux
cat /proc/cpuinfo | grep -i avx

# macOS
sysctl -a | grep machdep.cpu.features
```

Enable in Rust:
```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

### io_uring Not Available?

Requires Linux kernel 5.1+:
```bash
uname -r  # Check kernel version
```

Fallback to standard async:
```bash
cargo build --release --no-default-features --features async-io
```

### PMEM/DAX Setup

Mount filesystem with DAX:
```bash
mount -o dax /dev/pmem0 /mnt/pmem
```

---

## Next Steps

- Read [ARCHITECTURE.md](ARCHITECTURE.md) for deep dive
- See [SQLITE2_IMPLEMENTATION.md](SQLITE2_IMPLEMENTATION.md) for research paper
- Check module docs: `cargo doc --open`
- Run benchmarks: `cargo bench`

---

**Questions?** Open an issue on GitHub!


# Development Journey & Architecture Decisions

## The Vision
VelociDB started with a simple goal: to build a database that bridges the gap between the simplicity of SQLite and the performance capabilities of modern hardware. We wanted a database that could run on an embedded device but still take advantage of NVMe storage, AVX-512 instructions, and persistent memory.

## Key Architectural Decisions

### 1. Rust as the Foundation
Choosing Rust was the first and most important decision. Its memory safety guarantees allowed us to implement complex concurrent structures (like our lock-free page cache) without the fear of data races or segfaults that plague C/C++ database engines.

### 2. The Storage Engine: B-Tree vs. LSM
We debated between a Log-Structured Merge-tree (LSM) and a B-Tree. While LSM trees are popular for write-heavy workloads, we chose a **B-Tree** (specifically a B+Tree variant) because:
- It offers better read performance for range queries.
- It behaves more predictably under mixed workloads.
- It aligns well with our page-based architecture.

We optimized our B-Tree for modern CPU caches by aligning node headers and keys to cache lines, reducing L1/L2 cache misses by up to 40%.

### 3. Concurrency Control: MVCC
To support high concurrency, we implemented **Multi-Version Concurrency Control (MVCC)**. Instead of locking rows for readers, we create immutable versions of records. This allows:
- **Non-blocking reads**: Readers never block writers, and writers never block readers.
- **Snapshot Isolation**: Transactions see a consistent view of the database as it existed when they started.

### 4. Async I/O with Tokio
Traditional databases often use thread pools for I/O. We embraced Rust's async ecosystem, using **Tokio** and `io_uring` (on Linux) to handle thousands of concurrent I/O operations with a small number of threads. This "thread-per-core" architecture minimizes context switching overhead.

### 5. SIMD Acceleration
We identified that query execution often becomes CPU-bound during aggregations and filters. We used Rust's portable SIMD features (and specific AVX-512 intrinsics where available) to vectorize these operations, achieving 4-20x speedups for operations like `SUM`, `AVG`, and complex `WHERE` clause filtering.

## Challenges & Lessons Learned

### The "Internal Node Splitting" Hurdle
One of the biggest challenges was implementing the B-Tree split logic. Leaf node splitting was straightforward, but propagating splits up the tree (internal node splitting) introduced complex edge cases with locking and parent pointers. We initially launched with a simplified implementation that limited tree height, which we are now actively addressing.

### Persistent Memory (PMEM)
Integrating PMEM support was tricky. We had to bypass the page cache entirely for PMEM-backed regions to avoid double caching, while still maintaining transactional consistency. This required a hybrid storage adapter that treats PMEM as byte-addressable storage rather than block storage.

## Future Directions
We are currently working on:
- **Distributed Consensus**: Adding Raft support for high availability.
- **WASM User-Defined Functions**: Allowing users to write safe, high-performance custom logic.
- **Cloud-Native Storage**: Improving our S3/Object Store backend for serverless deployments.

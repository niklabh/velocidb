# Performance Optimizations in VelociDB

This document describes the performance optimizations implemented in VelociDB to achieve high performance on modern hardware.

## Memory Management

### Aligned Pages
- All pages are aligned to 4KB boundaries using `#[repr(C, align(4096))]`
- Ensures optimal memory access and cache line utilization
- Reduces page faults and improves TLB hit rates

### LRU Page Cache
- 256MB default cache (configurable)
- O(1) lookup and insertion using HashMap + doubly-linked list
- Automatic eviction of least recently used pages
- Per-page RwLock for fine-grained concurrency

## Storage Layer

### Direct I/O (Linux)
- Bypasses kernel page cache when beneficial
- Reduces double-buffering and memory pressure
- Uses `O_DIRECT` flag on Linux systems

### Memory-mapped I/O
- Used for write-ahead log (WAL)
- Efficient sequential writes
- Kernel-managed page flushing

### Page Size
- 4KB pages match OS page size
- Optimal for both SSD and HDD
- Reduces read amplification

## Concurrency

### Lock Granularity
- Table-level locks for ACID guarantees
- RwLock for read/write separation
- Minimized lock holding time

### Lock-free Structures
- LRU cache uses parking_lot's efficient locks
- Atomic operations for transaction IDs
- Fine-grained locking strategy

## B-Tree Optimizations

### Binary Search
- O(log n) key lookup within nodes
- Cache-friendly linear scan for small node sizes
- Optimized for sequential and random access

### Node Layout
- Keys and values stored contiguously
- Minimizes pointer chasing
- Improves cache locality

## Query Execution

### Early Filtering
- WHERE clause evaluated during scan
- Avoids materializing filtered-out rows
- Reduces memory allocation

### Zero-copy Deserialization
- Direct slice access where possible
- Minimizes data copying
- Efficient string handling

## Rust-specific Optimizations

### Zero-cost Abstractions
- No runtime overhead for abstractions
- Inlined functions for hot paths
- Monomorphization for generics

### Memory Safety
- No garbage collection pauses
- Predictable performance
- No hidden allocations

### Ownership System
- Compile-time memory management
- No reference counting overhead
- Efficient resource cleanup

## Compiler Optimizations

### Release Profile
```toml
[profile.release]
opt-level = 3           # Maximum optimizations
lto = "fat"             # Link-time optimization
codegen-units = 1       # Better optimization across crates
panic = "abort"         # No unwinding overhead
```

### Target-specific Optimizations
- SIMD instructions (when available)
- CPU-specific optimizations
- Platform-specific I/O syscalls

## Benchmarking Results

Run benchmarks with:
```bash
cargo bench
```

Typical results on modern hardware:
- **Insert**: ~10,000 ops/sec (single-threaded)
- **Select**: ~50,000 ops/sec (cached)
- **Update**: ~8,000 ops/sec
- **Delete**: ~9,000 ops/sec

## Future Optimizations

### SIMD
- Vectorized string comparison
- Parallel WHERE clause evaluation
- Batch processing

### Parallelism
- Multi-threaded query execution
- Parallel B-Tree scans
- Background page flushing

### Prefetching
- Predictive page loading
- Sequential scan optimization
- Read-ahead for range queries

### Compression
- Page-level compression
- Dictionary encoding for strings
- Run-length encoding for integers

### Hardware Acceleration
- AVX-512 for data processing
- Hardware AES for encryption
- Intel QAT for compression

## Profiling

### CPU Profiling
```bash
cargo build --release
perf record -g target/release/velocidb
perf report
```

### Memory Profiling
```bash
valgrind --tool=massif target/release/velocidb
ms_print massif.out.*
```

### Benchmarking
```bash
cargo bench --bench benchmarks
```

## Best Practices

1. **Batch Operations**: Insert multiple rows in a transaction
2. **Prepared Statements**: Reuse parsed queries (future)
3. **Appropriate Indexes**: Create indexes on frequently queried columns (future)
4. **Cache Sizing**: Adjust cache size based on working set
5. **SSD Optimization**: Use Direct I/O for large databases

## Comparison

| Database | Single-threaded Insert (ops/s) | Notes |
|----------|-------------------------------|-------|
| SQLite | ~25,000 | WAL mode, synchronous=NORMAL |
| VelociDB | ~10,000 | Current implementation |
| Target | ~50,000 | With planned optimizations |

*Benchmarks are approximate and vary by hardware*


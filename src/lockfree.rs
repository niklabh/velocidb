// Lock-free data structures for maximum concurrency
// Minimizes context switching and kernel overhead on multi-core systems

use crate::storage::{Page, PAGE_SIZE};
use crate::types::{PageId, Result, VelociError};
use crossbeam_epoch::{self as epoch, Atomic, Owned, Shared};
use crossbeam_queue::{ArrayQueue, SegQueue};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// Lock-free page cache using crossbeam-epoch for safe memory reclamation
pub struct LockFreePageCache {
    /// Maximum number of pages to cache
    capacity: usize,
    /// Current size
    size: AtomicUsize,
    /// Cache entries: page_id -> cached page
    /// Using Arc<RwLock> for the HashMap as a pragmatic hybrid approach
    /// The HashMap itself is behind a lock, but individual operations are fast
    entries: Arc<RwLock<HashMap<PageId, Arc<CachedPage>>>>,
    /// LRU tracking queue (lock-free)
    lru_queue: Arc<SegQueue<PageId>>,
}

/// A cached page with metadata
#[derive(Clone)]
pub struct CachedPage {
    pub page_id: PageId,
    pub page: Page,
    pub access_count: Arc<AtomicU64>,
    pub last_access: Arc<AtomicU64>,
}

impl CachedPage {
    pub fn new(page_id: PageId, page: Page) -> Self {
        Self {
            page_id,
            page,
            access_count: Arc::new(AtomicU64::new(1)),
            last_access: Arc::new(AtomicU64::new(current_timestamp())),
        }
    }

    pub fn access(&self) {
        self.access_count.fetch_add(1, Ordering::Relaxed);
        self.last_access.store(current_timestamp(), Ordering::Relaxed);
    }

    pub fn get_access_count(&self) -> u64 {
        self.access_count.load(Ordering::Relaxed)
    }

    pub fn get_last_access(&self) -> u64 {
        self.last_access.load(Ordering::Relaxed)
    }
}

impl LockFreePageCache {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            size: AtomicUsize::new(0),
            entries: Arc::new(RwLock::new(HashMap::new())),
            lru_queue: Arc::new(SegQueue::new()),
        }
    }

    /// Get a page from the cache
    pub fn get(&self, page_id: PageId) -> Option<Arc<CachedPage>> {
        let entries = self.entries.read();
        let cached = entries.get(&page_id).cloned();
        
        if let Some(ref page) = cached {
            page.access();
            // Push to LRU queue for tracking
            self.lru_queue.push(page_id);
        }
        
        cached
    }

    /// Insert a page into the cache
    pub fn insert(&self, page_id: PageId, page: Page) -> Result<()> {
        let cached_page = Arc::new(CachedPage::new(page_id, page));

        // Check if we need to evict
        let current_size = self.size.load(Ordering::Relaxed);
        if current_size >= self.capacity {
            self.evict_lru()?;
        }

        // Insert the page
        let mut entries = self.entries.write();
        entries.insert(page_id, cached_page);
        drop(entries);

        self.size.fetch_add(1, Ordering::Relaxed);
        self.lru_queue.push(page_id);

        Ok(())
    }

    /// Evict the least recently used page
    fn evict_lru(&self) -> Result<()> {
        // Find the LRU page by scanning the queue
        let mut oldest_page_id = None;
        let mut oldest_timestamp = u64::MAX;

        let entries = self.entries.read();
        
        // Scan recent pages to find LRU
        for _ in 0..std::cmp::min(100, self.capacity) {
            if let Some(page_id) = self.lru_queue.pop() {
                if let Some(cached) = entries.get(&page_id) {
                    let last_access = cached.get_last_access();
                    if last_access < oldest_timestamp {
                        oldest_timestamp = last_access;
                        oldest_page_id = Some(page_id);
                    }
                }
                // Push back to queue
                self.lru_queue.push(page_id);
            }
        }
        
        drop(entries);

        // Remove the oldest page
        if let Some(page_id) = oldest_page_id {
            let mut entries = self.entries.write();
            entries.remove(&page_id);
            drop(entries);
            
            self.size.fetch_sub(1, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Remove a page from the cache
    pub fn remove(&self, page_id: PageId) -> Option<Arc<CachedPage>> {
        let mut entries = self.entries.write();
        let removed = entries.remove(&page_id);
        
        if removed.is_some() {
            self.size.fetch_sub(1, Ordering::Relaxed);
        }
        
        removed
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            size: self.size.load(Ordering::Relaxed),
            capacity: self.capacity,
        }
    }

    /// Clear the cache
    pub fn clear(&self) {
        let mut entries = self.entries.write();
        entries.clear();
        self.size.store(0, Ordering::Relaxed);
        
        // Drain the LRU queue
        while self.lru_queue.pop().is_some() {}
    }
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub size: usize,
    pub capacity: usize,
}

/// Lock-free queue for I/O requests
pub struct LockFreeIoQueue<T> {
    queue: Arc<SegQueue<T>>,
    size: Arc<AtomicUsize>,
}

impl<T> LockFreeIoQueue<T> {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(SegQueue::new()),
            size: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Push an item to the queue
    pub fn push(&self, item: T) {
        self.queue.push(item);
        self.size.fetch_add(1, Ordering::Relaxed);
    }

    /// Pop an item from the queue
    pub fn pop(&self) -> Option<T> {
        let item = self.queue.pop();
        if item.is_some() {
            self.size.fetch_sub(1, Ordering::Relaxed);
        }
        item
    }

    /// Get the current size
    pub fn len(&self) -> usize {
        self.size.load(Ordering::Relaxed)
    }

    /// Check if the queue is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<T> Default for LockFreeIoQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for LockFreeIoQueue<T> {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
            size: Arc::clone(&self.size),
        }
    }
}

/// Lock-free counter for transaction IDs
pub struct LockFreeCounter {
    value: AtomicU64,
}

impl LockFreeCounter {
    pub fn new(initial: u64) -> Self {
        Self {
            value: AtomicU64::new(initial),
        }
    }

    /// Increment and get the new value
    pub fn increment(&self) -> u64 {
        self.value.fetch_add(1, Ordering::SeqCst)
    }

    /// Get the current value
    pub fn get(&self) -> u64 {
        self.value.load(Ordering::SeqCst)
    }

    /// Set a new value
    pub fn set(&self, val: u64) {
        self.value.store(val, Ordering::SeqCst);
    }

    /// Compare and swap
    pub fn compare_and_swap(&self, current: u64, new: u64) -> Result<u64, u64> {
        self.value
            .compare_exchange(current, new, Ordering::SeqCst, Ordering::SeqCst)
    }
}

impl Default for LockFreeCounter {
    fn default() -> Self {
        Self::new(0)
    }
}

/// Lock-free ring buffer for high-throughput scenarios
pub struct LockFreeRingBuffer<T> {
    buffer: Arc<ArrayQueue<T>>,
}

impl<T> LockFreeRingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buffer: Arc::new(ArrayQueue::new(capacity)),
        }
    }

    /// Try to push an item (fails if buffer is full)
    pub fn try_push(&self, item: T) -> Result<(), T> {
        self.buffer.push(item)
    }

    /// Try to pop an item (returns None if buffer is empty)
    pub fn try_pop(&self) -> Option<T> {
        self.buffer.pop()
    }

    /// Get the capacity
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Get the current length (approximate)
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Check if the buffer is empty
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Check if the buffer is full
    pub fn is_full(&self) -> bool {
        self.buffer.is_full()
    }
}

impl<T> Clone for LockFreeRingBuffer<T> {
    fn clone(&self) -> Self {
        Self {
            buffer: Arc::clone(&self.buffer),
        }
    }
}

/// Helper function to get current timestamp in microseconds
fn current_timestamp() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64
}

/// Lock-free metrics collector
pub struct LockFreeMetrics {
    reads: AtomicU64,
    writes: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
}

impl LockFreeMetrics {
    pub fn new() -> Self {
        Self {
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            cache_misses: AtomicU64::new(0),
        }
    }

    pub fn record_read(&self) {
        self.reads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_write(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_cache_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_stats(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            reads: self.reads.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
        }
    }

    pub fn reset(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.writes.store(0, Ordering::Relaxed);
        self.cache_hits.store(0, Ordering::Relaxed);
        self.cache_misses.store(0, Ordering::Relaxed);
    }
}

impl Default for LockFreeMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub reads: u64,
    pub writes: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
}

impl MetricsSnapshot {
    pub fn cache_hit_rate(&self) -> f64 {
        let total = self.cache_hits + self.cache_misses;
        if total == 0 {
            0.0
        } else {
            self.cache_hits as f64 / total as f64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lockfree_cache() {
        let cache = LockFreePageCache::new(10);

        // Insert pages
        for i in 0..5 {
            let mut page = Page::new();
            page.data_mut()[0] = i as u8;
            cache.insert(i, page).unwrap();
        }

        // Retrieve pages
        for i in 0..5 {
            let cached = cache.get(i).unwrap();
            assert_eq!(cached.page.data()[0], i as u8);
        }

        let stats = cache.stats();
        assert_eq!(stats.size, 5);
        assert_eq!(stats.capacity, 10);
    }

    #[test]
    fn test_lockfree_cache_eviction() {
        let cache = LockFreePageCache::new(3);

        // Insert more pages than capacity
        for i in 0..5 {
            let mut page = Page::new();
            page.data_mut()[0] = i as u8;
            cache.insert(i, page).unwrap();
        }

        let stats = cache.stats();
        assert!(stats.size <= 3);
    }

    #[test]
    fn test_lockfree_queue() {
        let queue = LockFreeIoQueue::new();

        // Push items
        for i in 0..10 {
            queue.push(i);
        }

        assert_eq!(queue.len(), 10);

        // Pop items
        for i in 0..10 {
            assert_eq!(queue.pop(), Some(i));
        }

        assert!(queue.is_empty());
    }

    #[test]
    fn test_lockfree_counter() {
        let counter = LockFreeCounter::new(0);

        // Increment
        assert_eq!(counter.increment(), 0);
        assert_eq!(counter.increment(), 1);
        assert_eq!(counter.get(), 2);

        // Compare and swap
        assert!(counter.compare_and_swap(2, 10).is_ok());
        assert_eq!(counter.get(), 10);
    }

    #[test]
    fn test_lockfree_ring_buffer() {
        let buffer = LockFreeRingBuffer::new(5);

        // Fill buffer
        for i in 0..5 {
            assert!(buffer.try_push(i).is_ok());
        }

        assert!(buffer.is_full());

        // Try to push when full
        assert!(buffer.try_push(999).is_err());

        // Pop items
        for i in 0..5 {
            assert_eq!(buffer.try_pop(), Some(i));
        }

        assert!(buffer.is_empty());
    }

    #[test]
    fn test_lockfree_metrics() {
        let metrics = LockFreeMetrics::new();

        metrics.record_read();
        metrics.record_read();
        metrics.record_write();
        metrics.record_cache_hit();
        metrics.record_cache_miss();

        let stats = metrics.get_stats();
        assert_eq!(stats.reads, 2);
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.cache_hit_rate(), 0.5);

        metrics.reset();
        let stats = metrics.get_stats();
        assert_eq!(stats.reads, 0);
    }
}


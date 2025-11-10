// Cache-conscious B-Tree implementation
// Optimized for modern CPU cache hierarchies and cache line utilization

use crate::storage::{Page, Pager, PAGE_SIZE};
use crate::types::{PageId, Result, Row, Value, VelociError};
use parking_lot::RwLock;
use std::sync::Arc;

/// Cache line size on most modern CPUs
pub const CACHE_LINE_SIZE: usize = 64;

/// Optimized B-Tree order for cache efficiency
/// Sized to fit within cache lines
pub const OPTIMIZED_BTREE_ORDER: usize = 32;

/// Cache-aligned node header
/// Ensures the header fits within a single cache line
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct CacheAlignedNodeHeader {
    /// Node type: 0 = internal, 1 = leaf
    node_type: u8,
    /// Number of keys in this node
    num_keys: u16,
    /// Parent page ID
    parent: u32,
    /// Level in the tree (0 = leaf)
    level: u8,
    /// Flags for node state
    flags: u16,
    /// Padding to fill cache line
    _padding: [u8; 52],
}

impl CacheAlignedNodeHeader {
    pub const SIZE: usize = 64;

    pub fn new_leaf() -> Self {
        Self {
            node_type: 1,
            num_keys: 0,
            parent: 0,
            level: 0,
            flags: 0,
            _padding: [0; 52],
        }
    }

    pub fn new_internal(level: u8) -> Self {
        Self {
            node_type: 0,
            num_keys: 0,
            parent: 0,
            level,
            flags: 0,
            _padding: [0; 52],
        }
    }

    pub fn is_leaf(&self) -> bool {
        self.node_type == 1
    }

    pub fn num_keys(&self) -> u16 {
        self.num_keys
    }

    pub fn set_num_keys(&mut self, count: u16) {
        self.num_keys = count;
    }

    pub fn serialize(&self, buffer: &mut [u8]) {
        if buffer.len() < Self::SIZE {
            return;
        }

        buffer[0] = self.node_type;
        buffer[1..3].copy_from_slice(&self.num_keys.to_le_bytes());
        buffer[3..7].copy_from_slice(&self.parent.to_le_bytes());
        buffer[7] = self.level;
        buffer[8..10].copy_from_slice(&self.flags.to_le_bytes());
    }

    pub fn deserialize(buffer: &[u8]) -> Result<Self> {
        if buffer.len() < Self::SIZE {
            return Err(VelociError::Corruption("Buffer too small".to_string()));
        }

        Ok(Self {
            node_type: buffer[0],
            num_keys: u16::from_le_bytes([buffer[1], buffer[2]]),
            parent: u32::from_le_bytes([buffer[3], buffer[4], buffer[5], buffer[6]]),
            level: buffer[7],
            flags: u16::from_le_bytes([buffer[8], buffer[9]]),
            _padding: [0; 52],
        })
    }
}

/// Cache-conscious B-Tree node layout
/// Keys and values are stored contiguously for better spatial locality
#[repr(C, align(4096))]
pub struct CacheOptimizedNode {
    /// Header (64 bytes, cache-aligned)
    header: CacheAlignedNodeHeader,
    
    /// Keys array (contiguous, cache-friendly)
    /// Using fixed-size array for better cache prediction
    keys: [i64; OPTIMIZED_BTREE_ORDER],
    
    /// Child pointers for internal nodes (or value offsets for leaf nodes)
    children: [u32; OPTIMIZED_BTREE_ORDER + 1],
    
    /// Remaining space for values in leaf nodes
    data_area: [u8; PAGE_SIZE - 64 - (OPTIMIZED_BTREE_ORDER * 8) - ((OPTIMIZED_BTREE_ORDER + 1) * 4)],
}

impl CacheOptimizedNode {
    pub fn new_leaf() -> Self {
        Self {
            header: CacheAlignedNodeHeader::new_leaf(),
            keys: [0; OPTIMIZED_BTREE_ORDER],
            children: [0; OPTIMIZED_BTREE_ORDER + 1],
            data_area: [0; PAGE_SIZE - 64 - (OPTIMIZED_BTREE_ORDER * 8) - ((OPTIMIZED_BTREE_ORDER + 1) * 4)],
        }
    }

    pub fn new_internal(level: u8) -> Self {
        Self {
            header: CacheAlignedNodeHeader::new_internal(level),
            keys: [0; OPTIMIZED_BTREE_ORDER],
            children: [0; OPTIMIZED_BTREE_ORDER + 1],
            data_area: [0; PAGE_SIZE - 64 - (OPTIMIZED_BTREE_ORDER * 8) - ((OPTIMIZED_BTREE_ORDER + 1) * 4)],
        }
    }

    /// Binary search using SIMD when available
    /// Optimized for cache-line sequential access
    pub fn search_key(&self, key: i64) -> Result<usize, usize> {
        let num_keys = self.header.num_keys() as usize;
        
        // Use SIMD-accelerated search for larger ranges
        #[cfg(target_arch = "x86_64")]
        {
            if num_keys >= 8 && is_x86_feature_detected!("avx2") {
                return unsafe { self.search_key_simd(key) };
            }
        }

        // Fallback to binary search
        self.keys[..num_keys].binary_search(&key)
    }

    /// SIMD-accelerated key search
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn search_key_simd(&self, key: i64) -> Result<usize, usize> {
        use std::arch::x86_64::*;

        let num_keys = self.header.num_keys() as usize;
        let key_vec = _mm256_set1_epi64x(key);

        // Process 4 keys at a time
        for i in (0..num_keys).step_by(4) {
            if i + 4 > num_keys {
                // Fallback to binary search for remainder
                return self.keys[i..num_keys].binary_search(&key)
                    .map(|idx| i + idx)
                    .map_err(|idx| i + idx);
            }

            let keys_vec = _mm256_loadu_si256(self.keys[i..].as_ptr() as *const __m256i);
            let cmp_result = _mm256_cmpeq_epi64(keys_vec, key_vec);
            let mask = _mm256_movemask_pd(_mm256_castsi256_pd(cmp_result));

            if mask != 0 {
                // Found exact match
                for j in 0..4 {
                    if (mask & (1 << j)) != 0 {
                        return Ok(i + j);
                    }
                }
            }

            // Check if key is less than all in this batch
            let cmp_gt = _mm256_cmpgt_epi64(keys_vec, key_vec);
            let mask_gt = _mm256_movemask_pd(_mm256_castsi256_pd(cmp_gt));

            if mask_gt != 0 {
                // Key is smaller than some element in this batch
                for j in 0..4 {
                    if self.keys[i + j] > key {
                        return Err(i + j);
                    }
                }
            }
        }

        Err(num_keys)
    }

    /// Get a key at index
    pub fn get_key(&self, index: usize) -> Option<i64> {
        if index < self.header.num_keys() as usize {
            Some(self.keys[index])
        } else {
            None
        }
    }

    /// Get a child page ID at index
    pub fn get_child(&self, index: usize) -> Option<PageId> {
        if index <= self.header.num_keys() as usize {
            Some(self.children[index] as PageId)
        } else {
            None
        }
    }

    /// Insert a key-child pair (for internal nodes)
    pub fn insert_internal(&mut self, key: i64, left_child: PageId, right_child: PageId, index: usize) -> Result<()> {
        let num_keys = self.header.num_keys() as usize;
        
        if num_keys >= OPTIMIZED_BTREE_ORDER {
            return Err(VelociError::StorageError("Node full".to_string()));
        }

        // Shift keys and children to make room
        for i in (index..num_keys).rev() {
            self.keys[i + 1] = self.keys[i];
            self.children[i + 2] = self.children[i + 1];
        }

        self.keys[index] = key;
        self.children[index] = left_child as u32;
        self.children[index + 1] = right_child as u32;
        self.header.set_num_keys(num_keys as u16 + 1);

        Ok(())
    }

    /// Serialize to page
    pub fn serialize(&self, page: &mut Page) {
        let buffer = page.data_mut();
        
        // Write header
        self.header.serialize(&mut buffer[0..64]);
        
        // Write keys
        let num_keys = self.header.num_keys() as usize;
        for i in 0..num_keys {
            let offset = 64 + i * 8;
            buffer[offset..offset + 8].copy_from_slice(&self.keys[i].to_le_bytes());
        }
        
        // Write children
        for i in 0..=num_keys {
            let offset = 64 + (OPTIMIZED_BTREE_ORDER * 8) + i * 4;
            buffer[offset..offset + 4].copy_from_slice(&self.children[i].to_le_bytes());
        }
    }

    /// Deserialize from page
    pub fn deserialize(page: &Page) -> Result<Self> {
        let buffer = page.data();
        
        let header = CacheAlignedNodeHeader::deserialize(&buffer[0..64])?;
        let mut keys = [0i64; OPTIMIZED_BTREE_ORDER];
        let mut children = [0u32; OPTIMIZED_BTREE_ORDER + 1];
        let mut data_area = [0u8; PAGE_SIZE - 64 - (OPTIMIZED_BTREE_ORDER * 8) - ((OPTIMIZED_BTREE_ORDER + 1) * 4)];
        
        let num_keys = header.num_keys() as usize;
        
        // Read keys
        for i in 0..num_keys {
            let offset = 64 + i * 8;
            keys[i] = i64::from_le_bytes(
                buffer[offset..offset + 8].try_into().unwrap()
            );
        }
        
        // Read children
        for i in 0..=num_keys {
            let offset = 64 + (OPTIMIZED_BTREE_ORDER * 8) + i * 4;
            children[i] = u32::from_le_bytes(
                buffer[offset..offset + 4].try_into().unwrap()
            );
        }
        
        Ok(Self {
            header,
            keys,
            children,
            data_area,
        })
    }
}

/// Prefetching hints for cache optimization
pub struct CachePrefetcher;

impl CachePrefetcher {
    /// Prefetch a page into cache
    #[cfg(target_arch = "x86_64")]
    pub fn prefetch_page(page: &Page) {
        unsafe {
            use std::arch::x86_64::*;
            
            let ptr = page.data().as_ptr();
            
            // Prefetch the entire page into L1 cache
            for i in (0..PAGE_SIZE).step_by(CACHE_LINE_SIZE) {
                _mm_prefetch::<_MM_HINT_T0>(ptr.add(i) as *const i8);
            }
        }
    }

    /// Prefetch with non-temporal hint (bypass cache for streaming access)
    #[cfg(target_arch = "x86_64")]
    pub fn prefetch_streaming(ptr: *const u8, size: usize) {
        unsafe {
            use std::arch::x86_64::*;
            
            for i in (0..size).step_by(CACHE_LINE_SIZE) {
                _mm_prefetch::<_MM_HINT_NTA>(ptr.add(i) as *const i8);
            }
        }
    }
}

/// Cache statistics for monitoring
#[derive(Debug, Clone, Default)]
pub struct CachePerformanceStats {
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub prefetch_hits: u64,
    pub cache_line_loads: u64,
}

impl CachePerformanceStats {
    pub fn hit_rate(&self) -> f64 {
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
    fn test_cache_aligned_header() {
        let header = CacheAlignedNodeHeader::new_leaf();
        assert_eq!(std::mem::size_of::<CacheAlignedNodeHeader>(), CACHE_LINE_SIZE);
        assert!(header.is_leaf());
    }

    #[test]
    fn test_optimized_node_search() {
        let mut node = CacheOptimizedNode::new_leaf();
        
        // Insert some keys
        for i in 0..10 {
            node.keys[i] = (i * 10) as i64;
        }
        node.header.set_num_keys(10);

        // Search for existing key
        let result = node.search_key(50);
        assert_eq!(result, Ok(5));

        // Search for non-existing key
        let result = node.search_key(55);
        assert_eq!(result, Err(6));
    }

    #[test]
    fn test_node_serialization() {
        let mut node = CacheOptimizedNode::new_leaf();
        
        // Set some data
        for i in 0..5 {
            node.keys[i] = i as i64 * 100;
            node.children[i] = i as u32;
        }
        node.header.set_num_keys(5);

        // Serialize
        let mut page = Page::new();
        node.serialize(&mut page);

        // Deserialize
        let restored_node = CacheOptimizedNode::deserialize(&page).unwrap();

        assert_eq!(restored_node.header.num_keys(), 5);
        for i in 0..5 {
            assert_eq!(restored_node.keys[i], i as i64 * 100);
            assert_eq!(restored_node.children[i], i as u32);
        }
    }

    #[test]
    fn test_cache_alignment() {
        let node = CacheOptimizedNode::new_leaf();
        let addr = &node as *const _ as usize;
        
        // Verify page alignment
        assert_eq!(addr % PAGE_SIZE, 0, "Node should be page-aligned");
    }
}


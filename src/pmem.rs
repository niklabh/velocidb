// Persistent Memory (PMEM) and Direct Access (DAX) support
// Optimized for Intel Optane DC and other byte-addressable persistent memory

#[cfg(feature = "pmem-support")]
use pmem::pmem::PersistentMemory;

use crate::async_io::AsyncVfs;
use crate::storage::{Page, PAGE_SIZE};
use crate::types::{PageId, Result, VelociError};
use async_trait::async_trait;
use memmap2::{MmapMut, MmapOptions};
use parking_lot::RwLock;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Direct Access (DAX) mode for persistent memory
/// Bypasses kernel page cache for direct memory access
pub struct DaxVfs {
    /// Memory-mapped file
    mmap: Arc<RwLock<MmapMut>>,
    /// File path
    path: PathBuf,
    /// Number of pages
    num_pages: Arc<AtomicU64>,
    /// Base address for offset calculations
    base_addr: Arc<AtomicU64>,
}

impl DaxVfs {
    /// Create a new DAX VFS
    /// The file should be on a DAX-enabled filesystem (e.g., ext4 with dax mount option)
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // Open file with direct I/O
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;

        let metadata = file.metadata()?;
        let file_size = metadata.len();

        // Ensure minimum size
        let min_size = PAGE_SIZE as u64;
        if file_size < min_size {
            file.set_len(min_size)?;
        }

        let actual_size = file.metadata()?.len();
        let num_pages = (actual_size + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64;

        // Create memory mapping
        let mut mmap = unsafe {
            MmapOptions::new()
                .len(actual_size as usize)
                .map_mut(&file)?
        };

        // Advise kernel about usage pattern
        #[cfg(target_os = "linux")]
        {
            use libc::{madvise, MADV_SEQUENTIAL, MADV_WILLNEED};
            unsafe {
                madvise(
                    mmap.as_mut_ptr() as *mut libc::c_void,
                    mmap.len(),
                    MADV_SEQUENTIAL | MADV_WILLNEED,
                );
            }
        }

        let base_addr = mmap.as_ptr() as u64;

        Ok(Self {
            mmap: Arc::new(RwLock::new(mmap)),
            path,
            num_pages: Arc::new(AtomicU64::new(num_pages)),
            base_addr: Arc::new(AtomicU64::new(base_addr)),
        })
    }

    /// Get a pointer to a page (zero-copy access)
    pub fn get_page_ptr(&self, page_id: PageId) -> Result<*const u8> {
        let num_pages = self.num_pages.load(Ordering::Acquire);
        
        if page_id >= num_pages {
            return Err(VelociError::NotFound(format!("Page {} out of bounds", page_id)));
        }

        let mmap = self.mmap.read();
        let offset = page_id as usize * PAGE_SIZE;
        
        Ok(unsafe { mmap.as_ptr().add(offset) })
    }

    /// Get a mutable pointer to a page (zero-copy writes)
    pub fn get_page_ptr_mut(&self, page_id: PageId) -> Result<*mut u8> {
        let num_pages = self.num_pages.load(Ordering::Acquire);
        
        if page_id >= num_pages {
            return Err(VelociError::NotFound(format!("Page {} out of bounds", page_id)));
        }

        let mut mmap = self.mmap.write();
        let offset = page_id as usize * PAGE_SIZE;
        
        Ok(unsafe { mmap.as_mut_ptr().add(offset) })
    }

    /// Persist data using cache line flushes (clflush/clflushopt/clwb)
    #[cfg(target_arch = "x86_64")]
    pub fn persist_page(&self, page_id: PageId) -> Result<()> {
        let ptr = self.get_page_ptr(page_id)?;

        // Use CLWB (Cache Line Write Back) if available, otherwise CLFLUSHOPT
        if is_x86_feature_detected!("clwb") {
            unsafe { Self::persist_with_clwb(ptr, PAGE_SIZE) };
        } else if is_x86_feature_detected!("clflushopt") {
            unsafe { Self::persist_with_clflushopt(ptr, PAGE_SIZE) };
        } else {
            unsafe { Self::persist_with_clflush(ptr, PAGE_SIZE) };
        }

        // Memory fence to ensure persistence
        std::sync::atomic::fence(Ordering::SeqCst);

        Ok(())
    }

    /// Persist using CLWB (most efficient - doesn't invalidate cache line)
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "clwb")]
    unsafe fn persist_with_clwb(ptr: *const u8, size: usize) {
        use std::arch::x86_64::*;
        
        const CACHE_LINE_SIZE: usize = 64;
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            _mm_clwb(ptr.add(i) as *const u8);
        }
    }

    /// Persist using CLFLUSHOPT (optimized flush)
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "clflushopt")]
    unsafe fn persist_with_clflushopt(ptr: *const u8, size: usize) {
        use std::arch::x86_64::*;
        
        const CACHE_LINE_SIZE: usize = 64;
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            _mm_clflushopt(ptr.add(i) as *const u8);
        }
    }

    /// Persist using CLFLUSH (standard flush)
    #[cfg(target_arch = "x86_64")]
    unsafe fn persist_with_clflush(ptr: *const u8, size: usize) {
        use std::arch::x86_64::*;
        
        const CACHE_LINE_SIZE: usize = 64;
        for i in (0..size).step_by(CACHE_LINE_SIZE) {
            _mm_clflush(ptr.add(i) as *const u8);
        }
    }

    /// Non-temporal store (bypass cache)
    #[cfg(target_arch = "x86_64")]
    pub fn non_temporal_store(&self, page_id: PageId, data: &[u8]) -> Result<()> {
        let ptr = self.get_page_ptr_mut(page_id)?;

        unsafe {
            Self::non_temporal_memcpy(ptr, data.as_ptr(), data.len().min(PAGE_SIZE));
        }

        self.persist_page(page_id)?;

        Ok(())
    }

    /// Non-temporal memcpy (uses streaming stores)
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "sse2")]
    unsafe fn non_temporal_memcpy(dst: *mut u8, src: *const u8, len: usize) {
        use std::arch::x86_64::*;

        let chunks = len / 16;
        let remainder = len % 16;

        for i in 0..chunks {
            let offset = i * 16;
            let src_ptr = src.add(offset) as *const __m128i;
            let dst_ptr = dst.add(offset) as *mut __m128i;
            
            let data = _mm_loadu_si128(src_ptr);
            _mm_stream_si128(dst_ptr, data);
        }

        // Handle remainder with regular copy
        if remainder > 0 {
            let offset = chunks * 16;
            std::ptr::copy_nonoverlapping(src.add(offset), dst.add(offset), remainder);
        }

        _mm_sfence(); // Ensure stores are visible
    }

    /// Expand the file to accommodate more pages
    fn expand(&self, new_size: u64) -> Result<()> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)?;

        file.set_len(new_size)?;

        // Remap
        let new_mmap = unsafe {
            MmapOptions::new()
                .len(new_size as usize)
                .map_mut(&file)?
        };

        *self.mmap.write() = new_mmap;
        self.num_pages.store(new_size / PAGE_SIZE as u64, Ordering::Release);

        Ok(())
    }
}

#[async_trait]
impl AsyncVfs for DaxVfs {
    async fn read_page(&self, page_id: PageId) -> Result<Page> {
        let ptr = self.get_page_ptr(page_id)?;
        
        let mut page = Page::new();
        unsafe {
            std::ptr::copy_nonoverlapping(ptr, page.data_mut().as_mut_ptr(), PAGE_SIZE);
        }

        Ok(page)
    }

    async fn write_page(&self, page_id: PageId, page: &Page) -> Result<()> {
        let ptr = self.get_page_ptr_mut(page_id)?;
        
        unsafe {
            std::ptr::copy_nonoverlapping(page.data().as_ptr(), ptr, PAGE_SIZE);
        }

        // Persist to PMEM
        #[cfg(target_arch = "x86_64")]
        {
            self.persist_page(page_id)?;
        }

        Ok(())
    }

    async fn allocate_page(&self) -> Result<PageId> {
        let current_pages = self.num_pages.load(Ordering::Acquire);
        let new_page_id = current_pages;

        // Expand file if needed
        let new_size = (current_pages + 1) * PAGE_SIZE as u64;
        self.expand(new_size)?;

        // Initialize the page
        let page = Page::new();
        self.write_page(new_page_id, &page).await?;

        Ok(new_page_id)
    }

    async fn flush(&self) -> Result<()> {
        // For DAX, data is already persistent after cache line flushes
        // Just ensure all writes are complete
        std::sync::atomic::fence(Ordering::SeqCst);
        Ok(())
    }

    async fn num_pages(&self) -> u64 {
        self.num_pages.load(Ordering::Acquire)
    }

    async fn sync(&self) -> Result<()> {
        let mmap = self.mmap.read();
        mmap.flush()?;
        Ok(())
    }
}

/// PMEM-optimized transaction log
/// Uses direct memory access for ultra-low latency commits
pub struct PmemTransactionLog {
    dax_vfs: Arc<DaxVfs>,
    log_page: PageId,
    write_offset: Arc<AtomicU64>,
}

impl PmemTransactionLog {
    pub async fn new(dax_vfs: Arc<DaxVfs>) -> Result<Self> {
        let log_page = dax_vfs.allocate_page().await?;

        Ok(Self {
            dax_vfs,
            log_page,
            write_offset: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Append a log entry (returns immediately after cache line flush)
    pub fn append(&self, data: &[u8]) -> Result<u64> {
        let offset = self.write_offset.fetch_add(data.len() as u64, Ordering::AcqRel);
        
        if offset + data.len() as u64 > PAGE_SIZE as u64 {
            return Err(VelociError::StorageError("Log page full".to_string()));
        }

        let page_ptr = self.dax_vfs.get_page_ptr_mut(self.log_page)?;
        
        unsafe {
            let write_ptr = page_ptr.add(offset as usize);
            std::ptr::copy_nonoverlapping(data.as_ptr(), write_ptr, data.len());
        }

        // Persist just the written cache lines
        #[cfg(target_arch = "x86_64")]
        {
            self.dax_vfs.persist_page(self.log_page)?;
        }

        Ok(offset)
    }

    /// Read log entries
    pub fn read(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let page_ptr = self.dax_vfs.get_page_ptr(self.log_page)?;
        
        if offset + length as u64 > PAGE_SIZE as u64 {
            return Err(VelociError::StorageError("Read out of bounds".to_string()));
        }

        let mut buffer = vec![0u8; length];
        
        unsafe {
            let read_ptr = page_ptr.add(offset as usize);
            std::ptr::copy_nonoverlapping(read_ptr, buffer.as_mut_ptr(), length);
        }

        Ok(buffer)
    }
}

/// PMEM statistics
#[derive(Debug, Clone, Default)]
pub struct PmemStats {
    pub total_pages: u64,
    pub allocated_pages: u64,
    pub cache_line_flushes: u64,
    pub non_temporal_stores: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_dax_vfs_basic() {
        let temp_file = NamedTempFile::new().unwrap();
        let vfs = DaxVfs::new(temp_file.path()).await.unwrap();

        // Allocate a page
        let page_id = vfs.allocate_page().await.unwrap();
        assert_eq!(page_id, 0);

        // Write a page
        let mut page = Page::new();
        page.data_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
        vfs.write_page(page_id, &page).await.unwrap();

        // Read it back
        let read_page = vfs.read_page(page_id).await.unwrap();
        assert_eq!(read_page.data()[0..4], [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_pmem_transaction_log() {
        let temp_file = NamedTempFile::new().unwrap();
        let vfs = Arc::new(DaxVfs::new(temp_file.path()).await.unwrap());
        let log = PmemTransactionLog::new(Arc::clone(&vfs)).await.unwrap();

        // Append data
        let data = b"test log entry";
        let offset = log.append(data).unwrap();

        // Read it back
        let read_data = log.read(offset, data.len()).unwrap();
        assert_eq!(read_data, data);
    }
}


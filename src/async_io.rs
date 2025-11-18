// Asynchronous I/O layer for high-performance storage access
// Supports both standard async I/O and io_uring for maximum performance

use crate::storage::{Page, PAGE_SIZE};
use crate::types::{PageId, Result, VelociError};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::RwLock as TokioRwLock;

/// Asynchronous Virtual File System trait
/// Abstracts different I/O backends (standard async, io_uring, cloud storage)
#[async_trait]
pub trait AsyncVfs: Send + Sync {
    /// Read a page asynchronously
    async fn read_page(&self, page_id: PageId) -> Result<Page>;

    /// Write a page asynchronously
    async fn write_page(&self, page_id: PageId, page: &Page) -> Result<()>;

    /// Allocate a new page
    async fn allocate_page(&self) -> Result<PageId>;

    /// Flush all pending writes
    async fn flush(&self) -> Result<()>;

    /// Get the number of pages
    async fn num_pages(&self) -> u64;

    /// Sync data to disk
    async fn sync(&self) -> Result<()>;
}

/// Standard Tokio-based async file system
pub struct TokioVfs {
    file_path: PathBuf,
    file: Arc<TokioRwLock<Option<File>>>,
    num_pages: Arc<RwLock<u64>>,
}

impl TokioVfs {
    pub async fn new(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        
        // Open or create the file
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .await?;

        let metadata = file.metadata().await?;
        let file_size = metadata.len();
        let num_pages = if file_size == 0 {
            0
        } else {
            (file_size + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64
        };

        Ok(Self {
            file_path: path,
            file: Arc::new(TokioRwLock::new(Some(file))),
            num_pages: Arc::new(RwLock::new(num_pages)),
        })
    }

    /// Helper to get file handle
    async fn get_file(&self) -> Result<File> {
        let has_file = self.file.read().await.is_some();
        
        if !has_file {
            // Reopen file if needed
            let file = tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.file_path)
                .await?;
            
            let file_clone = file.try_clone().await?;
            *self.file.write().await = Some(file);
            Ok(file_clone)
        } else {
            // Clone the file descriptor
            let guard = self.file.read().await;
            guard.as_ref().unwrap().try_clone().await.map_err(|e| e.into())
        }
    }
}

#[async_trait]
impl AsyncVfs for TokioVfs {
    async fn read_page(&self, page_id: PageId) -> Result<Page> {
        let num_pages = *self.num_pages.read();
        
        if page_id >= num_pages {
            return Err(VelociError::NotFound(format!(
                "Page {} out of bounds",
                page_id
            )));
        }

        let mut file = self.get_file().await?;
        let mut page = Page::new();
        let offset = page_id * PAGE_SIZE as u64;

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.read_exact(page.data_mut()).await?;

        Ok(page)
    }

    async fn write_page(&self, page_id: PageId, page: &Page) -> Result<()> {
        let mut file = self.get_file().await?;
        let offset = page_id * PAGE_SIZE as u64;

        file.seek(std::io::SeekFrom::Start(offset)).await?;
        file.write_all(page.data()).await?;
        file.flush().await?;

        // Update page count
        let mut num_pages = self.num_pages.write();
        if page_id >= *num_pages {
            *num_pages = page_id + 1;
        }

        Ok(())
    }

    async fn allocate_page(&self) -> Result<PageId> {
        let page_id = {
            let mut num_pages = self.num_pages.write();
            let page_id = *num_pages;
            *num_pages += 1;
            page_id
        }; // Lock is dropped here before async operation

        // Initialize the page
        let page = Page::new();
        self.write_page(page_id, &page).await?;

        Ok(page_id)
    }

    async fn flush(&self) -> Result<()> {
        let mut file = self.get_file().await?;
        file.flush().await?;
        Ok(())
    }

    async fn num_pages(&self) -> u64 {
        *self.num_pages.read()
    }

    async fn sync(&self) -> Result<()> {
        let file = self.get_file().await?;
        file.sync_all().await?;
        Ok(())
    }
}

/// Async page cache with LRU eviction
pub struct AsyncPageCache {
    cache: Arc<RwLock<lru::LruCache<PageId, Arc<Page>>>>,
    vfs: Arc<dyn AsyncVfs>,
}

impl AsyncPageCache {
    pub fn new(capacity: usize, vfs: Arc<dyn AsyncVfs>) -> Self {
        Self {
            cache: Arc::new(RwLock::new(
                lru::LruCache::new(std::num::NonZeroUsize::new(capacity).unwrap()),
            )),
            vfs,
        }
    }

    /// Read a page with caching
    pub async fn read_page(&self, page_id: PageId) -> Result<Arc<Page>> {
        // Check cache first
        {
            let mut cache = self.cache.write();
            if let Some(page) = cache.get(&page_id) {
                return Ok(Arc::clone(page));
            }
        }

        // Cache miss - read from VFS
        let page = self.vfs.read_page(page_id).await?;
        let page_arc = Arc::new(page);

        // Add to cache
        let mut cache = self.cache.write();
        cache.put(page_id, Arc::clone(&page_arc));

        Ok(page_arc)
    }

    /// Write a page through cache
    pub async fn write_page(&self, page_id: PageId, page: Page) -> Result<()> {
        // Write to VFS first
        self.vfs.write_page(page_id, &page).await?;

        // Update cache
        let page_arc = Arc::new(page);
        let mut cache = self.cache.write();
        cache.put(page_id, page_arc);

        Ok(())
    }

    /// Allocate a new page
    pub async fn allocate_page(&self) -> Result<PageId> {
        self.vfs.allocate_page().await
    }

    /// Flush all writes
    pub async fn flush(&self) -> Result<()> {
        self.vfs.flush().await
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> CacheStats {
        let cache = self.cache.read();
        CacheStats {
            size: cache.len(),
            capacity: cache.cap().get(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub size: usize,
    pub capacity: usize,
}

/// Async Pager that coordinates I/O operations
pub struct AsyncPager {
    cache: Arc<AsyncPageCache>,
    vfs: Arc<dyn AsyncVfs>,
}

impl AsyncPager {
    pub async fn new(path: impl AsRef<Path>, cache_size: usize) -> Result<Self> {
        let vfs: Arc<dyn AsyncVfs> = Arc::new(TokioVfs::new(path).await?);
        let cache = Arc::new(AsyncPageCache::new(cache_size, Arc::clone(&vfs)));

        Ok(Self { cache, vfs })
    }

    /// Read a page
    pub async fn read_page(&self, page_id: PageId) -> Result<Arc<Page>> {
        self.cache.read_page(page_id).await
    }

    /// Write a page
    pub async fn write_page(&self, page_id: PageId, page: Page) -> Result<()> {
        self.cache.write_page(page_id, page).await
    }

    /// Allocate a new page
    pub async fn allocate_page(&self) -> Result<PageId> {
        self.cache.allocate_page().await
    }

    /// Flush all pending writes
    pub async fn flush(&self) -> Result<()> {
        self.cache.flush().await?;
        self.vfs.sync().await
    }

    /// Get the number of pages
    pub async fn num_pages(&self) -> u64 {
        self.vfs.num_pages().await
    }

    /// Get cache statistics
    pub fn cache_stats(&self) -> CacheStats {
        self.cache.cache_stats()
    }
}

/// I/O request for batching
#[derive(Debug)]
pub enum IoRequest {
    Read { page_id: PageId },
    Write { page_id: PageId, page: Page },
}

/// Batch I/O executor for improved performance
pub struct BatchIoExecutor {
    pager: Arc<AsyncPager>,
}

impl BatchIoExecutor {
    pub fn new(pager: Arc<AsyncPager>) -> Self {
        Self { pager }
    }

    /// Execute a batch of I/O requests in parallel
    pub async fn execute_batch(&self, requests: Vec<IoRequest>) -> Result<HashMap<PageId, Page>> {
        let mut handles = Vec::new();
        let mut results = HashMap::new();

        for request in requests {
            let pager = Arc::clone(&self.pager);
            
            let handle = tokio::spawn(async move {
                match request {
                    IoRequest::Read { page_id } => {
                        let page = pager.read_page(page_id).await?;
                        Ok::<_, VelociError>((page_id, (*page).clone()))
                    }
                    IoRequest::Write { page_id, page } => {
                        pager.write_page(page_id, page.clone()).await?;
                        Ok((page_id, page))
                    }
                }
            });

            handles.push(handle);
        }

        // Wait for all requests to complete
        for handle in handles {
            match handle.await {
                Ok(Ok((page_id, page))) => {
                    results.insert(page_id, page);
                }
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(VelociError::IoError(e.to_string())),
            }
        }

        Ok(results)
    }

    /// Read multiple pages in parallel
    pub async fn read_batch(&self, page_ids: Vec<PageId>) -> Result<HashMap<PageId, Page>> {
        let requests: Vec<IoRequest> = page_ids
            .into_iter()
            .map(|page_id| IoRequest::Read { page_id })
            .collect();

        self.execute_batch(requests).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn test_async_vfs_basic() {
        let temp_file = NamedTempFile::new().unwrap();
        let vfs = TokioVfs::new(temp_file.path()).await.unwrap();

        // Allocate a page
        let page_id = vfs.allocate_page().await.unwrap();
        assert_eq!(page_id, 0);

        // Write a page
        let mut page = Page::new();
        page.data_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
        vfs.write_page(page_id, &page).await.unwrap();

        // Read the page back
        let read_page = vfs.read_page(page_id).await.unwrap();
        assert_eq!(read_page.data()[0..4], [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn test_async_page_cache() {
        let temp_file = NamedTempFile::new().unwrap();
        let vfs: Arc<dyn AsyncVfs> = Arc::new(TokioVfs::new(temp_file.path()).await.unwrap());
        let cache = AsyncPageCache::new(10, vfs);

        // Allocate and write
        let page_id = cache.allocate_page().await.unwrap();
        let mut page = Page::new();
        page.data_mut()[0..4].copy_from_slice(&[5, 6, 7, 8]);
        cache.write_page(page_id, page).await.unwrap();

        // Read from cache
        let cached_page = cache.read_page(page_id).await.unwrap();
        assert_eq!(cached_page.data()[0..4], [5, 6, 7, 8]);

        // Check cache stats
        let stats = cache.cache_stats();
        assert_eq!(stats.size, 1);
        assert_eq!(stats.capacity, 10);
    }

    #[tokio::test]
    async fn test_batch_io() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(AsyncPager::new(temp_file.path(), 100).await.unwrap());
        let executor = BatchIoExecutor::new(Arc::clone(&pager));

        // Allocate pages
        let page_ids: Vec<PageId> = futures::future::join_all(
            (0..5).map(|_| pager.allocate_page())
        )
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()
        .unwrap();

        // Write pages
        let mut write_requests = Vec::new();
        for (i, &page_id) in page_ids.iter().enumerate() {
            let mut page = Page::new();
            page.data_mut()[0] = i as u8;
            write_requests.push(IoRequest::Write {
                page_id,
                page,
            });
        }

        executor.execute_batch(write_requests).await.unwrap();

        // Read pages in batch
        let results = executor.read_batch(page_ids.clone()).await.unwrap();

        assert_eq!(results.len(), 5);
        for (i, &page_id) in page_ids.iter().enumerate() {
            let page = results.get(&page_id).unwrap();
            assert_eq!(page.data()[0], i as u8);
        }
    }
}


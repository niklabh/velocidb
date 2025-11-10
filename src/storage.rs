// Storage layer - Pager and page management

use crate::btree::BTree;
use crate::executor::Executor;
use crate::parser::Parser;
use crate::transaction::TransactionManager;
use crate::types::{PageId, QueryResult, Result, VelociError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

pub const PAGE_SIZE: usize = 4096;
pub const CACHE_SIZE: usize = 1024; // Number of pages to cache

#[repr(C, align(4096))]
#[derive(Clone)]
pub struct Page {
    data: [u8; PAGE_SIZE],
}

impl Page {
    pub fn new() -> Self {
        Self {
            data: [0; PAGE_SIZE],
        }
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Pager {
    file: File,
    num_pages: u64,
    cache: RwLock<lru::LruCache<PageId, Arc<RwLock<Page>>>>,
}

impl Pager {
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;

        let metadata = file.metadata()?;
        let file_size = metadata.len();
        let num_pages = if file_size == 0 {
            0
        } else {
            (file_size + PAGE_SIZE as u64 - 1) / PAGE_SIZE as u64
        };

        Ok(Self {
            file,
            num_pages,
            cache: RwLock::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(CACHE_SIZE).unwrap(),
            )),
        })
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Arc<RwLock<Page>>> {
        // Check cache first
        {
            let mut cache = self.cache.write();
            if let Some(page) = cache.get(&page_id) {
                return Ok(Arc::clone(page));
            }
        }

        // Read from disk
        if page_id >= self.num_pages {
            return Err(VelociError::NotFound(format!(
                "Page {} out of bounds",
                page_id
            )));
        }

        let mut page = Page::new();
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(&mut page.data)?;

        let page_arc = Arc::new(RwLock::new(page));

        // Add to cache
        let mut cache = self.cache.write();
        cache.put(page_id, Arc::clone(&page_arc));

        Ok(page_arc)
    }

    pub fn write_page(&mut self, page_id: PageId, page: &Page) -> Result<()> {
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        self.file.sync_data()?;

        // Update cache
        let mut cache = self.cache.write();
        cache.put(page_id, Arc::new(RwLock::new(page.clone())));

        if page_id >= self.num_pages {
            self.num_pages = page_id + 1;
        }

        Ok(())
    }

    pub fn allocate_page(&mut self) -> Result<PageId> {
        let page_id = self.num_pages;
        self.num_pages += 1;
        
        // Initialize the page
        let page = Page::new();
        self.write_page(page_id, &page)?;
        
        Ok(page_id)
    }

    pub fn num_pages(&self) -> u64 {
        self.num_pages
    }

    pub fn flush(&mut self) -> Result<()> {
        self.file.sync_all()?;
        Ok(())
    }
}

pub struct Database {
    pager: Arc<RwLock<Pager>>,
    btrees: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
    transaction_manager: Arc<TransactionManager>,
    schema: Arc<RwLock<Schema>>,
}

impl Database {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Arc<Self>> {
        let pager = Arc::new(RwLock::new(Pager::new(path.as_ref())?));
        let btrees = Arc::new(RwLock::new(HashMap::new()));
        let transaction_manager = Arc::new(TransactionManager::new());
        let schema = Arc::new(RwLock::new(Schema::new()));

        let db = Arc::new(Self {
            pager,
            btrees,
            transaction_manager,
            schema,
        });

        // Initialize if new database
        db.initialize()?;

        Ok(db)
    }

    fn initialize(&self) -> Result<()> {
        let mut pager = self.pager.write();
        
        // If empty database, create root page
        if pager.num_pages() == 0 {
            pager.allocate_page()?;
        }
        
        Ok(())
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        let parser = Parser::new();
        let statement = parser.parse(sql)?;
        
        let executor = Executor::new(
            Arc::clone(&self.pager),
            Arc::clone(&self.btrees),
            Arc::clone(&self.schema),
            Arc::clone(&self.transaction_manager),
        );
        
        executor.execute(statement)?;
        Ok(())
    }

    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let parser = Parser::new();
        let statement = parser.parse(sql)?;
        
        let executor = Executor::new(
            Arc::clone(&self.pager),
            Arc::clone(&self.btrees),
            Arc::clone(&self.schema),
            Arc::clone(&self.transaction_manager),
        );
        
        executor.query(statement)
    }

    pub fn close(&self) -> Result<()> {
        self.pager.write().flush()?;
        Ok(())
    }
}

// Schema management
#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<crate::types::Column>,
    pub root_page: PageId,
}

pub struct Schema {
    tables: HashMap<String, TableSchema>,
}

impl Schema {
    pub fn new() -> Self {
        Self {
            tables: HashMap::new(),
        }
    }

    pub fn create_table(&mut self, table: TableSchema) -> Result<()> {
        if self.tables.contains_key(&table.name) {
            return Err(VelociError::ConstraintViolation(format!(
                "Table '{}' already exists",
                table.name
            )));
        }
        self.tables.insert(table.name.clone(), table);
        Ok(())
    }

    pub fn get_table(&self, name: &str) -> Result<&TableSchema> {
        self.tables
            .get(name)
            .ok_or_else(|| VelociError::NotFound(format!("Table '{}' not found", name)))
    }

    pub fn drop_table(&mut self, name: &str) -> Result<()> {
        self.tables
            .remove(name)
            .ok_or_else(|| VelociError::NotFound(format!("Table '{}' not found", name)))?;
        Ok(())
    }

    pub fn list_tables(&self) -> Vec<String> {
        self.tables.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_pager_create() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Pager::new(temp_file.path()).unwrap();
        assert_eq!(pager.num_pages(), 0);
    }

    #[test]
    fn test_page_allocation() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut pager = Pager::new(temp_file.path()).unwrap();
        
        let page_id = pager.allocate_page().unwrap();
        assert_eq!(page_id, 0);
        assert_eq!(pager.num_pages(), 1);
    }

    #[test]
    fn test_read_write_page() {
        let temp_file = NamedTempFile::new().unwrap();
        let mut pager = Pager::new(temp_file.path()).unwrap();
        
        let page_id = pager.allocate_page().unwrap();
        
        let mut page = Page::new();
        page.data_mut()[0..4].copy_from_slice(&[1, 2, 3, 4]);
        
        pager.write_page(page_id, &page).unwrap();
        
        let read_page = pager.read_page(page_id).unwrap();
        let read_page_locked = read_page.read();
        assert_eq!(read_page_locked.data()[0..4], [1, 2, 3, 4]);
    }

    #[test]
    fn test_database_create() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open(temp_file.path()).unwrap();
        assert!(db.pager.read().num_pages() >= 1);
    }
}


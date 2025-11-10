// Storage layer - Pager and page management

use crate::btree::BTree;
use crate::executor::Executor;
use crate::parser::{Parser, Statement};
use crate::transaction::TransactionManager;
use crate::types::{DataType, PageId, QueryResult, Result, VelociError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

pub const PAGE_SIZE: usize = 4096;
pub const CACHE_SIZE: usize = 1024; // Number of pages to cache

#[repr(C, align(4096))]
#[derive(Clone, Debug)]
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

impl Drop for Pager {
    fn drop(&mut self) {
        // Ensure file is flushed before closing
        let _ = self.file.sync_all();
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
        let num_pages = {
            let mut pager = self.pager.write();

            // If empty database, create root page and schema page
            if pager.num_pages() == 0 {
                pager.allocate_page()?; // Page 0 - root
                pager.allocate_page()?; // Page 1 - schema
            }
            
            pager.num_pages()
        }; // Drop the write lock here
        
        // Load existing schema if database already exists
        if num_pages > 0 {
            self.load_schema()?;
        }

        Ok(())
    }

    fn load_schema(&self) -> Result<()> {
        let pager_read = self.pager.read();
        if pager_read.num_pages() < 2 {
            return Ok(()); // No schema page yet
        }
        drop(pager_read);

        // Read schema data without holding the lock for too long
        let data_copy: Vec<u8> = {
            let mut pager = self.pager.write();
            let page_arc = pager.read_page(1)?;
            let page = page_arc.read();
            let data = page.data();

            // Simple schema format: number of tables, then each table
            if data.len() < 4 {
                return Ok(()); // Empty schema
            }

            // Copy data to avoid holding locks while parsing
            data.to_vec()
        };

        // Parse schema without holding pager lock
        let data = &data_copy[..];
        if data.len() < 4 {
            return Ok(());
        }

        let num_tables = u32::from_le_bytes(data[0..4].try_into().unwrap_or([0, 0, 0, 0]));
        let mut offset = 4;

        for _ in 0..num_tables {
            if offset + 8 > data.len() {
                break; // Corrupted data
            }

            // Table name length and name
            let name_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0, 0, 0, 0])) as usize;
            offset += 4;

            if offset + name_len > data.len() {
                break;
            }

            let table_name = String::from_utf8(data[offset..offset + name_len].to_vec())
                .unwrap_or_default();
            offset += name_len;

            // Number of columns
            if offset + 4 > data.len() {
                break;
            }

            let num_cols = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0, 0, 0, 0])) as usize;
            offset += 4;

            let mut columns = Vec::new();
            for _ in 0..num_cols {
                if offset + 13 > data.len() {
                    break;
                }

                // Column data: name_len(4) + name + data_type(1) + flags(1) + root_page(8)
                let col_name_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap_or([0, 0, 0, 0])) as usize;
                offset += 4;

                if offset + col_name_len + 10 > data.len() {
                    break;
                }

                let col_name = String::from_utf8(data[offset..offset + col_name_len].to_vec())
                    .unwrap_or_default();
                offset += col_name_len;

                let data_type_byte = data[offset];
                let data_type = match data_type_byte {
                    0 => DataType::Integer,
                    1 => DataType::Real,
                    2 => DataType::Text,
                    3 => DataType::Blob,
                    _ => DataType::Text,
                };
                offset += 1;

                let flags = data[offset];
                let primary_key = (flags & 1) != 0;
                let not_null = (flags & 2) != 0;
                let unique = (flags & 4) != 0;
                offset += 1;

                let root_page = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap_or([0, 0, 0, 0, 0, 0, 0, 0]));
                offset += 8;

                columns.push(crate::types::Column {
                    name: col_name,
                    data_type,
                    primary_key,
                    not_null,
                    unique,
                });

                // Create B-Tree for this table
                let btree_root = if root_page == 0 {
                    // Root page is invalid, create a new properly initialized B-Tree
                    let mut pager = self.pager.write();
                    let new_root_page = pager.allocate_page()?;

                    // Initialize as B-Tree leaf node
                    let mut page = crate::storage::Page::new();
                    let header = crate::btree::NodeHeader::new_leaf();
                    header.serialize(page.data_mut());
                    pager.write_page(new_root_page, &page)?;

                    new_root_page
                } else {
                    root_page
                };

                let btree = crate::btree::BTree::from_root(btree_root, Arc::clone(&self.pager));
                self.btrees.write().insert(table_name.clone(), Arc::new(RwLock::new(btree)));
            }

            // Add table to schema
            let table_schema = TableSchema {
                name: table_name,
                columns,
                root_page: 0, // Will be set by individual columns
            };
            self.schema.write().create_table(table_schema)?;
        }

        Ok(())
    }

    fn save_schema(&self) -> Result<()> {
        let schema = self.schema.read();
        let mut buffer = Vec::new();

        // Number of tables
        let tables = schema.list_tables();
        buffer.extend_from_slice(&(tables.len() as u32).to_le_bytes());

        for table_name in tables {
            if let Ok(table_schema) = schema.get_table(&table_name) {
                // Table name
                let name_bytes = table_name.as_bytes();
                buffer.extend_from_slice(&(name_bytes.len() as u32).to_le_bytes());
                buffer.extend_from_slice(name_bytes);

                // Number of columns
                buffer.extend_from_slice(&(table_schema.columns.len() as u32).to_le_bytes());

                for column in &table_schema.columns {
                    // Column name
                    let col_name_bytes = column.name.as_bytes();
                    buffer.extend_from_slice(&(col_name_bytes.len() as u32).to_le_bytes());
                    buffer.extend_from_slice(col_name_bytes);

                    // Data type
                    let data_type_byte = match column.data_type {
                        DataType::Integer => 0u8,
                        DataType::Real => 1u8,
                        DataType::Text => 2u8,
                        DataType::Blob => 3u8,
                        DataType::Null => 4u8,
                    };
                    buffer.push(data_type_byte);

                    // Flags
                    let mut flags = 0u8;
                    if column.primary_key {
                        flags |= 1;
                    }
                    if column.not_null {
                        flags |= 2;
                    }
                    if column.unique {
                        flags |= 4;
                    }
                    buffer.push(flags);

                    // Root page - get from B-Tree
                    let root_page = if let Some(btree_arc) = self.btrees.read().get(&table_name) {
                        let rp = btree_arc.read().root_page();
                        if rp == 0 {
                            // This shouldn't happen for properly initialized B-Trees
                            eprintln!("Warning: Table '{}' has invalid root page 0", table_name);
                            0u64
                        } else {
                            rp
                        }
                    } else {
                        // This shouldn't happen - B-Tree should exist for created tables
                        eprintln!("Warning: No B-Tree found for table '{}'", table_name);
                        0u64
                    };
                    buffer.extend_from_slice(&root_page.to_le_bytes());
                }
            }
        }

        // Write to schema page (page 1)
        let mut pager = self.pager.write();
        if pager.num_pages() < 2 {
            pager.allocate_page()?; // Ensure schema page exists
        }

        let mut page = crate::storage::Page::new();
        let copy_len = std::cmp::min(buffer.len(), crate::storage::PAGE_SIZE);
        if buffer.len() > crate::storage::PAGE_SIZE {
            return Err(VelociError::StorageError(format!(
                "Schema size ({} bytes) exceeds page size ({} bytes). Consider multi-page schema storage.",
                buffer.len(),
                crate::storage::PAGE_SIZE
            )));
        }
        page.data_mut()[0..copy_len].copy_from_slice(&buffer[0..copy_len]);
        pager.write_page(1, &page)?;

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

        executor.execute(statement.clone())?;

        // Save schema if this was a DDL statement
        match statement {
            Statement::CreateTable { .. } |
            Statement::DropTable { .. } => {
                self.save_schema()?;
            }
            _ => {}
        }

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

    pub fn list_tables(&self) -> Vec<String> {
        self.schema.read().list_tables()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Ensure data is flushed to disk when database is dropped
        let _ = self.pager.write().flush();
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


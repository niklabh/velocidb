// Storage layer - Pager and page management

use crate::btree::BTree;
use crate::executor::Executor;
use crate::mvcc::MvccManager;
use crate::parser::{Parser, Statement};
use crate::transaction::TransactionManager;
use crate::types::{DataType, PageId, QueryResult, Result, VelociError};
use dashmap::DashMap;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
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
    cache: Arc<DashMap<PageId, Arc<RwLock<Page>>>>,
    cache_size: AtomicUsize,
    max_cache_size: usize,
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
            cache: Arc::new(DashMap::new()),
            cache_size: AtomicUsize::new(0),
            max_cache_size: CACHE_SIZE,
        })
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Arc<RwLock<Page>>> {
        // Check cache first (lock-free read via DashMap)
        if let Some(page) = self.cache.get(&page_id) {
            return Ok(page.clone());
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

        // Evict if cache is full
        while self.cache_size.load(Ordering::Relaxed) >= self.max_cache_size {
            // Evict the first entry we can remove
            let key_to_remove = self.cache.iter().next().map(|e| *e.key());
            if let Some(key) = key_to_remove {
                self.cache.remove(&key);
                self.cache_size.fetch_sub(1, Ordering::Relaxed);
            } else {
                break;
            }
        }

        // Add to cache
        self.cache.insert(page_id, page_arc.clone());
        self.cache_size.fetch_add(1, Ordering::Relaxed);

        Ok(page_arc)
    }

    pub fn write_page(&mut self, page_id: PageId, page: &Page) -> Result<()> {
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(&page.data)?;
        self.file.sync_data()?;

        // Update cache
        self.cache.insert(page_id, Arc::new(RwLock::new(page.clone())));

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

/// The main database structure.
///
/// `Database` provides the primary interface for interacting with a VelociDB database.
/// It handles storage management, transaction coordination, and query execution.
///
/// # Thread Safety
///
/// `Database` is thread-safe and can be shared across threads using `Arc`.
/// Internally, it uses fine-grained locking to allow concurrent reads and writes.
pub struct Database {
    pager: Arc<RwLock<Pager>>,
    btrees: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
    transaction_manager: Arc<TransactionManager>,
    schema: Arc<RwLock<Schema>>,
    mvcc: Arc<MvccManager>,
    executor: RwLock<Option<Arc<Executor>>>,
}

impl Database {
    /// Opens a database at the specified path.
    ///
    /// If the database file does not exist, it will be created.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to the database file.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing an `Arc<Database>` on success.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use velocidb::Database;
    ///
    /// let db = Database::open("my_database.db").unwrap();
    /// ```
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Arc<Self>> {
        let pager = Arc::new(RwLock::new(Pager::new(path.as_ref())?));
        let btrees = Arc::new(RwLock::new(HashMap::new()));
        let transaction_manager = Arc::new(TransactionManager::new());
        let schema = Arc::new(RwLock::new(Schema::new()));
        let mvcc = Arc::new(MvccManager::new());

        let db = Arc::new(Self {
            pager,
            btrees,
            transaction_manager,
            schema,
            mvcc,
            executor: RwLock::new(None),
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
        // Read all schema pages (starting from page 1) until we hit the last one
        let mut data_copy = Vec::new();
        let mut schema_page = 1u64;

        loop {
            let pager_read = self.pager.read();
            if pager_read.num_pages() <= schema_page {
                break;
            }
            drop(pager_read);

            let page_data: Vec<u8> = {
                let mut pager = self.pager.write();
                let page_arc = pager.read_page(schema_page)?;
                let page = page_arc.read();
                page.data().to_vec()
            };

            if page_data.len() < 4 {
                break;
            }

            let chunk_len = u32::from_le_bytes(
                page_data[0..4].try_into()
                    .map_err(|_| VelociError::Corruption("Failed to read chunk length".to_string()))?
            ) as usize;

            let chunk_end = std::cmp::min(4 + chunk_len, page_data.len());
            data_copy.extend_from_slice(&page_data[4..chunk_end]);

            if chunk_len < PAGE_SIZE - 4 {
                break; // Last page (partial chunk)
            }
            schema_page += 1;
        }

        if data_copy.len() < 4 {
            return Ok(()); // Empty schema
        }

        let data = &data_copy[..];

        let num_tables = u32::from_le_bytes(
            data[0..4].try_into()
                .map_err(|_| VelociError::Corruption("Failed to read table count".to_string()))?
        );
        let mut offset: usize = 4;

        for _ in 0..num_tables {
            if offset + 4 > data.len() {
                return Err(VelociError::Corruption("Schema truncated at table name length".to_string()));
            }

            let name_len = u32::from_le_bytes(
                data[offset..offset + 4].try_into()
                    .map_err(|_| VelociError::Corruption("Failed to read table name length".to_string()))?
            ) as usize;
            offset += 4;

            if offset + name_len > data.len() {
                return Err(VelociError::Corruption(format!(
                    "Schema truncated at table name (expected {} bytes)", name_len
                )));
            }

            let table_name = String::from_utf8(data[offset..offset + name_len].to_vec())
                .map_err(|_| VelociError::Corruption(format!("Invalid UTF-8 in table name at offset {}", offset)))?;
            offset += name_len;

            if offset + 4 > data.len() {
                return Err(VelociError::Corruption(format!(
                    "Schema truncated at column count for table '{}'", table_name
                )));
            }

            let num_cols = u32::from_le_bytes(
                data[offset..offset + 4].try_into()
                    .map_err(|_| VelociError::Corruption("Failed to read column count".to_string()))?
            ) as usize;
            offset += 4;

            let mut columns = Vec::new();
            let mut table_root_page: u64 = 0;
            for _ in 0..num_cols {
                if offset + 4 > data.len() {
                    return Err(VelociError::Corruption(format!(
                        "Schema truncated at column name length in table '{}'", table_name
                    )));
                }

                let col_name_len = u32::from_le_bytes(
                    data[offset..offset + 4].try_into()
                        .map_err(|_| VelociError::Corruption("Failed to read column name length".to_string()))?
                ) as usize;
                offset += 4;

                if offset + col_name_len + 10 > data.len() {
                    return Err(VelociError::Corruption(format!(
                        "Schema truncated at column data for table '{}'", table_name
                    )));
                }

                let col_name = String::from_utf8(data[offset..offset + col_name_len].to_vec())
                    .map_err(|_| VelociError::Corruption(format!("Invalid UTF-8 in column name at offset {}", offset)))?;
                offset += col_name_len;

                let data_type_byte = data[offset];
                let data_type = match data_type_byte {
                    0 => DataType::Integer,
                    1 => DataType::Real,
                    2 => DataType::Text,
                    3 => DataType::Blob,
                    _ => return Err(VelociError::Corruption(format!(
                        "Unknown data type byte {} for column '{}' in table '{}'",
                        data_type_byte, col_name, table_name
                    ))),
                };
                offset += 1;

                let flags = data[offset];
                let primary_key = (flags & 1) != 0;
                let not_null = (flags & 2) != 0;
                let unique = (flags & 4) != 0;
                offset += 1;

                let root_page = u64::from_le_bytes(
                    data[offset..offset + 8].try_into()
                        .map_err(|_| VelociError::Corruption("Failed to read root page".to_string()))?
                );
                offset += 8;

                if table_root_page == 0 && root_page != 0 {
                    table_root_page = root_page;
                }

                columns.push(crate::types::Column {
                    name: col_name,
                    data_type,
                    primary_key,
                    not_null,
                    unique,
                });
            }

            let btree_root = if table_root_page == 0 {
                let mut pager = self.pager.write();
                let new_root_page = pager.allocate_page()?;

                let mut page = crate::storage::Page::new();
                let header = crate::btree::NodeHeader::new_leaf();
                header.serialize(page.data_mut());
                pager.write_page(new_root_page, &page)?;

                new_root_page
            } else {
                table_root_page
            };

            let btree = crate::btree::BTree::from_root(btree_root, Arc::clone(&self.pager));
            self.btrees.write().insert(table_name.clone(), Arc::new(RwLock::new(btree)));

            let table_schema = TableSchema {
                name: table_name,
                columns,
                root_page: btree_root,
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
                            eprintln!("Warning: Table '{}' has invalid root page 0", table_name);
                            0u64
                        } else {
                            rp
                        }
                    } else {
                        eprintln!("Warning: No B-Tree found for table '{}'", table_name);
                        0u64
                    };
                    buffer.extend_from_slice(&root_page.to_le_bytes());
                }
            }
        }

        // Write schema across multiple pages if needed
        let mut pager = self.pager.write();
        let usable_size = PAGE_SIZE - 4; // Reserve 4 bytes for chunk length header
        let num_pages_needed = if buffer.is_empty() { 1 } else { (buffer.len() + usable_size - 1) / usable_size };

        // Ensure we have enough schema pages (starting from page 1)
        while pager.num_pages() < 1 + num_pages_needed as u64 {
            pager.allocate_page()?;
        }

        for page_idx in 0..num_pages_needed {
            let start = page_idx * usable_size;
            let end = std::cmp::min(start + usable_size, buffer.len());
            let chunk = &buffer[start..end];

            let mut page = crate::storage::Page::new();
            // First 4 bytes: chunk length for this page (u32)
            let chunk_len = chunk.len() as u32;
            page.data_mut()[0..4].copy_from_slice(&chunk_len.to_le_bytes());
            page.data_mut()[4..4 + chunk.len()].copy_from_slice(chunk);

            let schema_page_id = 1 + page_idx as u64;
            pager.write_page(schema_page_id, &page)?;
        }

        Ok(())
    }

    fn get_or_create_executor(&self) -> Arc<Executor> {
        {
            let exec_guard = self.executor.read();
            if let Some(ref exec) = *exec_guard {
                return Arc::clone(exec);
            }
        }
        let exec = Arc::new(Executor::new(
            Arc::clone(&self.pager),
            Arc::clone(&self.btrees),
            Arc::clone(&self.schema),
            Arc::clone(&self.transaction_manager),
            Arc::clone(&self.mvcc),
        ));
        *self.executor.write() = Some(Arc::clone(&exec));
        exec
    }

    /// Executes a SQL statement that does not return rows (e.g., CREATE, INSERT, UPDATE, DELETE).
    ///
    /// # Arguments
    ///
    /// * `sql` - The SQL statement to execute.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use velocidb::Database;
    /// # let db = Database::open("test.db").unwrap();
    /// db.execute("CREATE TABLE items (id INTEGER, name TEXT)").unwrap();
    /// db.execute("INSERT INTO items VALUES (1, 'Item 1')").unwrap();
    /// ```
    pub fn execute(&self, sql: &str) -> Result<()> {
        let parser = Parser::new();
        let statement = parser.parse(sql)?;

        let executor = self.get_or_create_executor();

        executor.execute_statement(statement.clone())?;

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

    /// Executes a SQL query that returns rows (e.g., SELECT).
    ///
    /// # Arguments
    ///
    /// * `sql` - The SQL query to execute.
    ///
    /// # Returns
    ///
    /// Returns a `Result` containing a `QueryResult` with the fetched rows and columns.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use velocidb::Database;
    /// # let db = Database::open("test.db").unwrap();
    /// let results = db.query("SELECT * FROM items").unwrap();
    /// for row in results.rows {
    ///     println!("{:?}", row.values);
    /// }
    /// ```
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let parser = Parser::new();
        let statement = parser.parse(sql)?;
        
        let executor = self.get_or_create_executor();
        
        executor.query_statement(statement)
    }

    /// Begins an explicit transaction.
    pub fn begin(&self) -> Result<()> {
        let executor = self.get_or_create_executor();
        executor.begin_transaction()
    }

    /// Commits the current explicit transaction.
    pub fn commit(&self) -> Result<()> {
        let executor = self.get_or_create_executor();
        executor.commit_transaction()
    }

    /// Rolls back the current explicit transaction.
    pub fn rollback(&self) -> Result<()> {
        let executor = self.get_or_create_executor();
        executor.rollback_transaction()
    }

    /// Closes the database and flushes all changes to disk.
    ///
    /// While `Database` implements `Drop` to automatically flush on cleanup,
    /// calling `close` explicitly allows handling any flush errors.
    pub fn close(&self) -> Result<()> {
        self.pager.write().flush()?;
        Ok(())
    }

    /// Lists all tables in the database.
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


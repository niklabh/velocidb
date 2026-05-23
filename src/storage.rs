//! Storage engine layer: file-backed pager, database lifecycle, and schema management.
//!
//! This module implements a page-based storage engine with 4 KB pages,
//! providing the foundation for B-Tree indexes and MVCC record storage.

use crate::btree::BTree;
use crate::executor::Executor;
use crate::parser::{Parser, Statement};
use crate::transaction::TransactionManager;
use crate::types::{DataType, PageId, QueryResult, Result, VelociError};
use crate::wal::WalManager;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
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

/// The pager owns the on-disk data file and the WAL.
///
/// All page mutations flow through `write_page`. If a "write group" is active
/// (started by `begin_group`), the page is appended to the WAL and buffered in
/// `pending` until `commit_group` fsyncs the WAL and applies the buffered
/// pages to the data file. With no active group, `write_page` opens an
/// implicit single-write group, giving every standalone write its own atomic
/// WAL transaction.
///
/// Reads consult `pending` first (so writes within the active group are
/// visible to subsequent reads), then the page cache, then the data file.
pub struct Pager {
    file: File,
    num_pages: u64,
    cache: Arc<DashMap<PageId, Arc<RwLock<Page>>>>,
    cache_size: AtomicUsize,
    max_cache_size: usize,
    wal: WalManager,
    active_group: Option<u64>,
    pending: HashMap<PageId, Page>,
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

        let wal = WalManager::open(path)?;

        let mut pager = Self {
            file,
            num_pages,
            cache: Arc::new(DashMap::new()),
            cache_size: AtomicUsize::new(0),
            max_cache_size: CACHE_SIZE,
            wal,
            active_group: None,
            pending: HashMap::new(),
        };

        pager.recover()?;
        Ok(pager)
    }

    /// Replays any committed WAL groups onto the data file, then truncates the
    /// WAL. Safe to call multiple times (truncated WAL yields zero groups).
    fn recover(&mut self) -> Result<()> {
        let groups = self.wal.read_committed_groups()?;
        if groups.is_empty() {
            return Ok(());
        }
        for group in &groups {
            for (page_id, data) in &group.writes {
                self.write_page_raw(*page_id, data)?;
            }
        }
        self.file.sync_data()?;
        self.wal.truncate()?;
        Ok(())
    }

    /// Writes a page directly to the data file (no WAL, no cache, no pending).
    /// Only used by recovery and `commit_group`.
    fn write_page_raw(&mut self, page_id: PageId, data: &[u8]) -> Result<()> {
        if data.len() != PAGE_SIZE {
            return Err(VelociError::StorageError(format!(
                "write_page_raw: expected {} bytes, got {}",
                PAGE_SIZE,
                data.len()
            )));
        }
        let offset = page_id * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        if page_id >= self.num_pages {
            self.num_pages = page_id + 1;
        }
        Ok(())
    }

    /// Begins a write group. Subsequent `write_page` calls are buffered and
    /// only made durable on `commit_group`. Fails if a group is already
    /// active (callers should serialize via a writer mutex).
    pub fn begin_group(&mut self) -> Result<u64> {
        if self.active_group.is_some() {
            return Err(VelociError::TransactionError(
                "Write group already active".to_string(),
            ));
        }
        let id = self.wal.allocate_group_id();
        self.active_group = Some(id);
        Ok(id)
    }

    /// Commits the active write group: fsyncs the WAL (durability of the
    /// COMMIT marker), applies buffered pages to the data file, fsyncs the
    /// data file, then truncates the WAL.
    pub fn commit_group(&mut self) -> Result<()> {
        let group_id = self.active_group.take().ok_or_else(|| {
            VelociError::TransactionError("No active write group to commit".to_string())
        })?;

        // Empty group: nothing to commit, no WAL records were written.
        if self.pending.is_empty() {
            return Ok(());
        }

        self.wal.log_commit(group_id)?;

        let pending = std::mem::take(&mut self.pending);
        for (page_id, page) in &pending {
            self.write_page_raw(*page_id, page.data())?;
        }
        self.file.sync_data()?;
        self.wal.truncate()?;
        Ok(())
    }

    /// Aborts the active write group: discards buffered pages and evicts them
    /// from the cache so subsequent reads observe the pre-group state.
    /// The WAL is left untouched; any orphan records are skipped by recovery
    /// (no COMMIT marker) and removed by the next successful `truncate`.
    pub fn abort_group(&mut self) -> Result<()> {
        if self.active_group.take().is_none() {
            return Err(VelociError::TransactionError(
                "No active write group to abort".to_string(),
            ));
        }
        for page_id in self.pending.keys() {
            if self.cache.remove(page_id).is_some() {
                self.cache_size.fetch_sub(1, Ordering::Relaxed);
            }
        }
        self.pending.clear();
        Ok(())
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Arc<RwLock<Page>>> {
        // Pending writes from the active group take precedence over disk/cache
        // so writers see their own modifications.
        if let Some(page) = self.pending.get(&page_id) {
            return Ok(Arc::new(RwLock::new(page.clone())));
        }

        if let Some(page) = self.cache.get(&page_id) {
            return Ok(page.clone());
        }

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

        while self.cache.len() >= self.max_cache_size {
            let key_to_remove = self.cache.iter().next().map(|e| *e.key());
            if let Some(key) = key_to_remove {
                if self.cache.remove(&key).is_some() {
                    self.cache_size.fetch_sub(1, Ordering::Relaxed);
                }
            } else {
                break;
            }
        }

        if self.cache.insert(page_id, page_arc.clone()).is_none() {
            self.cache_size.fetch_add(1, Ordering::Relaxed);
        }

        Ok(page_arc)
    }

    pub fn write_page(&mut self, page_id: PageId, page: &Page) -> Result<()> {
        // Auto-group if the caller didn't open one explicitly.
        let auto_group = self.active_group.is_none();
        if auto_group {
            self.begin_group()?;
        }
        let group_id = self.active_group.expect("group active after begin_group");

        self.wal.log_page_write(group_id, page_id, page.data())?;

        // Update pending buffer.
        self.pending.insert(page_id, page.clone());

        // Update cache (overwrites any older version).
        while self.cache.len() >= self.max_cache_size {
            let key_to_remove = self.cache.iter().next().map(|e| *e.key());
            if let Some(key) = key_to_remove {
                if key == page_id {
                    break;
                }
                if self.cache.remove(&key).is_some() {
                    self.cache_size.fetch_sub(1, Ordering::Relaxed);
                }
            } else {
                break;
            }
        }
        if self
            .cache
            .insert(page_id, Arc::new(RwLock::new(page.clone())))
            .is_none()
        {
            self.cache_size.fetch_add(1, Ordering::Relaxed);
        }

        if page_id >= self.num_pages {
            self.num_pages = page_id + 1;
        }

        if auto_group {
            self.commit_group()?;
        }

        Ok(())
    }

    pub fn allocate_page(&mut self) -> Result<PageId> {
        let page_id = self.num_pages;
        let page = Page::new();
        // `write_page` will bump num_pages.
        self.write_page(page_id, &page)?;
        Ok(page_id)
    }

    pub fn num_pages(&self) -> u64 {
        self.num_pages
    }

    pub fn flush(&mut self) -> Result<()> {
        // If a group is active when flush is called (typically because the
        // database is being closed mid-operation), abort it so we don't leave
        // partially-applied state behind.
        if self.active_group.is_some() {
            let _ = self.abort_group();
        }
        self.file.sync_all()?;
        Ok(())
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        let _ = self.flush();
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
    executor: RwLock<Option<Arc<Executor>>>,
    /// Serializes write groups across statements so only one writer creates an
    /// active WAL group at a time. Read paths (`query`) do not acquire this
    /// mutex.
    writer: Mutex<()>,
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

        let db = Arc::new(Self {
            pager,
            btrees,
            transaction_manager,
            schema,
            executor: RwLock::new(None),
            writer: Mutex::new(()),
        });

        // Initialize if new database
        db.initialize()?;

        Ok(db)
    }

    fn initialize(&self) -> Result<()> {
        // The two initial pages (root + schema) are durably allocated through a
        // WAL group so the database is consistent even if the process crashes
        // right after open().
        let num_pages = {
            let mut pager = self.pager.write();
            if pager.num_pages() == 0 {
                pager.begin_group()?;
                let result = (|| -> Result<()> {
                    pager.allocate_page()?; // Page 0 - root
                    pager.allocate_page()?; // Page 1 - schema
                    Ok(())
                })();
                match result {
                    Ok(()) => pager.commit_group()?,
                    Err(e) => {
                        let _ = pager.abort_group();
                        return Err(e);
                    }
                }
            }
            pager.num_pages()
        };

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
                // A schema entry without a valid root page indicates a bug or
                // partial recovery. Allocate a fresh leaf root for it.
                let mut pager = self.pager.write();
                pager.begin_group()?;
                let res = (|| -> Result<u64> {
                    let new_root_page = pager.allocate_page()?;
                    let mut page = crate::storage::Page::new();
                    let header = crate::btree::NodeHeader::new_leaf();
                    header.serialize(page.data_mut());
                    pager.write_page(new_root_page, &page)?;
                    Ok(new_root_page)
                })();
                match res {
                    Ok(p) => {
                        pager.commit_group()?;
                        p
                    }
                    Err(e) => {
                        let _ = pager.abort_group();
                        return Err(e);
                    }
                }
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

        // The entire schema serialization is one atomic WAL group: either all
        // chained pages land or none do.
        let mut pager = self.pager.write();
        let usable_size = PAGE_SIZE - 4; // Reserve 4 bytes for chunk length header
        let num_pages_needed = if buffer.is_empty() { 1 } else { (buffer.len() + usable_size - 1) / usable_size };

        pager.begin_group()?;
        let res = (|| -> Result<()> {
            while pager.num_pages() < 1 + num_pages_needed as u64 {
                pager.allocate_page()?;
            }

            for page_idx in 0..num_pages_needed {
                let start = page_idx * usable_size;
                let end = std::cmp::min(start + usable_size, buffer.len());
                let chunk = &buffer[start..end];

                let mut page = crate::storage::Page::new();
                let chunk_len = chunk.len() as u32;
                page.data_mut()[0..4].copy_from_slice(&chunk_len.to_le_bytes());
                page.data_mut()[4..4 + chunk.len()].copy_from_slice(chunk);

                let schema_page_id = 1 + page_idx as u64;
                pager.write_page(schema_page_id, &page)?;
            }
            Ok(())
        })();
        match res {
            Ok(()) => pager.commit_group()?,
            Err(e) => {
                let _ = pager.abort_group();
                return Err(e);
            }
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

        // Serialize writers so the pager only ever has a single active WAL
        // group. We do NOT hold `pager.write()` across the executor call —
        // the executor takes the pager lock many times internally and a
        // recursive write lock would deadlock. The active_group field on
        // Pager persists across lock releases, so `write_page` calls from the
        // executor still see the group and buffer correctly.
        let _writer = self.writer.lock();

        let needs_schema_save = matches!(
            statement,
            Statement::CreateTable { .. } | Statement::DropTable { .. }
        );

        // Capture root pages before the statement so we can detect any
        // B-tree root changes (caused by splits or root-collapse during
        // underflow). If a root changed, the schema page must be re-saved
        // so a subsequent open finds the correct root.
        let roots_before = self.snapshot_roots();

        self.pager.write().begin_group()?;
        let result = executor.execute_statement(statement);
        match &result {
            Ok(()) => self.pager.write().commit_group()?,
            Err(_) => {
                let _ = self.pager.write().abort_group();
            }
        }
        result?;

        let roots_after = self.snapshot_roots();
        if needs_schema_save || roots_before != roots_after {
            self.save_schema()?;
        }

        Ok(())
    }

    fn snapshot_roots(&self) -> HashMap<String, PageId> {
        let btrees = self.btrees.read();
        let mut out = HashMap::with_capacity(btrees.len());
        for (name, bt) in btrees.iter() {
            out.insert(name.clone(), bt.read().root_page());
        }
        out
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

    /// Returns a human-readable `CREATE TABLE` statement for the named table,
    /// suitable for display in `.schema`-style REPL commands.
    pub fn describe_table(&self, name: &str) -> Result<String> {
        let schema = self.schema.read();
        let table = schema.get_table(name)?;
        let mut out = format!("CREATE TABLE {} (", table.name);
        for (i, col) in table.columns.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let type_str = match col.data_type {
                DataType::Integer => "INTEGER",
                DataType::Real => "REAL",
                DataType::Text => "TEXT",
                DataType::Blob => "BLOB",
                DataType::Null => "NULL",
            };
            out.push_str(&format!("{} {}", col.name, type_str));
            if col.primary_key {
                out.push_str(" PRIMARY KEY");
            }
            if col.not_null && !col.primary_key {
                out.push_str(" NOT NULL");
            }
            if col.unique && !col.primary_key {
                out.push_str(" UNIQUE");
            }
        }
        out.push(')');
        Ok(out)
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


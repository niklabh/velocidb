//! Storage engine layer: file-backed pager, database lifecycle, and schema management.
//!
//! This module implements a page-based storage engine with 4 KB pages,
//! providing the foundation for B-Tree indexes and MVCC record storage.

use crate::btree::BTree;
use crate::cdc::{CdcManager, ChangeEvent};
use crate::executor::Executor;
use crate::parser::{Parser, Statement};
use crate::transaction::TransactionManager;
use crate::types::{DataType, PageId, QueryResult, Result, Row, VelociError};
use crate::vector::{self, DistanceMetric};
use crate::wal::WalManager;
use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub const PAGE_SIZE: usize = 4096;
pub const CACHE_SIZE: usize = 1024; // Number of pages to cache
/// WAL size at which a commit triggers a checkpoint (~1,000 page records).
pub const CHECKPOINT_WAL_BYTES: u64 = 4 * 1024 * 1024;

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
/// (started by `begin_group`), the page is buffered in `pending` until
/// `commit_group` appends every buffered page to the WAL and fsyncs it. The
/// committed pages then live in `committed` (and the WAL) until `checkpoint`
/// copies them into the data file. With no active group, `write_page`
/// opens an implicit single-write group, giving every standalone write its own
/// atomic WAL transaction.
///
/// Because nothing reaches the WAL before commit, a group can span several
/// statements (an explicit `BEGIN` … `COMMIT`) and be discarded wholesale by
/// `abort_group`. Within a group, a savepoint (`begin_savepoint`) records the
/// pre-image of every page a statement touches so a failed statement can be
/// undone with `rollback_to_savepoint` without aborting the whole group.
///
/// Reads consult `pending` first (so writes within the active group are
/// visible to subsequent reads), then `committed`, then the page cache, then
/// the data file.
pub struct Pager {
    file: File,
    num_pages: u64,
    cache: Arc<DashMap<PageId, Arc<RwLock<Page>>>>,
    cache_size: AtomicUsize,
    max_cache_size: usize,
    wal: WalManager,
    active_group: Option<u64>,
    pending: HashMap<PageId, Page>,
    /// Pages committed to the WAL but not yet checkpointed into the data file.
    /// Authoritative over the data file (and never evicted) until checkpoint.
    committed: HashMap<PageId, Page>,
    /// `num_pages` when the active group began; restored on abort.
    group_start_pages: u64,
    /// Active savepoint: prior `pending` entry of every page written since
    /// the savepoint began (`None` = page was not pending), plus `num_pages`.
    savepoint: Option<Savepoint>,
}

struct Savepoint {
    undo: HashMap<PageId, Option<Page>>,
    num_pages: u64,
}

impl Pager {
    pub fn new(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
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
            committed: HashMap::new(),
            group_start_pages: num_pages,
            savepoint: None,
        };

        pager.recover()?;
        Ok(pager)
    }

    /// Replays any committed WAL groups onto the data file, then truncates the
    /// WAL. The WAL is reset even when nothing was committed, so a torn or
    /// uncommitted tail can never sit in front of later appends. Safe to call
    /// multiple times (an empty WAL yields zero groups).
    fn recover(&mut self) -> Result<()> {
        if self.wal.size() == 0 {
            return Ok(());
        }
        let groups = self.wal.read_committed_groups()?;
        for group in &groups {
            for (page_id, data) in &group.writes {
                self.write_page_raw(*page_id, data)?;
            }
        }
        if !groups.is_empty() {
            self.file.sync_data()?;
        }
        self.wal.truncate()?;
        Ok(())
    }

    /// Copies every committed-but-unapplied page into the data file, fsyncs
    /// it, then truncates the WAL. On failure the pages stay in `committed`
    /// (they are still durable in the WAL) and the next checkpoint retries.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.committed.is_empty() {
            return Ok(());
        }
        let committed = std::mem::take(&mut self.committed);
        let applied = (|| -> Result<()> {
            let mut page_ids: Vec<&PageId> = committed.keys().collect();
            page_ids.sort_unstable();
            for page_id in page_ids {
                self.write_page_raw(*page_id, committed[page_id].data())?;
            }
            self.file.sync_data()?;
            self.wal.truncate()
        })();
        if applied.is_err() {
            self.committed = committed;
        }
        applied
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
        self.group_start_pages = self.num_pages;
        Ok(id)
    }

    /// Whether a write group is currently active.
    pub fn in_group(&self) -> bool {
        self.active_group.is_some()
    }

    /// Commits the active write group: appends every buffered page and a
    /// COMMIT record to the WAL in one write and fsyncs it (the durability
    /// point, and the only fsync on the commit path). The pages move to
    /// `committed`; once the WAL passes `CHECKPOINT_WAL_BYTES` they are
    /// checkpointed into the data file. If writing the WAL fails the group is
    /// aborted.
    pub fn commit_group(&mut self) -> Result<()> {
        let group_id = self.active_group.ok_or_else(|| {
            VelociError::TransactionError("No active write group to commit".to_string())
        })?;
        self.savepoint = None;

        // Empty group: nothing to commit.
        if self.pending.is_empty() {
            self.active_group = None;
            return Ok(());
        }

        let mut page_ids: Vec<PageId> = self.pending.keys().copied().collect();
        page_ids.sort_unstable();
        let pending = &self.pending;
        let logged = self.wal.append_group(
            group_id,
            page_ids.iter().map(|id| (*id, pending[id].data())),
        );
        if let Err(e) = logged {
            // No COMMIT record is durable, so recovery ignores the group.
            let _ = self.abort_group();
            return Err(e);
        }
        self.active_group = None;
        self.committed.extend(std::mem::take(&mut self.pending));

        if self.wal.size() >= CHECKPOINT_WAL_BYTES {
            // The commit is already durable; a failed checkpoint only delays
            // moving pages into the data file and is retried next time.
            if let Err(e) = self.checkpoint() {
                tracing::warn!("checkpoint failed, will retry: {}", e);
            }
        }
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
        self.savepoint = None;
        let pending = std::mem::take(&mut self.pending);
        for page_id in pending.keys() {
            self.evict(*page_id);
        }
        self.num_pages = self.group_start_pages;
        Ok(())
    }

    /// Starts a statement-level savepoint inside the active group. Every page
    /// written until `release_savepoint` / `rollback_to_savepoint` has its
    /// prior buffered state recorded.
    pub fn begin_savepoint(&mut self) -> Result<()> {
        if self.active_group.is_none() {
            return Err(VelociError::TransactionError(
                "Savepoint requires an active write group".to_string(),
            ));
        }
        if self.savepoint.is_some() {
            return Err(VelociError::TransactionError(
                "Savepoint already active".to_string(),
            ));
        }
        self.savepoint = Some(Savepoint {
            undo: HashMap::new(),
            num_pages: self.num_pages,
        });
        Ok(())
    }

    /// Keeps every write made since the savepoint began.
    pub fn release_savepoint(&mut self) {
        self.savepoint = None;
    }

    /// Restores `pending` (and `num_pages`) to their state when the savepoint
    /// began, undoing the writes of the failed statement only.
    pub fn rollback_to_savepoint(&mut self) -> Result<()> {
        let sp = self
            .savepoint
            .take()
            .ok_or_else(|| VelociError::TransactionError("No active savepoint".to_string()))?;
        for (page_id, prior) in sp.undo {
            match prior {
                Some(page) => {
                    self.pending.insert(page_id, page);
                }
                None => {
                    self.pending.remove(&page_id);
                }
            }
            self.evict(page_id);
        }
        self.num_pages = sp.num_pages;
        Ok(())
    }

    fn evict(&self, page_id: PageId) {
        if self.cache.remove(&page_id).is_some() {
            self.cache_size.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Arc<RwLock<Page>>> {
        // Pending writes from the active group take precedence over disk/cache
        // so writers see their own modifications.
        if let Some(page) = self.pending.get(&page_id) {
            return Ok(Arc::new(RwLock::new(page.clone())));
        }

        // Invariant: a cached page always equals its newest version (pending,
        // else committed, else on disk). `write_page` caches what it buffers,
        // and abort / savepoint rollback evict what they discard.
        if let Some(page) = self.cache.get(&page_id) {
            return Ok(page.clone());
        }

        let page = if let Some(page) = self.committed.get(&page_id) {
            page.clone()
        } else {
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
            page
        };

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
        if let Some(sp) = self.savepoint.as_mut() {
            if let Entry::Vacant(e) = sp.undo.entry(page_id) {
                e.insert(self.pending.get(&page_id).cloned());
            }
        }

        // Update pending buffer; the WAL is written at commit.
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
        self.checkpoint()?;
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
    /// Secondary index B-trees, keyed by index name.
    indexes: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
    transaction_manager: Arc<TransactionManager>,
    schema: Arc<RwLock<Schema>>,
    executor: RwLock<Option<Arc<Executor>>>,
    /// Change Data Capture log (Turso-inspired). Disabled by default; see
    /// [`Database::enable_cdc`].
    cdc: Arc<CdcManager>,
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
            indexes: Arc::new(RwLock::new(HashMap::new())),
            transaction_manager,
            schema,
            executor: RwLock::new(None),
            cdc: Arc::new(CdcManager::new()),
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
        let data_copy = self.read_schema_buffer()?;

        if data_copy.len() < 4 {
            return Ok(()); // Empty schema
        }

        let data = &data_copy[..];

        let num_tables = u32::from_le_bytes(
            data[0..4]
                .try_into()
                .map_err(|_| VelociError::Corruption("Failed to read table count".to_string()))?,
        );
        let mut offset: usize = 4;

        for _ in 0..num_tables {
            if offset + 4 > data.len() {
                return Err(VelociError::Corruption(
                    "Schema truncated at table name length".to_string(),
                ));
            }

            let name_len =
                u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
                    VelociError::Corruption("Failed to read table name length".to_string())
                })?) as usize;
            offset += 4;

            if offset + name_len > data.len() {
                return Err(VelociError::Corruption(format!(
                    "Schema truncated at table name (expected {} bytes)",
                    name_len
                )));
            }

            let table_name =
                String::from_utf8(data[offset..offset + name_len].to_vec()).map_err(|_| {
                    VelociError::Corruption(format!(
                        "Invalid UTF-8 in table name at offset {}",
                        offset
                    ))
                })?;
            offset += name_len;

            if offset + 4 > data.len() {
                return Err(VelociError::Corruption(format!(
                    "Schema truncated at column count for table '{}'",
                    table_name
                )));
            }

            let num_cols =
                u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
                    VelociError::Corruption("Failed to read column count".to_string())
                })?) as usize;
            offset += 4;

            let mut columns = Vec::new();
            let mut table_root_page: u64 = 0;
            for _ in 0..num_cols {
                if offset + 4 > data.len() {
                    return Err(VelociError::Corruption(format!(
                        "Schema truncated at column name length in table '{}'",
                        table_name
                    )));
                }

                let col_name_len =
                    u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
                        VelociError::Corruption("Failed to read column name length".to_string())
                    })?) as usize;
                offset += 4;

                if offset + col_name_len + 10 > data.len() {
                    return Err(VelociError::Corruption(format!(
                        "Schema truncated at column data for table '{}'",
                        table_name
                    )));
                }

                let col_name = String::from_utf8(data[offset..offset + col_name_len].to_vec())
                    .map_err(|_| {
                        VelociError::Corruption(format!(
                            "Invalid UTF-8 in column name at offset {}",
                            offset
                        ))
                    })?;
                offset += col_name_len;

                let data_type_byte = data[offset];
                offset += 1;
                let data_type = match data_type_byte {
                    0 => DataType::Integer,
                    1 => DataType::Real,
                    2 => DataType::Text,
                    3 => DataType::Blob,
                    5 => {
                        // Vector columns carry a 4-byte dimension.
                        if offset + 4 > data.len() {
                            return Err(VelociError::Corruption(format!(
                                "Schema truncated at vector dimension for column '{}'",
                                col_name
                            )));
                        }
                        let dim = u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(
                            |_| {
                                VelociError::Corruption(
                                    "Failed to read vector dimension".to_string(),
                                )
                            },
                        )?);
                        offset += 4;
                        DataType::Vector(dim)
                    }
                    _ => {
                        return Err(VelociError::Corruption(format!(
                            "Unknown data type byte {} for column '{}' in table '{}'",
                            data_type_byte, col_name, table_name
                        )))
                    }
                };

                if offset + 9 > data.len() {
                    return Err(VelociError::Corruption(format!(
                        "Schema truncated at column flags for table '{}'",
                        table_name
                    )));
                }
                let flags = data[offset];
                let primary_key = (flags & 1) != 0;
                let not_null = (flags & 2) != 0;
                let unique = (flags & 4) != 0;
                offset += 1;

                let root_page =
                    u64::from_le_bytes(data[offset..offset + 8].try_into().map_err(|_| {
                        VelociError::Corruption("Failed to read root page".to_string())
                    })?);
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
                // Inside an active group (reload after a rollback) the repair
                // joins that group; otherwise it gets one of its own.
                let own_group = !pager.in_group();
                if own_group {
                    pager.begin_group()?;
                }
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
                        if own_group {
                            pager.commit_group()?;
                        }
                        p
                    }
                    Err(e) => {
                        if own_group {
                            let _ = pager.abort_group();
                        }
                        return Err(e);
                    }
                }
            } else {
                table_root_page
            };

            let btree = crate::btree::BTree::from_root(btree_root, Arc::clone(&self.pager));
            self.btrees
                .write()
                .insert(table_name.clone(), Arc::new(RwLock::new(btree)));

            let table_schema = TableSchema {
                name: table_name,
                columns,
                root_page: btree_root,
                indexes: Vec::new(),
            };
            self.schema.write().create_table(table_schema)?;
        }

        // Optional trailing index section (absent in files written before
        // secondary indexes existed):
        // [INDEX_SECTION_MAGIC][num_indexes: u32] then per index
        // [table_len: u32][table][name_len: u32][name][col_len: u32][col][root: u64]
        // Older versions could leave stale bytes after the tables (see
        // `save_schema`), so trailing data without the magic is ignored.
        let mut reader = SchemaReader { data, offset };
        if data.get(offset..offset + INDEX_SECTION_MAGIC.len()) == Some(INDEX_SECTION_MAGIC) {
            reader.bytes(INDEX_SECTION_MAGIC.len(), "index section")?;
            let num_indexes = reader.u32("index count")?;
            for _ in 0..num_indexes {
                let table = reader.string("index table")?;
                let name = reader.string("index name")?;
                let column = reader.string("index column")?;
                let root_page = reader.u64("index root page")?;
                let btree = BTree::from_root(root_page, Arc::clone(&self.pager));
                self.indexes
                    .write()
                    .insert(name.clone(), Arc::new(RwLock::new(btree)));
                self.schema.write().get_table_mut(&table)?.indexes.push(
                    crate::index::IndexSchema {
                        name,
                        column,
                        root_page,
                    },
                );
            }
        }

        Ok(())
    }

    /// Discards the in-memory schema and B-tree handles and rebuilds them from
    /// the pager's current view (the data file plus any pending pages). Called
    /// after a rollback so in-memory roots and columns match storage again.
    fn reload_schema(&self) -> Result<()> {
        *self.schema.write() = Schema::new();
        self.btrees.write().clear();
        self.indexes.write().clear();
        self.load_schema()
    }

    /// Serializes the schema into the schema page chain. Runs inside the
    /// caller's active write group so the schema commits (or rolls back)
    /// atomically with the statement that changed it.
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

                    // Data type (vector columns append their dimension)
                    match column.data_type {
                        DataType::Integer => buffer.push(0u8),
                        DataType::Real => buffer.push(1u8),
                        DataType::Text => buffer.push(2u8),
                        DataType::Blob => buffer.push(3u8),
                        DataType::Null => buffer.push(4u8),
                        DataType::Vector(dim) => {
                            buffer.push(5u8);
                            buffer.extend_from_slice(&dim.to_le_bytes());
                        }
                    }

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

        let indexed: Vec<(&String, &crate::index::IndexSchema)> = schema
            .list_tables()
            .iter()
            .filter_map(|t| schema.get_table(t).ok())
            .flat_map(|t| t.indexes.iter().map(move |i| (&t.name, i)))
            .collect();
        buffer.extend_from_slice(INDEX_SECTION_MAGIC);
        buffer.extend_from_slice(&(indexed.len() as u32).to_le_bytes());
        for (table, index) in indexed {
            let root_page = match self.indexes.read().get(&index.name) {
                Some(bt) => bt.read().root_page(),
                None => {
                    return Err(VelociError::Corruption(format!(
                        "No B-tree for index '{}'",
                        index.name
                    )))
                }
            };
            for part in [table, &index.name, &index.column] {
                buffer.extend_from_slice(&(part.len() as u32).to_le_bytes());
                buffer.extend_from_slice(part.as_bytes());
            }
            buffer.extend_from_slice(&root_page.to_le_bytes());
        }

        drop(schema);

        let mut pager = self.pager.write();
        debug_assert!(
            pager.in_group(),
            "save_schema must run inside a write group"
        );
        // Reuse the current chain's overflow pages, then allocate more.
        // Never write to pages the chain does not own: they belong to
        // B-trees.
        let mut pages = vec![SCHEMA_HEAD_PAGE];
        pages.extend(schema_chain(&mut pager)?.into_iter().skip(1));
        let chunks: Vec<&[u8]> = if buffer.is_empty() {
            vec![&[]]
        } else {
            buffer.chunks(SCHEMA_CHUNK).collect()
        };
        while pages.len() < chunks.len() {
            pages.push(pager.allocate_page()?);
        }

        for (i, chunk) in chunks.iter().enumerate() {
            let next = pages
                .get(i + 1)
                .filter(|_| i + 1 < chunks.len())
                .copied()
                .unwrap_or(0);
            let mut page = crate::storage::Page::new();
            let data = page.data_mut();
            data[0..4].copy_from_slice(&(chunk.len() as u32 | SCHEMA_LINKED).to_le_bytes());
            data[4..12].copy_from_slice(&next.to_le_bytes());
            data[SCHEMA_PAGE_HEADER..SCHEMA_PAGE_HEADER + chunk.len()].copy_from_slice(chunk);
            pager.write_page(pages[i], &page)?;
        }
        // Overflow pages no longer needed are leaked (there is no free list).

        Ok(())
    }

    /// Reads the serialized schema from its page chain (either format).
    fn read_schema_buffer(&self) -> Result<Vec<u8>> {
        let mut pager = self.pager.write();
        if pager.num_pages() <= SCHEMA_HEAD_PAGE {
            return Ok(Vec::new());
        }
        let head = pager.read_page(SCHEMA_HEAD_PAGE)?.read().data().to_vec();
        let raw_len = u32::from_le_bytes(head[0..4].try_into().unwrap());
        let mut out = Vec::new();

        if raw_len & SCHEMA_LINKED == 0 {
            // Legacy format (before 0.4): `[len: u32][chunk]` on consecutive
            // pages from page 1, ending at the first short chunk. Those
            // pages past page 1 may since have been reused by B-trees, so
            // only the data is read; a later save rewrites it as a chain.
            let mut page_id = SCHEMA_HEAD_PAGE;
            let mut data = head;
            loop {
                let len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
                let len = len.min(PAGE_SIZE - 4);
                out.extend_from_slice(&data[4..4 + len]);
                page_id += 1;
                if len < PAGE_SIZE - 4 || page_id >= pager.num_pages() {
                    return Ok(out);
                }
                data = pager.read_page(page_id)?.read().data().to_vec();
            }
        }

        for page_id in schema_chain(&mut pager)? {
            let data = pager.read_page(page_id)?.read().data().to_vec();
            let len =
                (u32::from_le_bytes(data[0..4].try_into().unwrap()) & !SCHEMA_LINKED) as usize;
            if len > SCHEMA_CHUNK {
                return Err(VelociError::Corruption(format!(
                    "Schema page {} has invalid length {}",
                    page_id, len
                )));
            }
            out.extend_from_slice(&data[SCHEMA_PAGE_HEADER..SCHEMA_PAGE_HEADER + len]);
        }
        Ok(out)
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
            Arc::clone(&self.indexes),
            Arc::clone(&self.schema),
            Arc::clone(&self.transaction_manager),
            Arc::clone(&self.cdc),
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

        match statement {
            Statement::BeginTransaction => return self.begin(),
            Statement::CommitTransaction => return self.commit(),
            Statement::RollbackTransaction => return self.rollback(),
            _ => {}
        }

        let executor = self.get_or_create_executor();

        // Serialize writers so the pager only ever has a single active WAL
        // group. We do NOT hold `pager.write()` across the executor call —
        // the executor takes the pager lock many times internally and a
        // recursive write lock would deadlock. The active_group field on
        // Pager persists across lock releases, so `write_page` calls from the
        // executor still see the group and buffer correctly.
        let _writer = self.writer.lock();

        // Inside an explicit transaction the WAL group opened by `begin` stays
        // open; the statement runs under a savepoint so a failure undoes only
        // this statement. Otherwise the statement is its own group.
        let in_txn = executor.in_transaction();

        let needs_schema_save = matches!(
            statement,
            Statement::CreateTable { .. }
                | Statement::DropTable { .. }
                | Statement::AlterTable { .. }
                | Statement::CreateIndex { .. }
                | Statement::DropIndex { .. }
        );

        // Capture root pages before the statement so we can detect any
        // B-tree root changes (caused by splits or root-collapse during
        // underflow). If a root changed, the schema page must be re-saved
        // so a subsequent open finds the correct root.
        let roots_before = self.snapshot_roots();
        let cdc_mark = self.cdc.staged_mark();

        if in_txn {
            self.pager.write().begin_savepoint()?;
        } else {
            self.pager.write().begin_group()?;
        }

        // The schema is saved inside the same group so DDL and root changes
        // commit atomically with the rows they describe.
        let result = executor.execute_statement(statement).and_then(|()| {
            if needs_schema_save || roots_before != self.snapshot_roots() {
                self.save_schema()?;
            }
            Ok(())
        });

        match result {
            Ok(()) => {
                if in_txn {
                    self.pager.write().release_savepoint();
                } else {
                    let committed = self.pager.write().commit_group();
                    if let Err(e) = committed {
                        // commit_group already aborted the group.
                        self.cdc.discard_staged_from(cdc_mark);
                        self.reload_schema()?;
                        return Err(e);
                    }
                    self.cdc.publish_staged();
                }
                Ok(())
            }
            Err(e) => {
                if in_txn {
                    self.pager.write().rollback_to_savepoint()?;
                } else {
                    self.pager.write().abort_group()?;
                }
                self.cdc.discard_staged_from(cdc_mark);
                // The statement may have changed in-memory roots or columns
                // before failing; rebuild them from storage.
                self.reload_schema()?;
                Err(e)
            }
        }
    }

    /// Root page of every table (`false`, name) and index (`true`, name).
    fn snapshot_roots(&self) -> HashMap<(bool, String), PageId> {
        let mut out = HashMap::new();
        for (name, bt) in self.btrees.read().iter() {
            out.insert((false, name.clone()), bt.read().root_page());
        }
        for (name, bt) in self.indexes.read().iter() {
            out.insert((true, name.clone()), bt.read().root_page());
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
    ///
    /// Every statement until [`Database::commit`] or [`Database::rollback`]
    /// joins a single WAL group: nothing is durable until COMMIT, and ROLLBACK
    /// discards all of it. The transaction is database-wide (a `Database` is
    /// one session): statements from any thread join it, and readers see its
    /// uncommitted writes.
    pub fn begin(&self) -> Result<()> {
        let executor = self.get_or_create_executor();
        let _writer = self.writer.lock();
        executor.begin_transaction()?;
        if let Err(e) = self.pager.write().begin_group() {
            let _ = executor.rollback_transaction();
            return Err(e);
        }
        Ok(())
    }

    /// Commits the current explicit transaction, making all of its writes
    /// durable atomically. If the commit fails, the transaction is rolled back.
    pub fn commit(&self) -> Result<()> {
        let executor = self.get_or_create_executor();
        let _writer = self.writer.lock();
        if !executor.in_transaction() {
            return Err(VelociError::TransactionError(
                "No active transaction to commit".to_string(),
            ));
        }
        let committed = self.pager.write().commit_group();
        if let Err(e) = committed {
            // commit_group already aborted the group.
            self.cdc.discard_staged_from(0);
            let _ = executor.rollback_transaction();
            self.reload_schema()?;
            return Err(e);
        }
        self.cdc.publish_staged();
        executor.commit_transaction()
    }

    /// Rolls back the current explicit transaction, discarding every change
    /// made since [`Database::begin`], including schema changes.
    pub fn rollback(&self) -> Result<()> {
        let executor = self.get_or_create_executor();
        let _writer = self.writer.lock();
        if !executor.in_transaction() {
            return Err(VelociError::TransactionError(
                "No active transaction to rollback".to_string(),
            ));
        }
        self.pager.write().abort_group()?;
        self.cdc.discard_staged_from(0);
        self.reload_schema()?;
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

    /// Enables Change Data Capture. Subsequent committed INSERT / UPDATE /
    /// DELETE statements are recorded and can be polled with
    /// [`Database::changes_since`].
    pub fn enable_cdc(&self) {
        self.cdc.enable();
    }

    /// Disables Change Data Capture and clears the change log.
    pub fn disable_cdc(&self) {
        self.cdc.disable();
    }

    /// Returns whether CDC is currently enabled.
    pub fn cdc_enabled(&self) -> bool {
        self.cdc.is_enabled()
    }

    /// Returns all captured changes with sequence number greater than `since`.
    /// Pass 0 to receive everything currently retained.
    pub fn changes_since(&self, since: u64) -> Vec<ChangeEvent> {
        self.cdc.changes_since(since)
    }

    /// The sequence number of the most recent captured change (0 if none).
    pub fn cdc_latest_seq(&self) -> u64 {
        self.cdc.latest_seq()
    }

    /// Exact K-nearest-neighbour search over a vector column.
    ///
    /// Returns up to `k` `(distance, row)` pairs ordered by ascending
    /// distance. Distance computation is parallelized for large tables.
    ///
    /// Equivalent SQL:
    /// `SELECT * FROM <table> ORDER BY vector_distance_cos(<column>, vector32('[...]')) LIMIT k`
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # use velocidb::{Database, vector::DistanceMetric};
    /// # let db = Database::open("test.db").unwrap();
    /// let neighbors = db
    ///     .vector_search("docs", "embedding", &[0.1, 0.2, 0.3], 5, DistanceMetric::Cosine)
    ///     .unwrap();
    /// for (distance, row) in neighbors {
    ///     println!("{:.4}: {:?}", distance, row.values);
    /// }
    /// ```
    pub fn vector_search(
        &self,
        table: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: DistanceMetric,
    ) -> Result<Vec<(f64, Row)>> {
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        };

        let col_index = table_schema
            .columns
            .iter()
            .position(|c| c.name == column)
            .ok_or_else(|| {
                VelociError::NotFound(format!(
                    "Column '{}' not found in table '{}'",
                    column, table
                ))
            })?;

        if let DataType::Vector(dim) = table_schema.columns[col_index].data_type {
            if dim as usize != query.len() {
                return Err(VelociError::TypeMismatch {
                    expected: format!("query vector of dimension {}", dim),
                    actual: format!("dimension {}", query.len()),
                });
            }
        }

        let all_rows = {
            let btrees = self.btrees.read();
            let btree_arc = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;
            let btree = btree_arc.read();
            btree.scan()?
        };

        Ok(vector::knn(all_rows, col_index, query, metric, k)
            .into_iter()
            .map(|(d, _, row)| (d, row))
            .collect())
    }

    /// Lists all tables in the database.
    pub fn list_tables(&self) -> Vec<String> {
        self.schema.read().list_tables()
    }

    /// Returns a human-readable `CREATE TABLE` statement for the named table,
    /// followed by a `CREATE INDEX` statement per index (`;`-separated),
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
                DataType::Integer => "INTEGER".to_string(),
                DataType::Real => "REAL".to_string(),
                DataType::Text => "TEXT".to_string(),
                DataType::Blob => "BLOB".to_string(),
                DataType::Null => "NULL".to_string(),
                DataType::Vector(dim) => format!("F32_BLOB({})", dim),
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
        for index in &table.indexes {
            out.push_str(&format!(
                ";\nCREATE INDEX {} ON {} ({})",
                index.name, table.name, index.column
            ));
        }
        Ok(out)
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Ensure data is flushed to disk when database is dropped
        let _ = self.pager.write().flush();
    }
}

/// The schema chain starts at page 1 (page 0 is reserved).
const SCHEMA_HEAD_PAGE: PageId = 1;
/// Flag in a schema page's length word marking the linked format:
/// `[len | SCHEMA_LINKED: u32][next_page: u64][chunk]`, `next_page` 0 at the
/// end. Legacy pages (`[len: u32][chunk]`, consecutive) never set it.
const SCHEMA_LINKED: u32 = 0x8000_0000;
const SCHEMA_PAGE_HEADER: usize = 12;
const SCHEMA_CHUNK: usize = PAGE_SIZE - SCHEMA_PAGE_HEADER;

/// Pages of a linked-format schema chain, head first. Empty for a legacy or
/// blank head page (which owns no overflow pages).
fn schema_chain(pager: &mut Pager) -> Result<Vec<PageId>> {
    let mut chain = Vec::new();
    let mut page_id = SCHEMA_HEAD_PAGE;
    while page_id != 0 {
        if page_id >= pager.num_pages() || chain.len() as u64 > pager.num_pages() {
            return Err(VelociError::Corruption(format!(
                "Schema chain points at invalid page {}",
                page_id
            )));
        }
        let arc = pager.read_page(page_id)?;
        let page = arc.read();
        let data = page.data();
        if u32::from_le_bytes(data[0..4].try_into().unwrap()) & SCHEMA_LINKED == 0 {
            if chain.is_empty() {
                return Ok(chain);
            }
            return Err(VelociError::Corruption(format!(
                "Schema chain page {} is not a schema page",
                page_id
            )));
        }
        chain.push(page_id);
        page_id = u64::from_le_bytes(data[4..12].try_into().unwrap());
    }
    Ok(chain)
}

/// Marks the index section that follows the tables in the schema buffer.
const INDEX_SECTION_MAGIC: &[u8] = b"VDBIDX01";

/// Bounds-checked little-endian reader over the schema buffer.
struct SchemaReader<'a> {
    data: &'a [u8],
    offset: usize,
}

impl SchemaReader<'_> {
    fn bytes(&mut self, n: usize, what: &str) -> Result<&[u8]> {
        let bytes = self
            .data
            .get(self.offset..self.offset + n)
            .ok_or_else(|| VelociError::Corruption(format!("Schema truncated at {}", what)))?;
        self.offset += n;
        Ok(bytes)
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4, what)?.try_into().unwrap()))
    }

    fn u64(&mut self, what: &str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8, what)?.try_into().unwrap()))
    }

    fn string(&mut self, what: &str) -> Result<String> {
        let len = self.u32(what)? as usize;
        String::from_utf8(self.bytes(len, what)?.to_vec())
            .map_err(|_| VelociError::Corruption(format!("Invalid UTF-8 in {}", what)))
    }
}

// Schema management
#[derive(Debug, Clone)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<crate::types::Column>,
    pub root_page: PageId,
    /// Secondary indexes on this table's columns.
    pub indexes: Vec<crate::index::IndexSchema>,
}

impl TableSchema {
    pub fn index_on(&self, column: &str) -> Option<&crate::index::IndexSchema> {
        self.indexes.iter().find(|i| i.column == column)
    }
}

pub struct Schema {
    tables: HashMap<String, TableSchema>,
}

impl Default for Schema {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn get_table_mut(&mut self, name: &str) -> Result<&mut TableSchema> {
        self.tables
            .get_mut(name)
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

    /// The table that owns index `name`, if any.
    pub fn table_of_index(&self, name: &str) -> Option<&TableSchema> {
        self.tables
            .values()
            .find(|t| t.indexes.iter().any(|i| i.name == name))
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

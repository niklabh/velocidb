//! Write-ahead log (WAL) for crash recovery.
//!
//! # Semantics
//!
//! VelociDB uses a "no-steal" WAL with checkpointing: page writes performed
//! while a write group is active are buffered in memory (the pager's
//! `pending` map). On commit the final image of every buffered page plus a
//! COMMIT record is appended to the WAL in a single write and the WAL is
//! fsynced — the only fsync on the commit path. A crash before that fsync
//! leaves no committed group in the WAL.
//!
//! Committed pages are not written to the data file at commit. The pager
//! serves them from memory until a *checkpoint* copies them into the data
//! file, fsyncs it, and truncates the WAL. Checkpoints run when the WAL
//! exceeds a size threshold and when the database is closed; recovery on
//! open replays every committed group and then resets the WAL.
//!
//! The WAL tracks the end of its last committed group (`len`). Appends always
//! start there, and a failed append truncates back to it, so a partially
//! written record can never sit in front of later committed groups.
//!
//! # File format
//!
//! The WAL is a sequence of records:
//!
//! ```text
//! [type: u8] [group_id: u64 LE] [len: u32 LE] [payload: len bytes] [crc32: u32 LE]
//! ```
//!
//! `crc32` covers the entire preceding portion of the record. Record types:
//!
//! - `1 = PAGE_WRITE`: payload is `[page_id: u64 LE] [page_data: PAGE_SIZE]`.
//! - `2 = COMMIT`: payload is empty.
//!
//! Recovery reads records in order, stopping on the first CRC mismatch or
//! truncation (a "torn" record at the tail is treated as if it were never
//! written, which matches the "no-steal" semantics — uncommitted records are
//! discarded). Only groups with a `COMMIT` record have their `PAGE_WRITE`
//! records applied to the data file.

use crate::storage::PAGE_SIZE;
use crate::types::{PageId, Result, VelociError};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const REC_PAGE_WRITE: u8 = 1;
const REC_COMMIT: u8 = 2;

/// A committed group, as recovered from the WAL.
#[derive(Debug)]
pub struct CommittedGroup {
    pub group_id: u64,
    pub writes: Vec<(PageId, Vec<u8>)>,
}

/// Append-only WAL backing one VelociDB file.
#[allow(dead_code)]
pub struct WalManager {
    /// Kept for debugging / future log-rotation; not currently read.
    path: PathBuf,
    file: File,
    next_group_id: u64,
    /// Byte length of the WAL up to the end of the last committed group.
    len: u64,
}

impl WalManager {
    /// Opens (creating if needed) the WAL companion file for the given DB path.
    /// The WAL path is `<db>-wal`.
    pub fn open(db_path: &Path) -> Result<Self> {
        let wal_path = wal_path_for(db_path);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false) // committed groups must survive for recovery
            .open(&wal_path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            path: wal_path,
            file,
            next_group_id: 1,
            len,
        })
    }

    /// Path to the WAL file on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current WAL size in bytes (end of the last committed group).
    pub fn size(&self) -> u64 {
        self.len
    }

    /// Appends a whole committed group — one PAGE_WRITE record per page, then
    /// a COMMIT record — in a single write, and fsyncs. After this returns
    /// successfully the group is durable. On failure the WAL is truncated
    /// back to its previous committed length.
    pub fn append_group<'a>(
        &mut self,
        group_id: u64,
        pages: impl IntoIterator<Item = (PageId, &'a [u8])>,
    ) -> Result<()> {
        let mut buf = Vec::new();
        for (page_id, data) in pages {
            if data.len() != PAGE_SIZE {
                return Err(VelociError::StorageError(format!(
                    "WAL page write expected {} bytes, got {}",
                    PAGE_SIZE,
                    data.len()
                )));
            }
            let mut payload = Vec::with_capacity(8 + PAGE_SIZE);
            payload.extend_from_slice(&page_id.to_le_bytes());
            payload.extend_from_slice(data);
            encode_record(&mut buf, REC_PAGE_WRITE, group_id, &payload);
        }
        encode_record(&mut buf, REC_COMMIT, group_id, &[]);

        let written = (|| -> Result<()> {
            self.file.seek(SeekFrom::Start(self.len))?;
            self.file.write_all(&buf)?;
            self.file.sync_data()?;
            Ok(())
        })();
        match written {
            Ok(()) => {
                self.len += buf.len() as u64;
                Ok(())
            }
            Err(e) => {
                // Best effort: drop the partial group so later appends are not
                // stranded behind a torn record. If this also fails, recovery
                // still ignores the group (no durable COMMIT).
                let _ = self.file.set_len(self.len);
                Err(e)
            }
        }
    }

    /// Returns the next group id, advancing the counter.
    pub fn allocate_group_id(&mut self) -> u64 {
        let id = self.next_group_id;
        self.next_group_id = self.next_group_id.wrapping_add(1);
        id
    }

    /// Bumps the internal group counter to at least `floor + 1`. Used after
    /// recovery so newly allocated ids do not collide with any group seen on
    /// disk (committed or not).
    pub fn observe_group_id(&mut self, floor: u64) {
        if self.next_group_id <= floor {
            self.next_group_id = floor.wrapping_add(1);
        }
    }

    /// Appends a `PAGE_WRITE` record. Does NOT fsync; durability is provided
    /// by the subsequent `log_commit` call.
    pub fn log_page_write(&mut self, group_id: u64, page_id: PageId, data: &[u8]) -> Result<()> {
        if data.len() != PAGE_SIZE {
            return Err(VelociError::StorageError(format!(
                "WAL page write expected {} bytes, got {}",
                PAGE_SIZE,
                data.len()
            )));
        }
        let mut payload = Vec::with_capacity(8 + PAGE_SIZE);
        payload.extend_from_slice(&page_id.to_le_bytes());
        payload.extend_from_slice(data);
        self.append_record(REC_PAGE_WRITE, group_id, &payload)
    }

    /// Appends a `COMMIT` record and fsyncs the WAL file. After this returns
    /// successfully, the committed group is durable.
    pub fn log_commit(&mut self, group_id: u64) -> Result<()> {
        self.append_record(REC_COMMIT, group_id, &[])?;
        self.file.sync_data()?;
        Ok(())
    }

    /// Truncates the WAL to zero length and fsyncs. Call only after the data
    /// file has been fsynced so that all committed page writes are durable in
    /// their final home.
    pub fn truncate(&mut self) -> Result<()> {
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.sync_all()?;
        self.len = 0;
        Ok(())
    }

    /// Scans the WAL and returns committed groups in WAL order. A truncated or
    /// CRC-failing record at the tail is silently ignored (it represents a
    /// crash mid-record and therefore an uncommitted group).
    pub fn read_committed_groups(&mut self) -> Result<Vec<CommittedGroup>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut buf = Vec::new();
        self.file.read_to_end(&mut buf)?;

        // Pass 1: parse every record we can. Stop at first malformed record.
        struct ParsedRecord {
            rec_type: u8,
            group_id: u64,
            payload: Vec<u8>,
        }
        let mut records: Vec<ParsedRecord> = Vec::new();
        let mut cursor = 0usize;
        let mut highest_observed_group: u64 = 0;
        while cursor < buf.len() {
            // Need at least header (1 + 8 + 4) + crc (4) = 17 bytes.
            if cursor + 1 + 8 + 4 + 4 > buf.len() {
                break;
            }
            let rec_type = buf[cursor];
            let group_id = u64::from_le_bytes(buf[cursor + 1..cursor + 9].try_into().unwrap());
            let len = u32::from_le_bytes(buf[cursor + 9..cursor + 13].try_into().unwrap()) as usize;

            let payload_start = cursor + 13;
            let payload_end = payload_start + len;
            let crc_end = payload_end + 4;
            if crc_end > buf.len() {
                // Torn record at tail.
                break;
            }
            let payload = &buf[payload_start..payload_end];
            let crc_bytes = &buf[payload_end..crc_end];
            let stored_crc = u32::from_le_bytes(crc_bytes.try_into().unwrap());

            let mut hasher = crc32fast::Hasher::new();
            hasher.update(&buf[cursor..payload_end]);
            let computed = hasher.finalize();
            if computed != stored_crc {
                // Corruption / torn record — stop here.
                break;
            }

            records.push(ParsedRecord {
                rec_type,
                group_id,
                payload: payload.to_vec(),
            });
            if group_id > highest_observed_group {
                highest_observed_group = group_id;
            }
            cursor = crc_end;
        }

        self.observe_group_id(highest_observed_group);

        // Pass 2: collect page writes per group; emit groups in commit order.
        use std::collections::HashMap;
        let mut pending_writes: HashMap<u64, Vec<(PageId, Vec<u8>)>> = HashMap::new();
        let mut committed: Vec<CommittedGroup> = Vec::new();
        for rec in records {
            match rec.rec_type {
                REC_PAGE_WRITE => {
                    if rec.payload.len() < 8 + PAGE_SIZE {
                        return Err(VelociError::Corruption(format!(
                            "WAL PAGE_WRITE record too short: {} bytes",
                            rec.payload.len()
                        )));
                    }
                    let page_id = u64::from_le_bytes(rec.payload[0..8].try_into().unwrap());
                    let data = rec.payload[8..8 + PAGE_SIZE].to_vec();
                    pending_writes
                        .entry(rec.group_id)
                        .or_default()
                        .push((page_id, data));
                }
                REC_COMMIT => {
                    let writes = pending_writes.remove(&rec.group_id).unwrap_or_default();
                    committed.push(CommittedGroup {
                        group_id: rec.group_id,
                        writes,
                    });
                }
                _ => {
                    return Err(VelociError::Corruption(format!(
                        "Unknown WAL record type: {}",
                        rec.rec_type
                    )));
                }
            }
        }

        Ok(committed)
    }

    fn append_record(&mut self, rec_type: u8, group_id: u64, payload: &[u8]) -> Result<()> {
        let mut buf = Vec::with_capacity(17 + payload.len());
        encode_record(&mut buf, rec_type, group_id, payload);
        self.file.seek(SeekFrom::Start(self.len))?;
        self.file.write_all(&buf)?;
        self.len += buf.len() as u64;
        Ok(())
    }
}

/// Appends one encoded record (`type | group_id | len | payload | crc32`) to
/// `buf`.
fn encode_record(buf: &mut Vec<u8>, rec_type: u8, group_id: u64, payload: &[u8]) {
    let start = buf.len();
    buf.push(rec_type);
    buf.extend_from_slice(&group_id.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&buf[start..]);
    let crc = hasher.finalize();
    buf.extend_from_slice(&crc.to_le_bytes());
}

/// Returns the WAL path corresponding to a database file path. The suffix is
/// `-wal` (no dot) so that callers can rely on `path.exists()` semantics
/// without colliding with extension parsing.
pub fn wal_path_for(db_path: &Path) -> PathBuf {
    let mut s = db_path.as_os_str().to_owned();
    s.push("-wal");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn make_page(byte: u8) -> Vec<u8> {
        vec![byte; PAGE_SIZE]
    }

    #[test]
    fn test_wal_roundtrip_simple() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("test.db");

        let mut wal = WalManager::open(&db).unwrap();
        let g = wal.allocate_group_id();
        wal.log_page_write(g, 0, &make_page(0xaa)).unwrap();
        wal.log_page_write(g, 1, &make_page(0xbb)).unwrap();
        wal.log_commit(g).unwrap();
        drop(wal);

        let mut wal = WalManager::open(&db).unwrap();
        let groups = wal.read_committed_groups().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].writes.len(), 2);
        assert_eq!(groups[0].writes[0].0, 0);
        assert_eq!(groups[0].writes[0].1, make_page(0xaa));
        assert_eq!(groups[0].writes[1].0, 1);
        assert_eq!(groups[0].writes[1].1, make_page(0xbb));
    }

    #[test]
    fn test_wal_skips_uncommitted_groups() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("test.db");

        let mut wal = WalManager::open(&db).unwrap();
        let g1 = wal.allocate_group_id();
        wal.log_page_write(g1, 5, &make_page(1)).unwrap();
        // Note: no commit for g1.
        let g2 = wal.allocate_group_id();
        wal.log_page_write(g2, 6, &make_page(2)).unwrap();
        wal.log_commit(g2).unwrap();
        drop(wal);

        let mut wal = WalManager::open(&db).unwrap();
        let groups = wal.read_committed_groups().unwrap();
        // Only g2 (committed) should be returned.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_id, g2);
        assert_eq!(groups[0].writes.len(), 1);
        assert_eq!(groups[0].writes[0].0, 6);
    }

    #[test]
    fn test_wal_truncation_yields_no_records() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("test.db");

        let mut wal = WalManager::open(&db).unwrap();
        let g = wal.allocate_group_id();
        wal.log_page_write(g, 0, &make_page(0xcc)).unwrap();
        wal.log_commit(g).unwrap();
        wal.truncate().unwrap();

        let groups = wal.read_committed_groups().unwrap();
        assert!(groups.is_empty());
    }

    #[test]
    fn test_wal_tolerates_torn_tail() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("test.db");

        let mut wal = WalManager::open(&db).unwrap();
        let g1 = wal.allocate_group_id();
        wal.log_page_write(g1, 1, &make_page(1)).unwrap();
        wal.log_commit(g1).unwrap();
        let g2 = wal.allocate_group_id();
        wal.log_page_write(g2, 2, &make_page(2)).unwrap();
        wal.log_commit(g2).unwrap();
        drop(wal);

        // Simulate a crash mid-record by truncating the file by 5 bytes.
        let path = wal_path_for(&db);
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let len = f.metadata().unwrap().len();
        f.set_len(len - 5).unwrap();
        drop(f);

        let mut wal = WalManager::open(&db).unwrap();
        let groups = wal.read_committed_groups().unwrap();
        // Only the first group is fully present.
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].group_id, g1);
    }

    #[test]
    fn test_append_group_roundtrip_and_size() {
        let dir = tempdir().unwrap();
        let db = dir.path().join("test.db");

        let mut wal = WalManager::open(&db).unwrap();
        assert_eq!(wal.size(), 0);
        let (a, b) = (make_page(1), make_page(2));
        let g1 = wal.allocate_group_id();
        wal.append_group(g1, [(3, a.as_slice()), (4, b.as_slice())])
            .unwrap();
        let after_first = wal.size();
        // Two PAGE_WRITE records + COMMIT, each with a 17-byte envelope.
        assert_eq!(after_first, 2 * (17 + 8 + PAGE_SIZE as u64) + 17);
        let g2 = wal.allocate_group_id();
        wal.append_group(g2, [(3, b.as_slice())]).unwrap();
        assert!(wal.size() > after_first);
        drop(wal);

        let mut wal = WalManager::open(&db).unwrap();
        let groups = wal.read_committed_groups().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].writes, vec![(3, a.clone()), (4, b.clone())]);
        assert_eq!(groups[1].writes, vec![(3, b)]);

        wal.truncate().unwrap();
        assert_eq!(wal.size(), 0);
        assert!(wal.read_committed_groups().unwrap().is_empty());
    }

    #[test]
    fn test_append_group_rejects_wrong_page_size() {
        let dir = tempdir().unwrap();
        let mut wal = WalManager::open(&dir.path().join("test.db")).unwrap();
        let g = wal.allocate_group_id();
        assert!(wal.append_group(g, [(0, &[0u8; 10][..])]).is_err());
        assert_eq!(wal.size(), 0);
    }
}

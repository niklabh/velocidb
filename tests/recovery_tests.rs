//! Crash / recovery tests for the WAL.
//!
//! These tests exercise three scenarios:
//!  1. A clean process restart: data inserted in one process must be visible
//!     to a process that re-opens the same file.
//!  2. A simulated crash mid-WAL: a partial (torn) WAL record at the tail is
//!     discarded by recovery; data from committed groups is preserved.
//!  3. The WAL file is truncated to zero (or removed) between sessions, in
//!     which case nothing extra is recovered but the data file is still
//!     readable.
//!
//! The "crash" here is approximated by dropping the `Database` (which flushes
//! cleanly), then surgically corrupting the WAL file before re-opening.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use tempfile::tempdir;
use velocidb::storage::{Database, PAGE_SIZE};
use velocidb::types::Value;
use velocidb::wal::wal_path_for;

fn open_db(p: &PathBuf) -> std::sync::Arc<Database> {
    Database::open(p).unwrap()
}

#[test]
fn test_data_survives_clean_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("clean.db");

    {
        let db = open_db(&path);
        db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO u VALUES (1, 'Alice')").unwrap();
        db.execute("INSERT INTO u VALUES (2, 'Bob')").unwrap();
        db.execute("INSERT INTO u VALUES (3, 'Charlie')").unwrap();
        db.close().unwrap();
    }

    {
        let db = open_db(&path);
        let r = db.query("SELECT * FROM u ORDER BY id").unwrap();
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0].values[1], Value::Text("Alice".to_string()));
        assert_eq!(r.rows[1].values[1], Value::Text("Bob".to_string()));
        assert_eq!(r.rows[2].values[1], Value::Text("Charlie".to_string()));
    }
}

#[test]
fn test_wal_is_truncated_after_clean_commits() {
    // After a clean run, all committed WAL groups should have been applied to
    // the data file and the WAL truncated to zero length.
    let dir = tempdir().unwrap();
    let path = dir.path().join("trunc.db");

    {
        let db = open_db(&path);
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        for i in 0..50 {
            db.execute(&format!("INSERT INTO t VALUES ({}, {})", i, i)).unwrap();
        }
        db.close().unwrap();
    }

    let wal_len = std::fs::metadata(wal_path_for(&path)).unwrap().len();
    assert_eq!(wal_len, 0, "WAL should be truncated after clean shutdown");
}

#[test]
fn test_torn_wal_tail_is_discarded() {
    // Simulate a crash mid-record at the WAL tail: corrupt the last few bytes
    // of a freshly written WAL so the final commit record fails CRC. The
    // affected group must be skipped by recovery without aborting the open.
    let dir = tempdir().unwrap();
    let path = dir.path().join("torn.db");

    // Phase A: create the schema and commit several rows so the data file
    // contains the schema.
    {
        let db = open_db(&path);
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        for i in 0..3 {
            db.execute(&format!("INSERT INTO t VALUES ({}, {})", i, i * 10)).unwrap();
        }
        db.close().unwrap(); // truncates WAL after each commit
    }

    // Phase B: write more rows but corrupt the WAL after the *last* commit's
    // fsync but BEFORE truncation. We approximate this by appending some
    // bytes to the WAL after a normal session.
    //
    // After Phase A the WAL is empty. Open the DB, do work that commits and
    // truncates the WAL several times, then drop the DB. The WAL is empty.
    // Now append a partially-formed record to simulate a crash mid-write
    // BEFORE the COMMIT marker was fsynced. Recovery should ignore it.
    {
        let db = open_db(&path);
        db.execute("INSERT INTO t VALUES (3, 30)").unwrap();
        db.execute("INSERT INTO t VALUES (4, 40)").unwrap();
        db.close().unwrap();
    }

    // Now simulate a partial PAGE_WRITE record sitting at the WAL tail. This
    // models a crash after begin_group but before commit_group on a
    // subsequent transaction that never made it to disk.
    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(wal_path_for(&path))
            .unwrap();
        // Write a few bytes of garbage — a half-formed record without a CRC.
        f.seek(SeekFrom::End(0)).unwrap();
        let garbage = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]; // not a complete header
        f.write_all(&garbage).unwrap();
        f.sync_all().unwrap();
    }

    // Recovery must succeed and return exactly the rows from the previous
    // committed session.
    {
        let db = open_db(&path);
        let r = db.query("SELECT * FROM t ORDER BY id").unwrap();
        assert_eq!(r.rows.len(), 5);
        for (i, row) in r.rows.iter().enumerate() {
            assert_eq!(row.values[0], Value::Integer(i as i64));
            assert_eq!(row.values[1], Value::Integer(i as i64 * 10));
        }
    }
}

#[test]
fn test_uncommitted_group_in_wal_is_skipped() {
    // Build a WAL that contains a complete PAGE_WRITE record but NO COMMIT
    // record. Recovery must not apply that page write to the data file.
    let dir = tempdir().unwrap();
    let path = dir.path().join("uncommitted.db");

    // Phase A: clean DB with some data.
    {
        let db = open_db(&path);
        db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        db.execute("INSERT INTO t VALUES (1, 100)").unwrap();
        db.close().unwrap();
    }

    // Capture the data file checksum before the manual WAL injection.
    let original_data: Vec<u8> = {
        let mut buf = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
        buf
    };

    // Phase B: directly synthesize an uncommitted WAL record (PAGE_WRITE
    // with a valid CRC but no following COMMIT marker). We poke at page 0
    // (the root page) with all-0xff bytes; if recovery wrongly applied this
    // page write, page 0 of the data file would change.
    {
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .open(wal_path_for(&path))
            .unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();

        let group_id: u64 = 999;
        let page_id: u64 = 0;
        let page_data = vec![0xffu8; PAGE_SIZE];

        // Build the record: [type=1][group_id][len=PAGE_SIZE+8][page_id][data][crc32]
        let mut record = Vec::new();
        record.push(1u8); // PAGE_WRITE
        record.extend_from_slice(&group_id.to_le_bytes());
        record.extend_from_slice(&((PAGE_SIZE as u32) + 8).to_le_bytes());
        record.extend_from_slice(&page_id.to_le_bytes());
        record.extend_from_slice(&page_data);
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&record);
        record.extend_from_slice(&hasher.finalize().to_le_bytes());

        f.write_all(&record).unwrap();
        f.sync_all().unwrap();
    }

    // Open the database. Recovery must NOT apply the uncommitted PAGE_WRITE.
    {
        let db = open_db(&path);
        let r = db.query("SELECT * FROM t").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0].values[1], Value::Integer(100));
        db.close().unwrap();
    }

    // The data file's contents should be unchanged (modulo the legitimate
    // recovery actions, which here are none because the WAL only had an
    // uncommitted record).
    let after_data: Vec<u8> = {
        let mut buf = Vec::new();
        File::open(&path).unwrap().read_to_end(&mut buf).unwrap();
        buf
    };
    assert_eq!(
        original_data, after_data,
        "uncommitted WAL record should not modify the data file"
    );
}

#[test]
fn test_persistence_strong_assertions() {
    // Rewritten version of the older "test_persistence" assertion: every row
    // committed in one process must be visible in the next, with exact
    // values, even when there are enough rows to trigger B-tree splits.
    let dir = tempdir().unwrap();
    let path = dir.path().join("persist.db");

    {
        let db = open_db(&path);
        db.execute("CREATE TABLE big (id INTEGER PRIMARY KEY, v INTEGER)").unwrap();
        for i in 0..500 {
            db.execute(&format!("INSERT INTO big VALUES ({}, {})", i, i * 7)).unwrap();
        }
        db.close().unwrap();
    }

    {
        let db = open_db(&path);
        let r = db.query("SELECT * FROM big ORDER BY id").unwrap();
        assert_eq!(r.rows.len(), 500);
        for (i, row) in r.rows.iter().enumerate() {
            assert_eq!(row.values[0], Value::Integer(i as i64));
            assert_eq!(row.values[1], Value::Integer(i as i64 * 7));
        }

        let count = db.query("SELECT COUNT(*) FROM big").unwrap();
        assert_eq!(count.rows[0].values[0], Value::Integer(500));
    }
}

// ---------------------------------------------------------------------------
// Checkpointing: commits are durable in the WAL before they reach the data
// file. `std::mem::forget` simulates a crash — the database is never flushed
// or checkpointed, exactly as if the process died after its last commit.
// ---------------------------------------------------------------------------

fn crash(db: std::sync::Arc<Database>) {
    std::mem::forget(db);
}

fn wal_len(path: &std::path::Path) -> u64 {
    std::fs::metadata(wal_path_for(path)).map(|m| m.len()).unwrap_or(0)
}

fn ids(db: &Database) -> Vec<i64> {
    db.query("SELECT id FROM u ORDER BY id")
        .unwrap()
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Integer(i) => i,
            ref v => panic!("unexpected {:?}", v),
        })
        .collect()
}

#[test]
fn test_committed_but_not_checkpointed_survives_crash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ckpt_crash.db");
    let data_len_before;
    {
        let db = open_db(&path);
        db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
        for i in 0..50 {
            db.execute(&format!("INSERT INTO u VALUES ({}, 'n{}')", i, i)).unwrap();
        }
        db.execute("DELETE FROM u WHERE id = 7").unwrap();
        db.execute("UPDATE u SET name = 'x' WHERE id = 8").unwrap();
        // Everything is in the WAL, nothing has been checkpointed yet.
        assert!(wal_len(&path) > 0, "commits should be sitting in the WAL");
        data_len_before = std::fs::metadata(&path).unwrap().len();
        crash(db);
    }
    assert!(wal_len(&path) > 0);

    let db = open_db(&path);
    let expected: Vec<i64> = (0..50).filter(|i| *i != 7).collect();
    assert_eq!(ids(&db), expected);
    let r = db.query("SELECT name FROM u WHERE id = 8").unwrap();
    assert_eq!(r.rows[0].values[0], Value::Text("x".to_string()));
    // Recovery applied the WAL to the data file and reset it.
    assert_eq!(wal_len(&path), 0);
    assert!(std::fs::metadata(&path).unwrap().len() >= data_len_before);
}

#[test]
fn test_uncommitted_transaction_lost_on_crash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("txn_crash.db");
    {
        let db = open_db(&path);
        db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO u VALUES (1, 'a')").unwrap();
        db.begin().unwrap();
        for i in 2..200 {
            db.execute(&format!("INSERT INTO u VALUES ({}, 'n')", i)).unwrap();
        }
        crash(db);
    }
    let db = open_db(&path);
    assert_eq!(ids(&db), vec![1]);
}

#[test]
fn test_wal_is_checkpointed_when_large() {
    // Enough committed page images to cross CHECKPOINT_WAL_BYTES several
    // times: the WAL must be checkpointed (bounded), and data must be exact.
    use velocidb::storage::CHECKPOINT_WAL_BYTES;
    let dir = tempdir().unwrap();
    let path = dir.path().join("ckpt_size.db");
    let db = open_db(&path);
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    let per_commit = (PAGE_SIZE + 64) as u64; // at least one page image each
    let commits = (3 * CHECKPOINT_WAL_BYTES / per_commit) as i64;
    let mut max_wal = 0;
    for i in 0..commits {
        db.execute(&format!("INSERT INTO u VALUES ({}, 'row{}')", i, i)).unwrap();
        max_wal = max_wal.max(wal_len(&path));
    }
    assert!(
        max_wal < CHECKPOINT_WAL_BYTES + 64 * PAGE_SIZE as u64,
        "WAL grew to {} bytes; checkpoint did not run",
        max_wal
    );
    crash(db);

    let db = open_db(&path);
    assert_eq!(ids(&db), (0..commits).collect::<Vec<_>>());
}

#[test]
fn test_commits_after_torn_tail_recovery_survive_next_crash() {
    // A torn record left in the WAL must not strand later commits behind it:
    // recovery resets the WAL, so commits made afterwards are recoverable.
    let dir = tempdir().unwrap();
    let path = dir.path().join("torn_then_commit.db");
    {
        let db = open_db(&path);
        db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
        db.execute("INSERT INTO u VALUES (1, 'a')").unwrap();
        // Clean close: checkpointed, WAL empty.
    }
    assert_eq!(wal_len(&path), 0);
    {
        // The WAL now holds nothing but a torn record (a crash mid-append).
        let mut f = OpenOptions::new().append(true).open(wal_path_for(&path)).unwrap();
        f.write_all(&[1u8, 99, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff]).unwrap();
    }
    {
        let db = open_db(&path);
        assert_eq!(ids(&db), vec![1]);
        db.execute("INSERT INTO u VALUES (2, 'b')").unwrap();
        db.execute("INSERT INTO u VALUES (3, 'c')").unwrap();
        crash(db);
    }
    let db = open_db(&path);
    assert_eq!(ids(&db), vec![1, 2, 3]);
}

#[test]
fn test_repeated_crashes_between_commits() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("many_crashes.db");
    {
        let db = open_db(&path);
        db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
        crash(db);
    }
    for round in 0..5i64 {
        let db = open_db(&path);
        assert_eq!(ids(&db), (0..round * 20).collect::<Vec<_>>());
        for i in round * 20..(round + 1) * 20 {
            db.execute(&format!("INSERT INTO u VALUES ({}, 'r{}')", i, round)).unwrap();
        }
        crash(db);
    }
    let db = open_db(&path);
    assert_eq!(ids(&db), (0..100).collect::<Vec<_>>());
}

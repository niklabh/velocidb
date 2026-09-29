//! Explicit-transaction and constraint tests.
//!
//! `BEGIN` … `COMMIT` / `ROLLBACK` runs as one WAL group: ROLLBACK must undo
//! every storage and schema change, a failing statement inside a transaction
//! must undo only itself, and nothing uncommitted may survive a reopen.
//! Also covers `UNIQUE` enforcement on non-primary-key columns.

use std::path::PathBuf;
use std::sync::Arc;

use tempfile::tempdir;
use velocidb::storage::Database;
use velocidb::types::{Value, VelociError};

fn open_db(p: &PathBuf) -> Arc<Database> {
    Database::open(p).unwrap()
}

fn count(db: &Database, table: &str) -> usize {
    db.query(&format!("SELECT * FROM {}", table)).unwrap().rows.len()
}

fn names(db: &Database) -> Vec<String> {
    db.query("SELECT * FROM u ORDER BY id")
        .unwrap()
        .rows
        .into_iter()
        .map(|r| match &r.values[1] {
            Value::Text(s) => s.clone(),
            other => format!("{:?}", other),
        })
        .collect()
}

fn setup(db: &Database) {
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 'Alice')").unwrap();
    db.execute("INSERT INTO u VALUES (2, 'Bob')").unwrap();
}

#[test]
fn test_rollback_undoes_insert_update_delete() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("rb.db");
    let db = open_db(&path);
    setup(&db);

    db.begin().unwrap();
    db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
    db.execute("UPDATE u SET name = 'Alicia' WHERE id = 1").unwrap();
    db.execute("DELETE FROM u WHERE id = 2").unwrap();
    // Reads inside the transaction see its own writes.
    assert_eq!(names(&db), vec!["Alicia", "Carol"]);
    db.rollback().unwrap();

    assert_eq!(names(&db), vec!["Alice", "Bob"]);
    drop(db);

    let db = open_db(&path);
    assert_eq!(names(&db), vec!["Alice", "Bob"]);
}

#[test]
fn test_sql_begin_rollback_statements() {
    let dir = tempdir().unwrap();
    let db = open_db(&dir.path().join("sql.db"));
    setup(&db);

    db.execute("BEGIN").unwrap();
    db.execute("DELETE FROM u").unwrap();
    assert_eq!(count(&db, "u"), 0);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(count(&db, "u"), 2);

    db.execute("BEGIN TRANSACTION").unwrap();
    db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(count(&db, "u"), 3);
}

#[test]
fn test_commit_persists_across_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("commit.db");
    {
        let db = open_db(&path);
        setup(&db);
        db.begin().unwrap();
        db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
        db.execute("UPDATE u SET name = 'Bobby' WHERE id = 2").unwrap();
        db.commit().unwrap();
    }
    let db = open_db(&path);
    assert_eq!(names(&db), vec!["Alice", "Bobby", "Carol"]);
}

#[test]
fn test_uncommitted_transaction_discarded_on_close() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("uncommitted.db");
    {
        let db = open_db(&path);
        setup(&db);
        db.begin().unwrap();
        db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
        db.execute("DELETE FROM u WHERE id = 1").unwrap();
        // Dropped without COMMIT.
    }
    let db = open_db(&path);
    assert_eq!(names(&db), vec!["Alice", "Bob"]);
}

#[test]
fn test_rollback_undoes_schema_changes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ddl.db");
    let db = open_db(&path);
    setup(&db);

    db.begin().unwrap();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 'x')").unwrap();
    db.execute("ALTER TABLE u ADD COLUMN age INTEGER").unwrap();
    db.execute("ALTER TABLE u RENAME TO people").unwrap();
    db.rollback().unwrap();

    let mut tables = db.list_tables();
    tables.sort();
    assert_eq!(tables, vec!["u".to_string()]);
    assert_eq!(
        db.describe_table("u").unwrap(),
        "CREATE TABLE u (id INTEGER PRIMARY KEY, name TEXT)"
    );
    assert_eq!(names(&db), vec!["Alice", "Bob"]);

    // The rolled-back table name is free again and the database is usable.
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)").unwrap();
    db.execute("INSERT INTO t VALUES (7)").unwrap();
    drop(db);

    let db = open_db(&path);
    let mut tables = db.list_tables();
    tables.sort();
    assert_eq!(tables, vec!["t".to_string(), "u".to_string()]);
    assert_eq!(count(&db, "t"), 1);
    assert_eq!(names(&db), vec!["Alice", "Bob"]);
}

#[test]
fn test_rollback_after_many_splits() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("splits.db");
    let db = open_db(&path);
    setup(&db);

    db.begin().unwrap();
    for i in 10..600 {
        db.execute(&format!("INSERT INTO u VALUES ({}, 'name_{}')", i, i)).unwrap();
    }
    assert_eq!(count(&db, "u"), 592);
    db.rollback().unwrap();
    assert_eq!(names(&db), vec!["Alice", "Bob"]);

    // The tree is still healthy: grow it for real and reopen.
    for i in 10..600 {
        db.execute(&format!("INSERT INTO u VALUES ({}, 'n{}')", i, i)).unwrap();
    }
    drop(db);
    let db = open_db(&path);
    assert_eq!(count(&db, "u"), 592);
}

#[test]
fn test_failed_statement_in_transaction_undoes_only_itself() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("savepoint.db");
    {
        let db = open_db(&path);
        setup(&db);

        db.begin().unwrap();
        db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
        // Duplicate primary key: this statement fails ...
        assert!(db.execute("INSERT INTO u VALUES (1, 'Dup')").is_err());
        // ... but the transaction continues.
        db.execute("INSERT INTO u VALUES (4, 'Dave')").unwrap();
        db.commit().unwrap();
    }
    let db = open_db(&path);
    assert_eq!(names(&db), vec!["Alice", "Bob", "Carol", "Dave"]);
}

#[test]
fn test_transaction_state_errors() {
    let dir = tempdir().unwrap();
    let db = open_db(&dir.path().join("state.db"));
    setup(&db);

    assert!(db.commit().is_err());
    assert!(db.rollback().is_err());
    db.begin().unwrap();
    assert!(db.begin().is_err());
    db.commit().unwrap();
    assert!(db.commit().is_err());
}

#[test]
fn test_cdc_publishes_only_committed_changes() {
    let dir = tempdir().unwrap();
    let db = open_db(&dir.path().join("cdc.db"));
    setup(&db);
    db.enable_cdc();

    db.begin().unwrap();
    db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
    assert!(db.changes_since(0).is_empty(), "uncommitted changes must not publish");
    db.rollback().unwrap();
    assert!(db.changes_since(0).is_empty());

    db.begin().unwrap();
    db.execute("INSERT INTO u VALUES (3, 'Carol')").unwrap();
    assert!(db.execute("INSERT INTO u VALUES (3, 'Again')").is_err());
    db.execute("DELETE FROM u WHERE id = 1").unwrap();
    db.commit().unwrap();

    let changes = db.changes_since(0);
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].seq, 1);
    assert_eq!(changes[0].rowid, 3);
    assert_eq!(changes[1].seq, 2);
    assert_eq!(changes[1].rowid, 1);

    // A failed auto-commit statement publishes nothing.
    assert!(db.execute("INSERT INTO u VALUES (2, 'Dup')").is_err());
    assert_eq!(db.changes_since(0).len(), 2);
}

// ---------------------------------------------------------------------------
// UNIQUE
// ---------------------------------------------------------------------------

fn setup_unique(db: &Database) {
    db.execute("CREATE TABLE a (id INTEGER PRIMARY KEY, email TEXT UNIQUE, n INTEGER)")
        .unwrap();
    db.execute("INSERT INTO a VALUES (1, 'x@a', 1)").unwrap();
    db.execute("INSERT INTO a VALUES (2, 'y@a', 2)").unwrap();
}

fn is_constraint(r: velocidb::types::Result<()>) -> bool {
    matches!(r, Err(VelociError::ConstraintViolation(_)))
}

#[test]
fn test_unique_rejects_duplicate_insert() {
    let dir = tempdir().unwrap();
    let db = open_db(&dir.path().join("uq1.db"));
    setup_unique(&db);

    assert!(is_constraint(db.execute("INSERT INTO a VALUES (3, 'x@a', 3)")));
    assert!(is_constraint(
        db.execute("INSERT INTO a (id, email) VALUES (4, 'y@a')")
    ));
    assert_eq!(count(&db, "a"), 2);

    // NULLs never conflict.
    db.execute("INSERT INTO a VALUES (5, NULL, 5)").unwrap();
    db.execute("INSERT INTO a VALUES (6, NULL, 6)").unwrap();
    // Non-unique columns may repeat.
    db.execute("INSERT INTO a VALUES (7, 'z@a', 1)").unwrap();
    assert_eq!(count(&db, "a"), 5);
}

#[test]
fn test_unique_rejects_duplicate_update() {
    let dir = tempdir().unwrap();
    let db = open_db(&dir.path().join("uq2.db"));
    setup_unique(&db);

    assert!(is_constraint(
        db.execute("UPDATE a SET email = 'x@a' WHERE id = 2")
    ));
    // Setting several rows to one value collides among themselves.
    assert!(is_constraint(db.execute("UPDATE a SET email = 'same@a'")));
    // Updating a row to its own value, or to a fresh one, is fine.
    db.execute("UPDATE a SET email = 'x@a' WHERE id = 1").unwrap();
    db.execute("UPDATE a SET email = 'w@a' WHERE id = 2").unwrap();
    // Updating other columns never trips UNIQUE.
    db.execute("UPDATE a SET n = 9").unwrap();

    let r = db.query("SELECT email FROM a ORDER BY id").unwrap();
    assert_eq!(r.rows[0].values[0], Value::Text("x@a".to_string()));
    assert_eq!(r.rows[1].values[0], Value::Text("w@a".to_string()));
}

#[test]
fn test_unique_enforced_after_reopen() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("uq3.db");
    {
        let db = open_db(&path);
        setup_unique(&db);
    }
    let db = open_db(&path);
    assert!(is_constraint(db.execute("INSERT INTO a VALUES (3, 'x@a', 3)")));
    assert_eq!(count(&db, "a"), 2);
}

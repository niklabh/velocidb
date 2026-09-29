//! Secondary index tests: DDL, maintenance on every write path, persistence,
//! rollback and crash recovery, and a differential property test that runs
//! the same workload against an indexed and an unindexed table.

use proptest::prelude::*;
use std::path::Path;
use std::sync::Arc;
use tempfile::tempdir;
use velocidb::storage::Database;
use velocidb::types::{Value, VelociError};

fn open(path: &Path) -> Arc<Database> {
    Database::open(path).unwrap()
}

/// Rows of `sql` as `Debug` strings, for easy comparison.
fn rows(db: &Database, sql: &str) -> Vec<String> {
    db.query(sql)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e))
        .rows
        .iter()
        .map(|r| format!("{:?}", r.values))
        .collect()
}

fn ids(db: &Database, sql: &str) -> Vec<i64> {
    db.query(sql)
        .unwrap_or_else(|e| panic!("{}: {}", sql, e))
        .rows
        .iter()
        .map(|r| match r.values[0] {
            Value::Integer(i) => i,
            ref other => panic!("unexpected {:?}", other),
        })
        .collect()
}

fn people(db: &Database) {
    db.execute("CREATE TABLE p (id INTEGER PRIMARY KEY, city TEXT, age INTEGER, score REAL)")
        .unwrap();
    db.begin().unwrap();
    for i in 0..300 {
        db.execute(&format!(
            "INSERT INTO p VALUES ({}, 'city{}', {}, {})",
            i,
            i % 7,
            i % 50,
            (i % 4) as f64
        ))
        .unwrap();
    }
    db.commit().unwrap();
}

#[test]
fn test_index_lookup_matches_scan() {
    let dir = tempdir().unwrap();
    let db = open(&dir.path().join("t.db"));
    people(&db);
    let queries = [
        "SELECT * FROM p WHERE city = 'city3'",
        "SELECT * FROM p WHERE city = 'city3' AND age > 20",
        "SELECT id FROM p WHERE age = 7 ORDER BY id DESC",
        "SELECT * FROM p WHERE city = 'nowhere'",
        "SELECT COUNT(*) FROM p WHERE age = 10",
        "SELECT * FROM p WHERE score = 2",
        "SELECT * FROM p WHERE score = 2.0 AND city = 'city1'",
        "SELECT * FROM p WHERE age = 3 AND id = 53",
        "SELECT * FROM p WHERE age = NULL",
    ];
    let before: Vec<_> = queries.iter().map(|q| rows(&db, q)).collect();

    db.execute("CREATE INDEX p_city ON p (city)").unwrap();
    db.execute("CREATE INDEX p_age ON p (age)").unwrap();
    db.execute("CREATE INDEX p_score ON p (score)").unwrap();
    for (q, expected) in queries.iter().zip(&before) {
        assert_eq!(&rows(&db, q), expected, "{}", q);
    }
    // Integer probe on a REAL column matches like `=` does.
    assert_eq!(ids(&db, "SELECT id FROM p WHERE score = 3").len(), 75);
}

#[test]
fn test_index_maintained_by_writes() {
    let dir = tempdir().unwrap();
    let db = open(&dir.path().join("t.db"));
    people(&db);
    db.execute("CREATE INDEX p_city ON p (city)").unwrap();

    db.execute("INSERT INTO p VALUES (1000, 'new', 1, 0.5)")
        .unwrap();
    assert_eq!(ids(&db, "SELECT id FROM p WHERE city = 'new'"), vec![1000]);

    // Change the indexed column.
    db.execute("UPDATE p SET city = 'moved' WHERE city = 'city2'")
        .unwrap();
    assert!(ids(&db, "SELECT id FROM p WHERE city = 'city2'").is_empty());
    assert_eq!(ids(&db, "SELECT id FROM p WHERE city = 'moved'").len(), 43);

    // Change the primary key of an indexed row.
    db.execute("UPDATE p SET id = 2000 WHERE id = 1000")
        .unwrap();
    assert_eq!(ids(&db, "SELECT id FROM p WHERE city = 'new'"), vec![2000]);

    // Set to NULL, then delete.
    db.execute("UPDATE p SET city = NULL WHERE id = 2000")
        .unwrap();
    assert!(ids(&db, "SELECT id FROM p WHERE city = 'new'").is_empty());
    db.execute("DELETE FROM p WHERE city = 'moved'").unwrap();
    assert!(ids(&db, "SELECT id FROM p WHERE city = 'moved'").is_empty());
    assert_eq!(ids(&db, "SELECT COUNT(*) FROM p"), vec![258]);
}

#[test]
fn test_index_ddl() {
    let dir = tempdir().unwrap();
    let db = open(&dir.path().join("t.db"));
    people(&db);
    db.execute("CREATE INDEX i ON p (city)").unwrap();
    assert!(matches!(
        db.execute("CREATE INDEX i ON p (age)"),
        Err(VelociError::ConstraintViolation(_))
    ));
    db.execute("CREATE INDEX IF NOT EXISTS i ON p (age)")
        .unwrap();
    assert!(matches!(
        db.execute("CREATE INDEX j ON p (nope)"),
        Err(VelociError::NotFound(_))
    ));
    assert!(db.execute("CREATE INDEX j ON nope (city)").is_err());
    assert!(db.execute("CREATE UNIQUE INDEX j ON p (city)").is_err());
    assert!(db.execute("CREATE INDEX j ON p (city, age)").is_err());

    let schema = db.describe_table("p").unwrap();
    assert!(
        schema.ends_with(";\nCREATE INDEX i ON p (city)"),
        "{}",
        schema
    );

    // The indexed column cannot be dropped; renaming it keeps the index.
    assert!(matches!(
        db.execute("ALTER TABLE p DROP COLUMN city"),
        Err(VelociError::ConstraintViolation(_))
    ));
    db.execute("ALTER TABLE p RENAME COLUMN city TO town")
        .unwrap();
    assert_eq!(ids(&db, "SELECT id FROM p WHERE town = 'city4'").len(), 43);
    db.execute("ALTER TABLE p RENAME TO q").unwrap();
    db.execute("INSERT INTO q VALUES (500, 'x', 1, 1.0)")
        .unwrap();
    assert_eq!(ids(&db, "SELECT id FROM q WHERE town = 'x'"), vec![500]);

    db.execute("DROP INDEX i").unwrap();
    assert!(db.execute("DROP INDEX i").is_err());
    db.execute("DROP INDEX IF EXISTS i").unwrap();
    db.execute("ALTER TABLE q DROP COLUMN town").unwrap();

    // DROP TABLE drops its indexes; the name is free again.
    db.execute("CREATE INDEX k ON q (age)").unwrap();
    db.execute("DROP TABLE q").unwrap();
    people(&db);
    db.execute("CREATE INDEX k ON p (age)").unwrap();
    assert_eq!(ids(&db, "SELECT id FROM p WHERE age = 49").len(), 6);
}

#[test]
fn test_index_survives_reopen_and_crash() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.db");
    {
        let db = open(&path);
        people(&db);
        db.execute("CREATE INDEX p_city ON p (city)").unwrap();
        db.close().unwrap();
    }
    {
        let db = open(&path);
        assert!(db
            .describe_table("p")
            .unwrap()
            .contains("CREATE INDEX p_city"));
        assert_eq!(ids(&db, "SELECT id FROM p WHERE city = 'city0'").len(), 43);
        db.execute("INSERT INTO p VALUES (900, 'city0', 1, 1.0)")
            .unwrap();
        // Crash: no close / checkpoint; the commit is only in the WAL.
        std::mem::forget(db);
    }
    let db = open(&path);
    let found = ids(&db, "SELECT id FROM p WHERE city = 'city0'");
    assert_eq!(found.len(), 44);
    assert_eq!(found.last(), Some(&900));
}

#[test]
fn test_index_rollback() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("t.db");
    let db = open(&path);
    people(&db);
    db.execute("CREATE INDEX p_city ON p (city)").unwrap();

    db.begin().unwrap();
    db.execute("INSERT INTO p VALUES (700, 'ghost', 1, 1.0)")
        .unwrap();
    db.execute("UPDATE p SET city = 'ghost' WHERE id = 1")
        .unwrap();
    db.execute("CREATE INDEX p_age ON p (age)").unwrap();
    assert_eq!(
        ids(&db, "SELECT id FROM p WHERE city = 'ghost'"),
        vec![1, 700]
    );
    db.rollback().unwrap();

    assert!(ids(&db, "SELECT id FROM p WHERE city = 'ghost'").is_empty());
    assert_eq!(ids(&db, "SELECT id FROM p WHERE city = 'city1'")[0], 1);
    assert!(!db.describe_table("p").unwrap().contains("p_age"));
    // The rolled-back name is free.
    db.execute("CREATE INDEX p_age ON p (age)").unwrap();

    // A failing statement inside a transaction undoes its index writes too.
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE)")
        .unwrap();
    db.execute("CREATE INDEX u_email ON u (email)").unwrap();
    db.execute("INSERT INTO u VALUES (1, 'a@x')").unwrap();
    db.begin().unwrap();
    db.execute("INSERT INTO u VALUES (2, 'b@x')").unwrap();
    assert!(db.execute("INSERT INTO u VALUES (3, 'a@x')").is_err());
    assert!(db
        .execute("UPDATE u SET email = 'a@x' WHERE id = 2")
        .is_err());
    db.commit().unwrap();
    assert_eq!(ids(&db, "SELECT id FROM u WHERE email = 'b@x'"), vec![2]);
    assert_eq!(ids(&db, "SELECT id FROM u WHERE email = 'a@x'"), vec![1]);
    drop(db);

    let db = open(&path);
    assert_eq!(ids(&db, "SELECT id FROM u WHERE email = 'b@x'"), vec![2]);
}

#[derive(Debug, Clone)]
enum Op {
    Insert(i64, i64),
    Update(i64, i64),
    UpdatePk(i64, i64),
    Delete(i64),
    DeleteWhere(i64),
}

fn op() -> impl Strategy<Value = Op> {
    // Few distinct values so buckets hold many rows.
    let id = 0i64..120;
    let v = 0i64..8;
    prop_oneof![
        4 => (id.clone(), v.clone()).prop_map(|(i, v)| Op::Insert(i, v)),
        2 => (id.clone(), v.clone()).prop_map(|(i, v)| Op::Update(i, v)),
        1 => (id.clone(), id.clone()).prop_map(|(a, b)| Op::UpdatePk(a, b)),
        2 => id.prop_map(Op::Delete),
        1 => v.prop_map(Op::DeleteWhere),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Tables `a` (indexed) and `b` (not) get the same statements; every
    /// statement must succeed or fail identically and every equality query
    /// must return the same rows.
    #[test]
    fn indexed_and_unindexed_tables_agree(ops in prop::collection::vec(op(), 1..150)) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = open(&path);
        for t in ["a", "b"] {
            db.execute(&format!("CREATE TABLE {} (id INTEGER PRIMARY KEY, v INTEGER, w TEXT)", t))
                .unwrap();
        }
        db.execute("CREATE INDEX a_v ON a (v)").unwrap();
        db.execute("CREATE INDEX a_w ON a (w)").unwrap();

        db.begin().unwrap();
        for op in &ops {
            let sql = |t: &str| match op {
                Op::Insert(i, v) => format!("INSERT INTO {} VALUES ({}, {}, 'w{}')", t, i, v, v % 3),
                Op::Update(i, v) => format!("UPDATE {} SET v = {}, w = 'w{}' WHERE id = {}", t, v, v % 3, i),
                Op::UpdatePk(a, b) => format!("UPDATE {} SET id = {} WHERE id = {}", t, b, a),
                Op::Delete(i) => format!("DELETE FROM {} WHERE id = {}", t, i),
                Op::DeleteWhere(v) => format!("DELETE FROM {} WHERE v = {}", t, v),
            };
            let ra = db.execute(&sql("a")).is_ok();
            let rb = db.execute(&sql("b")).is_ok();
            prop_assert_eq!(ra, rb, "{:?}", op);
        }
        db.commit().unwrap();

        let check = |db: &Database| -> Result<(), TestCaseError> {
            for v in 0..8 {
                let q = |t: &str| format!("SELECT * FROM {} WHERE v = {}", t, v);
                prop_assert_eq!(rows(db, &q("a")), rows(db, &q("b")));
            }
            for w in 0..3 {
                let q = |t: &str| format!("SELECT * FROM {} WHERE w = 'w{}' AND v >= 0", t, w);
                prop_assert_eq!(rows(db, &q("a")), rows(db, &q("b")));
            }
            prop_assert_eq!(rows(db, "SELECT * FROM a"), rows(db, "SELECT * FROM b"));
            Ok(())
        };
        check(&db)?;
        drop(db);
        check(&open(&path))?;
    }
}

/// The schema chain ends at the first chunk shorter than a page's payload
/// (4092 bytes). Sweep table-name lengths so the serialized schema lands on
/// and around exact multiples of it; every size must reopen intact.
#[test]
fn test_schema_sizes_around_page_boundary_reopen() {
    for len in (4030..4070).chain(8130..8150) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("t.db");
        let name = "t".repeat(len);
        {
            let db = open(&path);
            db.execute(&format!(
                "CREATE TABLE {} (id INTEGER PRIMARY KEY, v INTEGER)",
                name
            ))
            .unwrap();
            db.execute(&format!("CREATE INDEX i ON {} (v)", name))
                .unwrap();
            db.execute(&format!("INSERT INTO {} VALUES (1, 7)", name))
                .unwrap();
            db.close().unwrap();
        }
        let db = open(&path);
        assert_eq!(db.list_tables(), vec![name.clone()], "len {}", len);
        assert!(db.describe_table(&name).unwrap().contains("CREATE INDEX i"));
        assert_eq!(
            ids(&db, &format!("SELECT id FROM {} WHERE v = 7", name)),
            vec![1]
        );
    }
}

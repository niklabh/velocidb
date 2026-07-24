//! Integration tests for the Turso-inspired features: vector search,
//! async API, parallel query execution, Change Data Capture, and ALTER TABLE.

use tempfile::NamedTempFile;
use velocidb::vector::DistanceMetric;
use velocidb::{ChangeOp, Database, Value};

// ---------------------------------------------------------------------------
// Vector search
// ---------------------------------------------------------------------------

#[test]
fn test_vector_column_roundtrip() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(3))")
        .unwrap();
    db.execute("INSERT INTO docs VALUES (1, vector32('[1.0, 0.0, 0.0]'))")
        .unwrap();
    db.execute("INSERT INTO docs VALUES (2, vector32('[0.0, 1.0, 0.0]'))")
        .unwrap();

    let result = db.query("SELECT * FROM docs ORDER BY id").unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(
        result.rows[0].values[1],
        Value::Vector(vec![1.0, 0.0, 0.0])
    );
}

#[test]
fn test_vector_dimension_enforced() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(3))")
        .unwrap();
    // Wrong dimension must be rejected.
    let err = db.execute("INSERT INTO docs VALUES (1, vector32('[1.0, 0.0]'))");
    assert!(err.is_err());
    // Non-vector value must be rejected.
    let err = db.execute("INSERT INTO docs VALUES (1, 'not a vector')");
    assert!(err.is_err());
}

#[test]
fn test_vector_knn_order_by_limit() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(2))")
        .unwrap();
    // Points along the x axis at increasing distance from (0, 0).
    for i in 0..20 {
        db.execute(&format!(
            "INSERT INTO docs VALUES ({}, vector32('[{}.0, 0.0]'))",
            i, i
        ))
        .unwrap();
    }

    let result = db
        .query(
            "SELECT id FROM docs \
             ORDER BY vector_distance_l2(embedding, vector32('[4.0, 0.0]')) LIMIT 3",
        )
        .unwrap();
    assert_eq!(result.rows.len(), 3);
    // Nearest to x=4 are ids 4, then 3/5.
    assert_eq!(result.rows[0].values[0], Value::Integer(4));
    let second = result.rows[1].values[0].as_integer().unwrap();
    let third = result.rows[2].values[0].as_integer().unwrap();
    assert!(second == 3 || second == 5);
    assert!(third == 3 || third == 5);
    assert_ne!(second, third);
}

#[test]
fn test_vector_distance_in_projection() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(2))")
        .unwrap();
    db.execute("INSERT INTO docs VALUES (1, vector32('[1.0, 0.0]'))")
        .unwrap();
    db.execute("INSERT INTO docs VALUES (2, vector32('[0.0, 1.0]'))")
        .unwrap();

    let result = db
        .query(
            "SELECT id, vector_distance_cos(embedding, vector32('[1.0, 0.0]')) FROM docs \
             ORDER BY vector_distance_cos(embedding, vector32('[1.0, 0.0]'))",
        )
        .unwrap();

    assert_eq!(result.columns.len(), 2);
    assert_eq!(result.rows.len(), 2);
    // Row 1 is identical to the query vector: distance ~0.
    assert_eq!(result.rows[0].values[0], Value::Integer(1));
    let d0 = result.rows[0].values[1].as_float().unwrap();
    let d1 = result.rows[1].values[1].as_float().unwrap();
    assert!(d0.abs() < 1e-6);
    assert!((d1 - 1.0).abs() < 1e-6); // orthogonal => cosine distance 1
}

#[test]
fn test_vector_search_api() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(2))")
        .unwrap();
    for i in 0..10 {
        db.execute(&format!(
            "INSERT INTO docs VALUES ({}, vector32('[{}.0, 1.0]'))",
            i, i
        ))
        .unwrap();
    }

    let neighbors = db
        .vector_search("docs", "embedding", &[7.0, 1.0], 2, DistanceMetric::Euclidean)
        .unwrap();
    assert_eq!(neighbors.len(), 2);
    assert_eq!(neighbors[0].1.values[0], Value::Integer(7));
    assert!(neighbors[0].0 < 1e-9);

    // Dimension mismatch is an error.
    assert!(db
        .vector_search("docs", "embedding", &[1.0], 2, DistanceMetric::Cosine)
        .is_err());
}

#[test]
fn test_vector_schema_survives_reopen() {
    let tmp = NamedTempFile::new().unwrap();
    {
        let db = Database::open(tmp.path()).unwrap();
        db.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(4))")
            .unwrap();
        db.execute("INSERT INTO docs VALUES (1, vector32('[1, 2, 3, 4]'))")
            .unwrap();
        db.close().unwrap();
    }

    let db = Database::open(tmp.path()).unwrap();
    let schema = db.describe_table("docs").unwrap();
    assert!(schema.contains("F32_BLOB(4)"), "schema was: {}", schema);

    let result = db.query("SELECT * FROM docs").unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(
        result.rows[0].values[1],
        Value::Vector(vec![1.0, 2.0, 3.0, 4.0])
    );
}

// ---------------------------------------------------------------------------
// Parallel query execution (correctness at scale)
// ---------------------------------------------------------------------------

#[test]
fn test_parallel_filter_and_sort_large_set() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE nums (id INTEGER PRIMARY KEY, v INTEGER)")
        .unwrap();
    // Enough rows to cross the parallel threshold (1024).
    for i in 0..2000 {
        db.execute(&format!("INSERT INTO nums VALUES ({}, {})", i, 1999 - i))
            .unwrap();
    }

    let result = db
        .query("SELECT id, v FROM nums WHERE v >= 1000 ORDER BY v")
        .unwrap();
    assert_eq!(result.rows.len(), 1000);
    // Sorted ascending by v.
    assert_eq!(result.rows[0].values[1], Value::Integer(1000));
    assert_eq!(result.rows[999].values[1], Value::Integer(1999));

    let count = db
        .query("SELECT COUNT(*) FROM nums WHERE v < 500")
        .unwrap();
    assert_eq!(count.rows[0].values[0], Value::Integer(500));
}

#[test]
fn test_parallel_knn_large_set() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE pts (id INTEGER PRIMARY KEY, e VECTOR(2))")
        .unwrap();
    for i in 0..1500 {
        db.execute(&format!(
            "INSERT INTO pts VALUES ({}, vector32('[{}.0, 0.0]'))",
            i, i
        ))
        .unwrap();
    }

    let result = db
        .query(
            "SELECT id FROM pts \
             ORDER BY vector_distance_l2(e, vector32('[750.0, 0.0]')) LIMIT 1",
        )
        .unwrap();
    assert_eq!(result.rows[0].values[0], Value::Integer(750));
}

// ---------------------------------------------------------------------------
// Change Data Capture
// ---------------------------------------------------------------------------

#[test]
fn test_cdc_capture_and_poll() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();

    // Nothing recorded while disabled.
    db.execute("INSERT INTO t VALUES (1, 'before')").unwrap();
    assert!(db.changes_since(0).is_empty());

    db.enable_cdc();
    db.execute("INSERT INTO t VALUES (2, 'alice')").unwrap();
    db.execute("UPDATE t SET name = 'bob' WHERE id = 2").unwrap();
    db.execute("DELETE FROM t WHERE id = 1").unwrap();

    let changes = db.changes_since(0);
    assert_eq!(changes.len(), 3);

    assert_eq!(changes[0].op, ChangeOp::Insert);
    assert_eq!(changes[0].rowid, 2);
    assert!(changes[0].before.is_none());
    assert_eq!(
        changes[0].after.as_ref().unwrap().values[1],
        Value::Text("alice".to_string())
    );

    assert_eq!(changes[1].op, ChangeOp::Update);
    assert_eq!(
        changes[1].before.as_ref().unwrap().values[1],
        Value::Text("alice".to_string())
    );
    assert_eq!(
        changes[1].after.as_ref().unwrap().values[1],
        Value::Text("bob".to_string())
    );

    assert_eq!(changes[2].op, ChangeOp::Delete);
    assert_eq!(changes[2].rowid, 1);
    assert!(changes[2].after.is_none());

    // Incremental polling.
    let newer = db.changes_since(changes[1].seq);
    assert_eq!(newer.len(), 1);
    assert_eq!(newer[0].op, ChangeOp::Delete);
    assert_eq!(db.cdc_latest_seq(), changes[2].seq);
}

// ---------------------------------------------------------------------------
// ALTER TABLE
// ---------------------------------------------------------------------------

#[test]
fn test_alter_table_add_and_drop_column() {
    let tmp = NamedTempFile::new().unwrap();
    let db = Database::open(tmp.path()).unwrap();

    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();

    db.execute("ALTER TABLE t ADD COLUMN age INTEGER").unwrap();
    // Old row was padded with NULL.
    let result = db.query("SELECT * FROM t").unwrap();
    assert_eq!(result.columns.len(), 3);
    assert_eq!(result.rows[0].values[2], Value::Null);

    db.execute("INSERT INTO t VALUES (2, 'bob', 42)").unwrap();
    let result = db.query("SELECT age FROM t WHERE id = 2").unwrap();
    assert_eq!(result.rows[0].values[0], Value::Integer(42));

    db.execute("ALTER TABLE t DROP COLUMN name").unwrap();
    let result = db.query("SELECT * FROM t ORDER BY id").unwrap();
    assert_eq!(result.columns.len(), 2);
    assert_eq!(result.rows[1].values[1], Value::Integer(42));

    // Dropping the primary key must fail.
    assert!(db.execute("ALTER TABLE t DROP COLUMN id").is_err());
}

#[test]
fn test_alter_table_renames_survive_reopen() {
    let tmp = NamedTempFile::new().unwrap();
    {
        let db = Database::open(tmp.path()).unwrap();
        db.execute("CREATE TABLE old_name (id INTEGER PRIMARY KEY, a TEXT)")
            .unwrap();
        db.execute("INSERT INTO old_name VALUES (1, 'x')").unwrap();
        db.execute("ALTER TABLE old_name RENAME TO new_name").unwrap();
        db.execute("ALTER TABLE new_name RENAME COLUMN a TO b").unwrap();
        db.close().unwrap();
    }

    let db = Database::open(tmp.path()).unwrap();
    assert!(db.list_tables().contains(&"new_name".to_string()));
    let result = db.query("SELECT b FROM new_name WHERE id = 1").unwrap();
    assert_eq!(result.rows[0].values[0], Value::Text("x".to_string()));
}

// ---------------------------------------------------------------------------
// Async API
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_async_end_to_end() {
    let tmp = NamedTempFile::new().unwrap();
    let db = velocidb::Builder::new_local(tmp.path())
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();

    conn.execute("CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(2))")
        .await
        .unwrap();
    for i in 0..10 {
        conn.execute(&format!(
            "INSERT INTO docs VALUES ({}, vector32('[{}.0, 0.0]'))",
            i, i
        ))
        .await
        .unwrap();
    }

    // Async SQL vector search.
    let result = conn
        .query(
            "SELECT id FROM docs \
             ORDER BY vector_distance_l2(embedding, vector32('[3.0, 0.0]')) LIMIT 1",
        )
        .await
        .unwrap();
    assert_eq!(result.rows[0].values[0], Value::Integer(3));

    // Async KNN API.
    let neighbors = conn
        .vector_search("docs", "embedding", &[6.0, 0.0], 2, DistanceMetric::Euclidean)
        .await
        .unwrap();
    assert_eq!(neighbors[0].1.values[0], Value::Integer(6));
}

#[tokio::test]
async fn test_async_concurrent_writers_and_readers() {
    let tmp = NamedTempFile::new().unwrap();
    let db = velocidb::Builder::new_local(tmp.path())
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER)")
        .await
        .unwrap();

    // Concurrent async writers (engine serializes the writes internally).
    let mut handles = Vec::new();
    for task in 0..4 {
        let c = db.connect().unwrap();
        handles.push(tokio::spawn(async move {
            for i in 0..25 {
                let id = task * 25 + i;
                c.execute(&format!("INSERT INTO t VALUES ({}, {})", id, id))
                    .await
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Concurrent async readers.
    let mut readers = Vec::new();
    for _ in 0..4 {
        let c = db.connect().unwrap();
        readers.push(tokio::spawn(async move {
            c.query("SELECT COUNT(*) FROM t").await.unwrap().rows[0].values[0]
                .as_integer()
                .unwrap()
        }));
    }
    for r in readers {
        assert_eq!(r.await.unwrap(), 100);
    }
}

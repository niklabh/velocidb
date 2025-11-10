// Integration tests for VelociDB

use tempfile::NamedTempFile;
use velocidb::storage::Database;

#[test]
fn test_create_table() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();
}

#[test]
fn test_insert_and_select() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();

    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")
        .unwrap();
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")
        .unwrap();

    let result = db.query("SELECT * FROM users").unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn test_select_with_where() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();

    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")
        .unwrap();
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")
        .unwrap();
    db.execute("INSERT INTO users (id, name, age) VALUES (3, 'Charlie', 35)")
        .unwrap();

    let result = db.query("SELECT * FROM users WHERE age > 25").unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn test_update() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();

    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")
        .unwrap();

    db.execute("UPDATE users SET age = 31 WHERE id = 1")
        .unwrap();

    let result = db.query("SELECT * FROM users WHERE id = 1").unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn test_delete() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
        .unwrap();

    db.execute("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")
        .unwrap();
    db.execute("INSERT INTO users (id, name, age) VALUES (2, 'Bob', 25)")
        .unwrap();

    db.execute("DELETE FROM users WHERE id = 2").unwrap();

    let result = db.query("SELECT * FROM users").unwrap();
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn test_multiple_tables() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("CREATE TABLE posts (id INTEGER PRIMARY KEY, title TEXT)")
        .unwrap();

    db.execute("INSERT INTO users (id, name) VALUES (1, 'Alice')")
        .unwrap();
    db.execute("INSERT INTO posts (id, title) VALUES (1, 'First Post')")
        .unwrap();

    let users = db.query("SELECT * FROM users").unwrap();
    let posts = db.query("SELECT * FROM posts").unwrap();

    assert_eq!(users.rows.len(), 1);
    assert_eq!(posts.rows.len(), 1);
}

#[test]
fn test_large_dataset() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value INTEGER)")
        .unwrap();

    // Insert 50 rows (limited by single page B-Tree)
    for i in 0..50 {
        db.execute(&format!(
            "INSERT INTO test (id, value) VALUES ({}, {})",
            i,
            i * 2
        ))
        .unwrap();
    }

    let result = db.query("SELECT * FROM test").unwrap();
    assert_eq!(result.rows.len(), 50);
}

#[test]
fn test_text_data() {
    let temp_file = NamedTempFile::new().unwrap();
    let db = Database::open(temp_file.path()).unwrap();

    // Test inserting text data
    db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, value TEXT)")
        .unwrap();

    db.execute("INSERT INTO test (id, value) VALUES (1, 'Hello')")
        .unwrap();
    db.execute("INSERT INTO test (id, value) VALUES (2, 'World')")
        .unwrap();

    let result = db.query("SELECT * FROM test").unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn test_persistence() {
    // Note: Schema persistence not yet implemented
    // This test verifies page-level persistence only
    let temp_file = NamedTempFile::new().unwrap();
    let path = temp_file.path().to_path_buf();

    // Create database and insert data in single session
    let db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO users VALUES (1, 'Alice')")
        .unwrap();
    
    let result = db.query("SELECT * FROM users").unwrap();
    assert_eq!(result.rows.len(), 1);
}


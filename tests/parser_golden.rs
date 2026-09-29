//! Golden tests for the SQL parser.
//!
//! Every statement in `CASES` is parsed and rendered to one canonical line;
//! the output must match `tests/golden/parser.golden` exactly. The corpus
//! covers every SQL form used by the test suites and README plus known edge
//! cases, so a parser rewrite shows up as a reviewable diff.
//!
//! Regenerate after an intentional change with:
//!
//! ```text
//! UPDATE_GOLDEN=1 cargo test --test parser_golden
//! ```

use velocidb::parser::{Parser, Statement};

const CASES: &[&str] = &[
    // --- CREATE TABLE ---
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)",
    "create table users (id integer primary key, name text)",
    "CREATE TABLE t (id INTEGER PRIMARY KEY)",
    "CREATE TABLE items (id INTEGER, name TEXT)",
    "CREATE TABLE a (id INTEGER PRIMARY KEY, email TEXT UNIQUE, n INTEGER)",
    "CREATE TABLE a (id INT NOT NULL, r REAL, f FLOAT, d DOUBLE, b BLOB, s STRING, v VARCHAR)",
    "CREATE TABLE a (id INTEGER PRIMARY KEY, name VARCHAR(255) NOT NULL UNIQUE)",
    "CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding F32_BLOB(3))",
    "CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding VECTOR(2))",
    "CREATE TABLE docs (id INTEGER PRIMARY KEY, embedding f32_blob( 4 ))",
    "CREATE TABLE q (\"my col\" TEXT, `other` INTEGER, [third] REAL)",
    "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);",
    "CREATE TABLE t (\n    id INTEGER PRIMARY KEY,\n    name TEXT\n)",
    "CREATE TABLE t (id)",
    "CREATE TABLE t ()",
    "CREATE TABLE t",
    "CREATE TABLE",
    // --- DROP TABLE ---
    "DROP TABLE users",
    "drop table users;",
    "DROP TABLE",
    // --- ALTER TABLE ---
    "ALTER TABLE old_name RENAME TO new_name",
    "ALTER TABLE new_name RENAME COLUMN a TO b",
    "ALTER TABLE t RENAME a TO b",
    "ALTER TABLE t ADD COLUMN age INTEGER",
    "ALTER TABLE t ADD age INTEGER UNIQUE",
    "ALTER TABLE t ADD COLUMN v VECTOR(3);",
    "ALTER TABLE t ADD COLUMN id INTEGER PRIMARY KEY",
    "ALTER TABLE t ADD COLUMN x TEXT NOT NULL",
    "ALTER TABLE t ADD COLUMN x",
    "ALTER TABLE t DROP COLUMN name",
    "ALTER TABLE t DROP name",
    "alter table t rename to u",
    "ALTER TABLE t FROB x",
    "ALTER TABLE",
    // --- INSERT ---
    "INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)",
    "INSERT INTO users VALUES (1, 'Alice', 30)",
    "insert into users values (1, 'Alice', 30);",
    "INSERT INTO t VALUES (7)",
    "INSERT INTO notes (id, content) VALUES (1, 'Hello, World')",
    "INSERT INTO a VALUES (5, NULL, 5)",
    "INSERT INTO a VALUES (5, null, 5)",
    "INSERT INTO t VALUES (1, -42, 3.5, -0.25, 1e3)",
    "INSERT INTO t VALUES (1, X'DEADbeef', x'')",
    "INSERT INTO t VALUES (1, X'ABC')",
    "INSERT INTO t VALUES (1, \"double quoted\")",
    "INSERT INTO t VALUES (1, 'it\\'s')",
    "INSERT INTO t VALUES (1, 'a (paren) [bracket]')",
    "INSERT INTO t VALUES (1, '')",
    "INSERT INTO docs VALUES (1, vector32('[1.0, 0.0, 0.0]'))",
    "INSERT INTO docs VALUES (1, VECTOR('[1, 2]'))",
    "INSERT INTO docs VALUES (1, vector64('[0.5]'))",
    "INSERT INTO docs VALUES (1, [3, 4])",
    "INSERT INTO docs VALUES (1, '[3, 4]')",
    "INSERT INTO docs VALUES (1, 'not a vector')",
    "INSERT INTO t VALUES (1, bareword)",
    "INSERT INTO t VALUES (9223372036854775807)",
    "INSERT INTO t\n  (id, v)\n  VALUES (1, 2)",
    "INSERT INTO t VALUES ()",
    "INSERT INTO t (id, v)",
    "INSERT INTO t",
    "INSERT INTO",
    "INSERT",
    // --- SELECT ---
    "SELECT * FROM users",
    "select * from users",
    "SELECT * FROM users;",
    "SELECT name FROM users",
    "SELECT id, v FROM nums WHERE v >= 1000 ORDER BY v",
    "SELECT COUNT(*) FROM items",
    "SELECT count(*) FROM items WHERE val > 15",
    "SELECT * FROM users WHERE age > 25",
    "SELECT * FROM users WHERE age > 25 AND age < 35",
    "SELECT * FROM users WHERE age > 25 and age < 35",
    "SELECT * FROM p WHERE id = 1 AND id = 2",
    "SELECT * FROM items WHERE val != 20",
    "SELECT * FROM items WHERE val <> 20",
    "SELECT * FROM items WHERE val <= 20",
    "SELECT * FROM items WHERE val>=20",
    "SELECT * FROM users WHERE name LIKE 'Ali%'",
    "SELECT * FROM users WHERE name = 'Alice'",
    "SELECT * FROM users WHERE name = 'A AND B'",
    "SELECT * FROM users WHERE name = 'x ORDER BY y'",
    "SELECT * FROM users WHERE name = 'it\\'s'",
    "SELECT * FROM users WHERE score = 1.5",
    "SELECT * FROM users WHERE score = -3",
    "SELECT * FROM users WHERE name = NULL",
    "SELECT * FROM users WHERE name = Alice",
    "SELECT * FROM t ORDER BY id",
    "SELECT * FROM t ORDER BY id ASC",
    "SELECT * FROM items ORDER BY val DESC",
    "SELECT * FROM items order by val desc",
    "SELECT * FROM t LIMIT 3",
    "SELECT * FROM t limit 3;",
    "SELECT * FROM u WHERE age > 24 ORDER BY age ASC LIMIT 2",
    "SELECT * FROM users WHERE age > 18 ORDER BY name DESC LIMIT 5",
    "SELECT * FROM docs ORDER BY vector_distance_l2(embedding, vector32('[4.0, 0.0]')) LIMIT 3",
    "SELECT id, vector_distance_cos(embedding, vector32('[1.0, 0.0]')) FROM docs ORDER BY vector_distance_cos(embedding, vector32('[1.0, 0.0]'))",
    "SELECT id, title, vector_distance_cos(embedding, vector32('[1, 0, 0]'))\nFROM docs\nORDER BY vector_distance_cos(embedding, vector32('[1, 0, 0]'))\nLIMIT 2",
    "SELECT * FROM docs ORDER BY vector_distance_dot(e, '[1, 2]') DESC LIMIT 1",
    "SELECT *\nFROM users\nWHERE age > 25\nORDER BY name\nLIMIT 10",
    "SELECT * FROM t LIMIT -1",
    "SELECT * FROM t LIMIT x",
    "SELECT * FROM t ORDER BY",
    "SELECT * FROM t WHERE",
    "SELECT * FROM t WHERE id",
    "SELECT * FROM t WHERE id = 1 OR id = 2",
    "SELECT * FROM t WHERE (id = 1)",
    "SELECT * FROM",
    "SELECT",
    // --- UPDATE ---
    "UPDATE users SET age = 31 WHERE name = 'Alice'",
    "UPDATE users SET age = 31 WHERE id = 1",
    "UPDATE users SET age = 32, name = 'Alice Cooper' WHERE id = 1",
    "UPDATE a SET n = 9",
    "update a set n = 9;",
    "UPDATE a SET email = NULL WHERE id = 2 AND n > 1",
    "UPDATE docs SET e = vector32('[1, 2]') WHERE id = 1",
    "UPDATE t SET name = 'a, b' WHERE id = 1",
    "UPDATE t SET name = 'a=b' WHERE id = 1",
    "UPDATE t SET name WHERE id = 1",
    "UPDATE t",
    "UPDATE",
    // --- DELETE ---
    "DELETE FROM users WHERE id = 2",
    "delete from users where id = 2;",
    "DELETE FROM u",
    "DELETE FROM u WHERE name LIKE '%x%' AND id >= 3",
    "DELETE FROM",
    "DELETE",
    // --- Transactions ---
    "BEGIN",
    "begin",
    "BEGIN TRANSACTION",
    "COMMIT",
    "COMMIT TRANSACTION",
    "ROLLBACK",
    "rollback transaction",
    "BEGIN;",
    "COMMIT;",
    "ROLLBACK;",
    // --- Garbage ---
    "",
    "   ",
    "EXPLAIN SELECT * FROM t",
    "SELEC * FROM t",
    // --- Tokenizer-era additions ---
    "INSERT INTO t VALUES (1, 'it''s')",
    "INSERT INTO t VALUES (1, 'C:\\dir')",
    "INSERT INTO t VALUES (1, +5, .5, -1.5e-3)",
    "INSERT INTO t VALUES (1, 99999999999999999999)",
    "INSERT INTO t VALUES (1, 2), (3, 4)",
    "INSERT INTO t VALUES (1, 'unterminated)",
    "INSERT INTO t VALUES (1, vector32('[1, x]'))",
    "INSERT INTO t VALUES (1, [1, 2)",
    "SELECT * FROM t -- trailing comment",
    "SELECT * FROM t; SELECT * FROM u",
    "SELECT * FROM t WHERE id = 1 extra",
    "SELECT 1 FROM t",
    "SELECT a AS b FROM t",
    "SELECT * FROM t ORDER BY *",
    "SELECT * FROM t ORDER BY desc DESC",
    "SELECT * FROM t WHERE name like 'a%'",
    "SELECT * FROM t WHERE name == 'a'",
    "SELECT * FROM t WHERE v = [1, 2]",
    "SELECT * FROM t LIMIT 1.5",
    "SELECT \"my col\", `other` FROM \"my table\" WHERE [third] = 1",
    "SELECT * FROM where",
    "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT)",
    "CREATE TABLE t (id INTEGER DEFAULT 0)",
    "CREATE TABLE t (id INTEGER NULL, p DECIMAL(10, 2))",
    "CREATE TABLE t (id INTEGER,)",
    "UPDATE t SET a = 1, a = 2",
    "END",
    "END TRANSACTION",
    "BEGIN IMMEDIATE",
    // --- Indexes ---
    "CREATE INDEX idx_city ON users (city)",
    "create index if not exists i on t(c);",
    "CREATE INDEX \"my idx\" ON [my table] (`my col`)",
    "DROP INDEX idx_city",
    "drop index if exists i;",
    "CREATE UNIQUE INDEX i ON t (c)",
    "CREATE INDEX i ON t (a, b)",
    "CREATE INDEX i ON t (a DESC)",
    "CREATE INDEX i ON t",
    "CREATE INDEX ON t (a)",
    "CREATE INDEX IF EXISTS i ON t (a)",
    "DROP INDEX",
    "DROP INDEX IF NOT EXISTS i",
];

/// Renders a parse result as one deterministic line. `UPDATE` assignments
/// live in a `HashMap`, so they are sorted before printing. Errors render as
/// `ERR` only: messages may change, whether a statement parses may not.
fn render(result: velocidb::types::Result<Statement>) -> String {
    match result {
        Err(_) => "ERR".to_string(),
        Ok(Statement::Update {
            table,
            assignments,
            where_clause,
        }) => {
            let mut assignments: Vec<_> = assignments.into_iter().collect();
            assignments.sort_by(|a, b| a.0.cmp(&b.0));
            format!(
                "Update {{ table: {:?}, assignments: {:?}, where_clause: {:?} }}",
                table, assignments, where_clause
            )
        }
        Ok(stmt) => format!("{:?}", stmt),
    }
}

#[test]
fn parser_golden() {
    let parser = Parser::new();
    let mut out = String::new();
    for sql in CASES {
        out.push_str(&format!("{:?}\n=> {}\n\n", sql, render(parser.parse(sql))));
    }

    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/parser.golden");
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, &out).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .expect("missing golden file; run with UPDATE_GOLDEN=1 to create it");
    if out != expected {
        let actual_path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/parser.golden.actual");
        std::fs::write(actual_path, &out).unwrap();
        panic!(
            "parser output differs from tests/golden/parser.golden; \
             see `diff tests/golden/parser.golden target/parser.golden.actual` \
             and rerun with UPDATE_GOLDEN=1 if the change is intended"
        );
    }
}

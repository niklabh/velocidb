# VelociDB REPL Usage Guide

## Starting the REPL

```bash
cargo run --release
```

Or after building:

```bash
./target/release/velocidb
```

## Interactive Shell

The REPL provides an interactive SQL shell where you can execute commands in real-time:

```
VelociDB v0.1.0
Type 'help' for help, 'exit' or 'quit' to exit

velocidb>
```

## Meta Commands

Commands starting with `.` or standalone keywords:

| Command | Description |
|---------|-------------|
| `help` or `.help` | Show help information |
| `.tables` | List all tables (planned) |
| `exit`, `quit`, `.exit`, `.quit` | Exit the REPL |

## SQL Commands

### CREATE TABLE

Create a new table with columns and constraints:

```sql
CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)
```

**Supported constraints:**
- `PRIMARY KEY` - Marks column as primary key
- `NOT NULL` - Column cannot be null (planned)
- `UNIQUE` - Column values must be unique (planned)

### INSERT INTO

Insert data into a table:

```sql
-- With column list
INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)

-- Without column list (all columns)
INSERT INTO users VALUES (2, 'Bob', 25)
```

### SELECT

Query data from a table:

```sql
-- Select all columns
SELECT * FROM users

-- Select with WHERE clause
SELECT * FROM users WHERE age > 25

-- Select specific columns (planned)
SELECT name, age FROM users WHERE id = 1
```

**Supported operators:**
- `=` - Equal
- `!=` or `<>` - Not equal
- `>` - Greater than
- `<` - Less than
- `>=` - Greater than or equal
- `<=` - Less than or equal
- `LIKE` - Pattern matching

### UPDATE

Update existing rows:

```sql
UPDATE users SET age = 31 WHERE name = 'Alice'

-- Update multiple columns
UPDATE users SET name = 'Robert', age = 26 WHERE id = 2
```

### DELETE

Delete rows from a table:

```sql
DELETE FROM users WHERE id = 2

-- Delete with condition
DELETE FROM users WHERE age < 18
```

### DROP TABLE

Delete an entire table:

```sql
DROP TABLE users
```

## Data Types

| Type | Description | Example |
|------|-------------|---------|
| `INTEGER` | 64-bit signed integer | `42`, `-1000`, `0` |
| `REAL` | 64-bit floating point | `3.14`, `-0.5`, `1.0` |
| `TEXT` | UTF-8 string | `'Hello'`, `"World"` |
| `BLOB` | Binary data | (not yet in REPL) |
| `NULL` | Null value | `NULL` |

## Example Session

```sql
velocidb> CREATE TABLE employees (id INTEGER PRIMARY KEY, name TEXT, salary INTEGER)
OK

velocidb> INSERT INTO employees VALUES (1, 'Alice', 80000)
OK

velocidb> INSERT INTO employees VALUES (2, 'Bob', 75000)
OK

velocidb> INSERT INTO employees VALUES (3, 'Charlie', 90000)
OK

velocidb> SELECT * FROM employees WHERE salary > 75000
Columns: 3
Rows: 2
id | name | salary
---+----------+----------
1 | Alice | 80000
3 | Charlie | 90000

2 row(s) returned

velocidb> UPDATE employees SET salary = 85000 WHERE name = 'Alice'
OK

velocidb> SELECT * FROM employees
Columns: 3
Rows: 3
id | name | salary
---+---------+----------
1 | Alice | 85000
2 | Bob | 75000
3 | Charlie | 90000

3 row(s) returned

velocidb> DELETE FROM employees WHERE id = 2
OK

velocidb> exit
Goodbye!
```

## Tips

1. **Semicolons are optional** - Commands execute on Enter
2. **Case insensitive** - `SELECT` and `select` are equivalent  
3. **String literals** - Use single `'` or double `"` quotes
4. **Database persistence** - Data is saved to `veloci.db` by default
5. **Exit anytime** - Use `exit`, `quit`, `.exit`, or `.quit`

## Error Handling

If a command fails, you'll see an error message:

```sql
velocidb> INSERT INTO nonexistent VALUES (1)
Error: Table 'nonexistent' not found

velocidb> SELECT * FROM users WHERE invalid_column = 1
Error: Column 'invalid_column' not found
```

The REPL remains active after errors - just correct your command and try again.

## Scripting

You can pipe commands to the REPL:

```bash
echo "CREATE TABLE test (id INTEGER PRIMARY KEY)" | cargo run

# Or from a file
cargo run < commands.sql
```

## Batch Processing

For batch operations, consider using the library API instead:

```rust
use velocidb::Database;

let db = Database::open("mydb.db")?;
db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY)")?;

for i in 0..1000 {
    db.execute(&format!("INSERT INTO test VALUES ({})", i))?;
}
```

## Performance Notes

- Each command is auto-committed (auto-commit mode)
- For bulk inserts, consider using the library API with explicit transactions
- The REPL is optimized for interactive use, not batch processing
- Database file grows automatically as needed

## Limitations

Current REPL limitations:

- No multi-line statement support (yet)
- No command history (yet)
- No tab completion (yet)
- No syntax highlighting (yet)
- One command per line
- Auto-commit mode only

## Future Features

Planned enhancements:

- [ ] Multi-line statement editing
- [ ] Command history with arrow keys
- [ ] Tab completion for tables/columns
- [ ] `.schema` command to show table structures
- [ ] `.tables` implementation
- [ ] Syntax highlighting
- [ ] Transaction control (BEGIN, COMMIT, ROLLBACK)
- [ ] `.import` and `.export` commands
- [ ] Query result formatting options

## Troubleshooting

### REPL doesn't start

```bash
# Rebuild and try again
cargo clean
cargo build --release
cargo run --release
```

### Database locked

The database file can only be opened by one REPL at a time. Close other instances.

### Commands hang

- Check for syntax errors in your SQL
- Ensure primary key is provided for INSERT
- Verify table exists before querying

### Out of disk space

The database file grows as you add data. Ensure sufficient disk space is available.

---

For programmatic access, see the [README.md](README.md) for library usage examples.


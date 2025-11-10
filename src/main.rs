// VelociDB - A high-performance SQLite reimplementation in Rust
// REPL interface for interactive SQL commands

mod storage;
mod btree;
mod parser;
mod executor;
mod transaction;
mod types;

use anyhow::Result;
use std::env;
use std::io::{self, IsTerminal, Write};
use tracing::{info, error, Level};
use tracing_subscriber;

use crate::storage::Database;

fn process_command(db: &Database, input: &str) -> Result<bool> {
    // Handle special commands
    match input.to_lowercase().as_str() {
        "" => return Ok(true), // Continue
        "exit" | "quit" | ".exit" | ".quit" => {
            // Close database before exiting
            if let Err(e) = db.close() {
                error!("Error closing database: {}", e);
            }
            println!("Goodbye!");
            return Ok(false); // Exit
        }
        "help" | ".help" => {
            print_help();
            return Ok(true); // Continue
        }
        ".tables" => {
            let tables = db.list_tables();
            if tables.is_empty() {
                println!("No tables found.");
            } else {
                println!("Tables:");
                for table in tables {
                    println!("  {}", table);
                }
            }
            return Ok(true); // Continue
        }
        _ => {}
    }

    // Execute SQL
    if input.to_uppercase().starts_with("SELECT") {
        // Query
        match db.query(input) {
            Ok(result) => {
                println!("Columns: {}", result.columns.len());
                println!("Rows: {}", result.rows.len());

                // Print column headers
                for (i, col) in result.columns.iter().enumerate() {
                    if i > 0 { print!(" | "); }
                    print!("{}", col.name);
                }
                println!();

                // Print separator
                for (i, col) in result.columns.iter().enumerate() {
                    if i > 0 { print!("-+-"); }
                    print!("{}", "-".repeat(col.name.len().max(10)));
                }
                println!();

                // Print rows
                for row in &result.rows {
                    for (i, value) in row.values.iter().enumerate() {
                        if i > 0 { print!(" | "); }
                        print!("{}", value);
                    }
                    println!();
                }

                println!("\n{} row(s) returned", result.rows.len());
            }
            Err(e) => {
                error!("Query error: {}", e);
                println!("Error: {}", e);
            }
        }
    } else {
        // Execute command
        match db.execute(input) {
            Ok(_) => {
                println!("OK");
            }
            Err(e) => {
                error!("Execution error: {}", e);
                println!("Error: {}", e);
            }
        }
    }

    Ok(true) // Continue
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // Parse command line arguments
    let mut db_filename = "veloci.db".to_string();
    let mut i = 1;

    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            "--version" | "-v" => {
                println!("VelociDB v0.1.0");
                return Ok(());
            }
            "--db" | "-d" => {
                // Next argument should be the database filename
                if i + 1 < args.len() {
                    db_filename = args[i + 1].clone();
                    i += 2;
                } else {
                    eprintln!("Error: --db/-d requires a filename argument");
                    eprintln!("Usage: {} [--help|-h] [--version|-v] [--db|-d <filename>] [database_file]", args[0]);
                    return Ok(());
                }
            }
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {}", arg);
                eprintln!("Usage: {} [--help|-h] [--version|-v] [--db|-d <filename>] [database_file]", args[0]);
                return Ok(());
            }
            _ => {
                // Treat as database filename
                db_filename = args[i].clone();
                i += 1;
            }
        }
    }

    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    info!("VelociDB v0.1.0 - Interactive SQL Shell");
    println!("VelociDB v0.1.0");
    println!("Database: {}", db_filename);
    println!("Type 'help' for help, 'exit' or 'quit' to exit");
    println!();

    // Open or create database
    let db = Database::open(&db_filename)?;
    info!("Database opened: {}", db_filename);

    // Check if stdin is a TTY (for non-interactive environments)
    if !io::stdin().is_terminal() {
        // Non-interactive environment - exit cleanly
        info!("Non-interactive environment detected, exiting cleanly");
        return Ok(());
    }

    // REPL loop
    loop {
        // Print prompt
        print!("velocidb> ");
        io::stdout().flush()?;

        // Read line
        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) => {
                // EOF reached (Ctrl+D on Unix, Ctrl+Z on Windows)
                println!("\nGoodbye!");
                break;
            }
            Ok(_) => {
                let input = input.trim();
                if !process_command(&db, input)? {
                    break; // Exit requested
                }
            }
            Err(e) => {
                error!("Failed to read input: {}", e);
                println!("Error reading input: {}", e);
                break;
            }
        }
    }

    // Ensure database is properly closed before exiting
    if let Err(e) = db.close() {
        error!("Error closing database: {}", e);
        eprintln!("Warning: Failed to properly close database: {}", e);
    }

    Ok(())
}

fn print_help() {
    println!("VelociDB v0.1.0 - SQLite-compatible database");
    println!();
    println!("USAGE:");
    println!("    velocidb [OPTIONS] [DATABASE]");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help       Show this help message");
    println!("    -v, --version    Show version information");
    println!("    -d, --db <FILE>  Specify database file (alternative syntax)");
    println!();
    println!("ARGUMENTS:");
    println!("    <DATABASE>       Database file path (default: veloci.db)");
    println!();
    println!("SQL Commands:");
    println!("  CREATE TABLE <name> (<columns>)  - Create a new table");
    println!("  DROP TABLE <name>                - Drop a table");
    println!("  INSERT INTO <table> VALUES (...)  - Insert data");
    println!("  SELECT * FROM <table>             - Query data");
    println!("  SELECT * FROM <table> WHERE ...   - Query with filter");
    println!("  UPDATE <table> SET ... WHERE ...  - Update data");
    println!("  DELETE FROM <table> WHERE ...     - Delete data");
    println!();
    println!("Meta Commands:");
    println!("  .help    - Show this help");
    println!("  .tables  - List all tables");
    println!("  .exit    - Exit the shell");
    println!();
    println!("Supported Data Types:");
    println!("  INTEGER  - 64-bit signed integer");
    println!("  REAL     - 64-bit floating point");
    println!("  TEXT     - UTF-8 text string");
    println!("  BLOB     - Binary data");
    println!();
    println!("Examples:");
    println!("  CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)");
    println!("  INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)");
    println!("  SELECT * FROM users WHERE age > 25");
    println!("  UPDATE users SET age = 31 WHERE name = 'Alice'");
    println!("  DELETE FROM users WHERE id = 1");
}

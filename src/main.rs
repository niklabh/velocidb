// VelociDB - A high-performance SQLite reimplementation in Rust
// REPL interface for interactive SQL commands

mod storage;
mod btree;
mod parser;
mod executor;
mod transaction;
mod types;

use anyhow::Result;
use std::io::{self, Write};
use tracing::{info, error, Level};
use tracing_subscriber;

use crate::storage::Database;

fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(Level::INFO)
        .init();

    info!("VelociDB v0.1.0 - Interactive SQL Shell");
    println!("VelociDB v0.1.0");
    println!("Type 'help' for help, 'exit' or 'quit' to exit");
    println!();

    // Open or create database
    let db = Database::open("veloci.db")?;
    info!("Database opened: veloci.db");

    // REPL loop
    loop {
        // Print prompt
        print!("velocidb> ");
        io::stdout().flush()?;

        // Read line
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        
        let input = input.trim();

        // Handle special commands
        match input.to_lowercase().as_str() {
            "" => continue,
            "exit" | "quit" | ".exit" | ".quit" => {
                println!("Goodbye!");
                break;
            }
            "help" | ".help" => {
                print_help();
                continue;
            }
            ".tables" => {
                println!("Tables:");
                println!("  (schema inspection not yet implemented)");
                continue;
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
    }

    Ok(())
}

fn print_help() {
    println!("VelociDB Commands:");
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

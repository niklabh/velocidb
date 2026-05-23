//! VelociDB — A high-performance embedded database engine written in Rust.
//!
//! This binary provides an interactive REPL for executing SQL commands.
//! Type `.help` for a list of supported commands or `.exit` to quit.

mod storage;
mod btree;
mod parser;
mod executor;
mod transaction;
mod types;
mod mvcc;
mod wal;

use anyhow::Result;
use rustyline::config::Builder as RustylineBuilder;
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Editor, EventHandler, KeyCode, KeyEvent, Modifiers};
use std::env;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use tracing::{error, info, Level};
use tracing_subscriber;

use crate::storage::Database;

/// Returns true to continue the REPL, false to exit.
fn process_command(db: &Database, input: &str) -> Result<bool> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(true);
    }

    // Meta commands always start with `.` and are single-line.
    if let Some(rest) = trimmed.strip_prefix('.') {
        return handle_meta_command(db, rest.trim());
    }

    // Plain word commands carried over from the previous REPL for compatibility.
    match trimmed.to_lowercase().as_str() {
        "exit" | "quit" => return handle_meta_command(db, "exit"),
        "help" => return handle_meta_command(db, "help"),
        "begin" | "begin transaction" => {
            match db.begin() {
                Ok(()) => println!("Transaction started."),
                Err(e) => println!("Error: {}", e),
            }
            return Ok(true);
        }
        "commit" | "commit transaction" => {
            match db.commit() {
                Ok(()) => println!("Transaction committed."),
                Err(e) => println!("Error: {}", e),
            }
            return Ok(true);
        }
        "rollback" | "rollback transaction" => {
            match db.rollback() {
                Ok(()) => println!("Transaction rolled back."),
                Err(e) => println!("Error: {}", e),
            }
            return Ok(true);
        }
        _ => {}
    }

    // Execute one or more SQL statements separated by `;`.
    for statement in split_statements(trimmed) {
        run_sql(db, &statement);
    }

    Ok(true)
}

fn handle_meta_command(db: &Database, cmd: &str) -> Result<bool> {
    let mut parts = cmd.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or("").to_lowercase();
    let arg = parts.next().map(str::trim).unwrap_or("");

    match verb.as_str() {
        "exit" | "quit" => {
            if let Err(e) = db.close() {
                error!("Error closing database: {}", e);
            }
            println!("Goodbye!");
            Ok(false)
        }
        "help" => {
            print_help();
            Ok(true)
        }
        "tables" => {
            let tables = db.list_tables();
            if tables.is_empty() {
                println!("No tables.");
            } else {
                println!("Tables:");
                for table in tables {
                    println!("  {}", table);
                }
            }
            Ok(true)
        }
        "schema" => {
            if arg.is_empty() {
                let tables = db.list_tables();
                if tables.is_empty() {
                    println!("No tables.");
                }
                for t in tables {
                    match db.describe_table(&t) {
                        Ok(sql) => println!("{};", sql),
                        Err(e) => println!("Error describing '{}': {}", t, e),
                    }
                }
            } else {
                match db.describe_table(arg) {
                    Ok(sql) => println!("{};", sql),
                    Err(e) => println!("Error: {}", e),
                }
            }
            Ok(true)
        }
        other => {
            println!("Unknown meta command: .{}", other);
            Ok(true)
        }
    }
}

/// Splits a buffer on top-level `;` boundaries, respecting single/double quoted
/// string literals. Empty statements are skipped.
fn split_statements(buffer: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut quote = '\'';

    for ch in buffer.chars() {
        if in_string {
            current.push(ch);
            if ch == quote {
                in_string = false;
            }
        } else if ch == '\'' || ch == '"' {
            in_string = true;
            quote = ch;
            current.push(ch);
        } else if ch == ';' {
            let s = current.trim().to_string();
            if !s.is_empty() {
                out.push(s);
            }
            current.clear();
        } else {
            current.push(ch);
        }
    }

    let s = current.trim().to_string();
    if !s.is_empty() {
        out.push(s);
    }
    out
}

/// Returns true if the buffered text contains at least one complete statement
/// (a `;` outside of a quoted string).
fn has_complete_statement(buffer: &str) -> bool {
    let mut in_string = false;
    let mut quote = '\'';
    for ch in buffer.chars() {
        if in_string {
            if ch == quote {
                in_string = false;
            }
        } else if ch == '\'' || ch == '"' {
            in_string = true;
            quote = ch;
        } else if ch == ';' {
            return true;
        }
    }
    false
}

fn run_sql(db: &Database, sql: &str) {
    if sql.to_uppercase().starts_with("SELECT") {
        match db.query(sql) {
            Ok(result) => print_query_result(&result),
            Err(e) => {
                error!("Query error: {}", e);
                println!("Error: {}", e);
            }
        }
    } else {
        match db.execute(sql) {
            Ok(_) => println!("OK"),
            Err(e) => {
                error!("Execution error: {}", e);
                println!("Error: {}", e);
            }
        }
    }
}

fn print_query_result(result: &crate::types::QueryResult) {
    // Header.
    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            print!(" | ");
        }
        print!("{}", col.name);
    }
    println!();
    for (i, col) in result.columns.iter().enumerate() {
        if i > 0 {
            print!("-+-");
        }
        print!("{}", "-".repeat(col.name.len().max(10)));
    }
    println!();
    for row in &result.rows {
        for (i, value) in row.values.iter().enumerate() {
            if i > 0 {
                print!(" | ");
            }
            print!("{}", value);
        }
        println!();
    }
    println!("\n{} row(s) returned", result.rows.len());
}

fn history_path() -> Option<PathBuf> {
    if let Ok(p) = env::var("VELOCIDB_HISTORY") {
        return Some(PathBuf::from(p));
    }
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".velocidb_history"))
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

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
                if i + 1 < args.len() {
                    db_filename = args[i + 1].clone();
                    i += 2;
                } else {
                    eprintln!("Error: --db/-d requires a filename argument");
                    return Ok(());
                }
            }
            arg if arg.starts_with('-') => {
                eprintln!("Unknown option: {}", arg);
                return Ok(());
            }
            _ => {
                db_filename = args[i].clone();
                i += 1;
            }
        }
    }

    tracing_subscriber::fmt()
        .with_max_level(Level::WARN)
        .init();

    info!("VelociDB v0.1.0 - Interactive SQL Shell");
    println!("VelociDB v0.1.0");
    println!("Database: {}", db_filename);
    println!("Type '.help' for help, '.exit' to quit. Statements end with ';'.");
    println!();

    let db = Database::open(&db_filename)?;

    if !io::stdin().is_terminal() {
        // Non-interactive: read stdin line by line so that `.meta` commands
        // and SQL statements terminated by `;` are both handled.
        let mut buf = String::new();
        use std::io::Read as _;
        if io::stdin().read_to_string(&mut buf).is_ok() {
            let mut sql_buf = String::new();
            for line in buf.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if sql_buf.is_empty() && trimmed.starts_with('.') {
                    if let Ok(false) = process_command(&db, trimmed) {
                        break;
                    }
                    continue;
                }
                if !sql_buf.is_empty() {
                    sql_buf.push('\n');
                }
                sql_buf.push_str(line);
                if has_complete_statement(&sql_buf) {
                    let to_run = std::mem::take(&mut sql_buf);
                    if let Ok(false) = process_command(&db, to_run.trim()) {
                        break;
                    }
                }
            }
            if !sql_buf.trim().is_empty() {
                let _ = process_command(&db, sql_buf.trim());
            }
        }
        if let Err(e) = db.close() {
            eprintln!("Warning: failed to close database: {}", e);
        }
        return Ok(());
    }

    // Interactive REPL with rustyline.
    let config = RustylineBuilder::new()
        .auto_add_history(true)
        .history_ignore_dups(true)?
        .build();
    let mut rl: Editor<(), FileHistory> = Editor::with_config(config)?;
    rl.bind_sequence(
        KeyEvent(KeyCode::Char('c'), Modifiers::CTRL),
        EventHandler::Simple(rustyline::Cmd::Interrupt),
    );

    let hist_path = history_path();
    if let Some(ref p) = hist_path {
        let _ = rl.load_history(p);
    }

    let mut buffer = String::new();
    loop {
        let prompt = if buffer.is_empty() { "velocidb> " } else { "      ...> " };
        match rl.readline(prompt) {
            Ok(line) => {
                let line_trimmed = line.trim();

                // Meta commands (`.help`, `.tables`, ...) execute immediately
                // even when there is partial buffered SQL.
                if buffer.is_empty() && line_trimmed.starts_with('.') {
                    match process_command(&db, line_trimmed) {
                        Ok(true) => continue,
                        Ok(false) => break,
                        Err(e) => {
                            println!("Error: {}", e);
                            continue;
                        }
                    }
                }

                // Top-level non-SQL keywords kept for compatibility (single line).
                if buffer.is_empty() {
                    let lower = line_trimmed.to_lowercase();
                    if matches!(
                        lower.as_str(),
                        "exit" | "quit" | "help" | "begin" | "commit" | "rollback"
                            | "begin transaction" | "commit transaction" | "rollback transaction"
                    ) {
                        match process_command(&db, line_trimmed) {
                            Ok(true) => continue,
                            Ok(false) => break,
                            Err(e) => {
                                println!("Error: {}", e);
                                continue;
                            }
                        }
                    }
                }

                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);

                if has_complete_statement(&buffer) {
                    let to_run = std::mem::take(&mut buffer);
                    if let Err(e) = process_command(&db, to_run.trim()) {
                        println!("Error: {}", e);
                    }
                }
            }
            Err(ReadlineError::Interrupted) => {
                if !buffer.is_empty() {
                    println!("(input cleared)");
                    buffer.clear();
                } else {
                    println!("(Ctrl-D or .exit to quit)");
                }
            }
            Err(ReadlineError::Eof) => {
                println!("\nGoodbye!");
                break;
            }
            Err(e) => {
                error!("Failed to read input: {}", e);
                break;
            }
        }
    }

    if let Some(ref p) = hist_path {
        let _ = rl.save_history(p);
    }

    if let Err(e) = db.close() {
        error!("Error closing database: {}", e);
        eprintln!("Warning: Failed to properly close database: {}", e);
    }

    Ok(())
}

fn print_help() {
    println!("VelociDB v0.1.0");
    println!();
    println!("USAGE:");
    println!("    velocidb [OPTIONS] [DATABASE]");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help       Show this help message");
    println!("    -v, --version    Show version information");
    println!("    -d, --db <FILE>  Specify database file (alternative syntax)");
    println!();
    println!("Meta commands (single-line):");
    println!("    .help              Show this help");
    println!("    .tables            List tables");
    println!("    .schema [name]     Show CREATE TABLE for one or all tables");
    println!("    .exit              Exit the shell");
    println!();
    println!("SQL (terminated by ';'):");
    println!("    CREATE TABLE <name> (...)");
    println!("    DROP TABLE <name>");
    println!("    INSERT INTO <table> [(cols)] VALUES (...)");
    println!("    SELECT [* | cols | COUNT(*)] FROM <table>");
    println!("        [WHERE expr [AND expr ...]] [ORDER BY col [ASC|DESC]] [LIMIT n]");
    println!("    UPDATE <table> SET col = val [, ...] [WHERE ...]");
    println!("    DELETE FROM <table> [WHERE ...]");
    println!("    BEGIN | COMMIT | ROLLBACK");
}

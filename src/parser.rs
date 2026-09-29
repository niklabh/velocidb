//! SQL parser supporting DDL (CREATE TABLE, DROP TABLE, ALTER TABLE), DML
//! (INSERT, UPDATE, DELETE), and DQL (SELECT with WHERE, COUNT(\*)
//! aggregation, and vector distance expressions).
//!
//! Parses SQL text into an AST of [`Statement`] variants consumed by the executor.

use crate::types::{Column, DataType, Result, Value, VelociError};
use regex::Regex;
use std::collections::HashMap;

/// A `&'static Regex` compiled once on first use. Parsing runs on every
/// `execute` / `query`, so recompiling patterns per call dominated the cost
/// of small statements.
macro_rules! regex {
    ($pattern:literal) => {{
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new($pattern).expect("static regex is valid"))
    }};
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<Column>,
    },
    DropTable {
        name: String,
    },
    AlterTable {
        table: String,
        action: AlterAction,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        values: Vec<Value>,
    },
    Select {
        table: String,
        columns: Vec<String>,
        where_clause: Option<WhereClause>,
        order_by: Option<OrderBy>,
        limit: Option<u64>,
    },
    Update {
        table: String,
        assignments: HashMap<String, Value>,
        where_clause: Option<WhereClause>,
    },
    Delete {
        table: String,
        where_clause: Option<WhereClause>,
    },
    BeginTransaction,
    CommitTransaction,
    RollbackTransaction,
}

/// Actions supported by `ALTER TABLE` (Turso-inspired improved schema management).
#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    RenameTable { new_name: String },
    RenameColumn { old_name: String, new_name: String },
    AddColumn { column: Column },
    DropColumn { name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    pub conditions: Vec<Condition>,
}

/// ORDER BY clause: what to sort by and whether ascending.
///
/// `column` is either a plain column name or a vector distance expression
/// such as `vector_distance_cos(embedding, vector32('[1,2,3]'))` — the
/// executor detects the latter and performs a (parallel) KNN sort.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub column: String,
    pub ascending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    pub column: String,
    pub operator: Operator,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Operator {
    Equal,
    NotEqual,
    GreaterThan,
    LessThan,
    GreaterThanOrEqual,
    LessThanOrEqual,
    Like,
}

impl Operator {
    #[allow(clippy::should_implement_trait)] // returns our Result, not FromStr::Err
    pub fn from_str(s: &str) -> Result<Self> {
        match s {
            "=" => Ok(Operator::Equal),
            "!=" | "<>" => Ok(Operator::NotEqual),
            ">" => Ok(Operator::GreaterThan),
            "<" => Ok(Operator::LessThan),
            ">=" => Ok(Operator::GreaterThanOrEqual),
            "<=" => Ok(Operator::LessThanOrEqual),
            "LIKE" => Ok(Operator::Like),
            _ => Err(VelociError::ParseError(format!("Unknown operator: {}", s))),
        }
    }

    pub fn evaluate(&self, left: &Value, right: &Value) -> Result<bool> {
        // SQL NULL semantics: any comparison involving NULL returns false
        if matches!(left, Value::Null) || matches!(right, Value::Null) {
            return Ok(false);
        }

        match (self, left, right) {
            // Integer comparisons
            (Operator::Equal, Value::Integer(a), Value::Integer(b)) => Ok(a == b),
            (Operator::NotEqual, Value::Integer(a), Value::Integer(b)) => Ok(a != b),
            (Operator::GreaterThan, Value::Integer(a), Value::Integer(b)) => Ok(a > b),
            (Operator::LessThan, Value::Integer(a), Value::Integer(b)) => Ok(a < b),
            (Operator::GreaterThanOrEqual, Value::Integer(a), Value::Integer(b)) => Ok(a >= b),
            (Operator::LessThanOrEqual, Value::Integer(a), Value::Integer(b)) => Ok(a <= b),

            // Float/Real comparisons (at least one operand is Float/Real)
            (op, left, right) if matches!((left, right),
                (Value::Float(_) | Value::Real(_) | Value::Integer(_),
                 Value::Float(_) | Value::Real(_) | Value::Integer(_))
            ) && (!matches!(left, Value::Integer(_)) || !matches!(right, Value::Integer(_))) => {
                let a = left.as_float().map_err(|_| VelociError::TypeMismatch {
                    expected: "numeric".to_string(), actual: format!("{:?}", left),
                })?;
                let b = right.as_float().map_err(|_| VelociError::TypeMismatch {
                    expected: "numeric".to_string(), actual: format!("{:?}", right),
                })?;
                match op {
                    Operator::Equal => Ok(a == b),
                    Operator::NotEqual => Ok(a != b),
                    Operator::GreaterThan => Ok(a > b),
                    Operator::LessThan => Ok(a < b),
                    Operator::GreaterThanOrEqual => Ok(a >= b),
                    Operator::LessThanOrEqual => Ok(a <= b),
                    Operator::Like => Err(VelociError::ParseError("LIKE not supported for numeric types".to_string())),
                }
            }

            // Text comparisons
            (Operator::Equal, Value::Text(a), Value::Text(b)) => Ok(a == b),
            (Operator::NotEqual, Value::Text(a), Value::Text(b)) => Ok(a != b),
            (Operator::GreaterThan, Value::Text(a), Value::Text(b)) => Ok(a > b),
            (Operator::LessThan, Value::Text(a), Value::Text(b)) => Ok(a < b),
            (Operator::GreaterThanOrEqual, Value::Text(a), Value::Text(b)) => Ok(a >= b),
            (Operator::LessThanOrEqual, Value::Text(a), Value::Text(b)) => Ok(a <= b),
            (Operator::Like, Value::Text(a), Value::Text(pattern)) => {
                // Convert SQL LIKE pattern to regex character by character
                let mut regex_pattern = String::from("^");
                for ch in pattern.chars() {
                    match ch {
                        '%' => regex_pattern.push_str(".*"),
                        '_' => regex_pattern.push('.'),
                        // Escape regex metacharacters
                        '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']'
                        | '{' | '}' | '|' | '^' | '$' | '\\' => {
                            regex_pattern.push('\\');
                            regex_pattern.push(ch);
                        }
                        _ => regex_pattern.push(ch),
                    }
                }
                regex_pattern.push('$');
                let regex = Regex::new(&regex_pattern)
                    .map_err(|e| VelociError::ParseError(format!("Invalid LIKE pattern: {}", e)))?;
                Ok(regex.is_match(a))
            }

            _ => Err(VelociError::TypeMismatch {
                expected: format!("{:?}", right),
                actual: format!("{:?}", left),
            }),
        }
    }
}

/// Finds the byte offset of a top-level `ORDER BY` keyword (outside quoted
/// strings), or `None` if absent.
fn find_order_by(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_string = false;
    let mut quote = b'\'';
    let mut i = 0;

    while i < bytes.len() {
        let b = bytes[i];
        if in_string {
            if b == quote {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            in_string = true;
            quote = b;
            i += 1;
            continue;
        }
        // Try to match "ORDER" followed by whitespace and "BY", both bounded
        // by whitespace (or start of string on the left).
        if (b == b'O' || b == b'o')
            && (i == 0 || bytes[i - 1].is_ascii_whitespace())
            && s[i..].len() >= 8
        {
            let rest = &s[i..];
            let upper: String = rest.chars().take(9).collect::<String>().to_uppercase();
            if upper.starts_with("ORDER ") || upper.starts_with("ORDER\t") || upper.starts_with("ORDER\n") {
                // Confirm "BY" follows the whitespace run.
                let after_order = rest[5..].trim_start();
                let upper_after: String = after_order.chars().take(3).collect::<String>().to_uppercase();
                if upper_after.starts_with("BY")
                    && after_order[2..]
                        .chars()
                        .next()
                        .map(|c| c.is_whitespace())
                        .unwrap_or(false)
                {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Splits `s` on commas that are outside quoted strings, parentheses and
/// square brackets.
fn split_top_level_commas(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    let mut quote = '\'';
    let mut depth: usize = 0;

    for ch in s.chars() {
        if in_string {
            current.push(ch);
            if ch == quote {
                in_string = false;
            }
        } else {
            match ch {
                '\'' | '"' => {
                    in_string = true;
                    quote = ch;
                    current.push(ch);
                }
                '(' | '[' => {
                    depth += 1;
                    current.push(ch);
                }
                ')' | ']' => {
                    depth = depth.saturating_sub(1);
                    current.push(ch);
                }
                ',' if depth == 0 => {
                    let part = current.trim().to_string();
                    if !part.is_empty() {
                        parts.push(part);
                    }
                    current.clear();
                }
                _ => current.push(ch),
            }
        }
    }

    let part = current.trim().to_string();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

pub struct Parser {
    // Parser state can be added here if needed
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {}
    }

    pub fn parse(&self, sql: &str) -> Result<Statement> {
        let sql = sql.trim();
        let upper = sql.to_uppercase();

        if upper.starts_with("CREATE TABLE") {
            self.parse_create_table(sql)
        } else if upper.starts_with("DROP TABLE") {
            self.parse_drop_table(sql)
        } else if upper.starts_with("ALTER TABLE") {
            self.parse_alter_table(sql)
        } else if upper.starts_with("INSERT INTO") {
            self.parse_insert(sql)
        } else if upper.starts_with("SELECT") {
            self.parse_select(sql)
        } else if upper.starts_with("UPDATE") {
            self.parse_update(sql)
        } else if upper.starts_with("DELETE FROM") {
            self.parse_delete(sql)
        } else if upper == "BEGIN" || upper == "BEGIN TRANSACTION" {
            Ok(Statement::BeginTransaction)
        } else if upper == "COMMIT" || upper == "COMMIT TRANSACTION" {
            Ok(Statement::CommitTransaction)
        } else if upper == "ROLLBACK" || upper == "ROLLBACK TRANSACTION" {
            Ok(Statement::RollbackTransaction)
        } else {
            Err(VelociError::ParseError(format!(
                "Unsupported statement: {}",
                sql
            )))
        }
    }

    fn parse_create_table(&self, sql: &str) -> Result<Statement> {
        // CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)
        let re = regex!(r"(?i)CREATE\s+TABLE\s+(\w+)\s*\((.+)\)");

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid CREATE TABLE syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();
        let columns_str = captures.get(2).unwrap().as_str();

        let mut columns = Vec::new();
        for col_def in columns_str.split(',') {
            let col_def = col_def.trim();
            if col_def.is_empty() {
                continue;
            }

            // Parse column name (handle quoted identifiers)
            let (col_name, remainder) = self.parse_identifier(col_def)?;

            // Parse data type and constraints
            let remainder = remainder.trim();
            if remainder.is_empty() {
                return Err(VelociError::ParseError(format!(
                    "Missing data type for column '{}'",
                    col_name
                )));
            }

            // Split remainder into parts, handling quoted strings
            let parts = self.split_sql_parts(remainder);
            if parts.is_empty() {
                return Err(VelociError::ParseError(format!(
                    "Invalid column definition: {}",
                    col_def
                )));
            }

            let data_type = DataType::from_str(&parts[0]);
            let mut primary_key = false;
            let mut not_null = false;
            let mut unique = false;

            // Check for constraints
            let upper_parts: Vec<String> = parts.iter().map(|s| s.to_uppercase()).collect();
            if upper_parts.contains(&"PRIMARY".to_string())
                && upper_parts.contains(&"KEY".to_string())
            {
                primary_key = true;
                not_null = true;
            }
            if upper_parts.contains(&"NOT".to_string())
                && upper_parts.contains(&"NULL".to_string())
            {
                not_null = true;
            }
            if upper_parts.contains(&"UNIQUE".to_string()) {
                unique = true;
            }

            columns.push(Column {
                name: col_name,
                data_type,
                primary_key,
                not_null,
                unique,
            });
        }

        Ok(Statement::CreateTable {
            name: table_name,
            columns,
        })
    }

    fn parse_drop_table(&self, sql: &str) -> Result<Statement> {
        // DROP TABLE users
        let re = regex!(r"(?i)DROP\s+TABLE\s+(\w+)");

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid DROP TABLE syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();

        Ok(Statement::DropTable { name: table_name })
    }

    fn parse_alter_table(&self, sql: &str) -> Result<Statement> {
        // ALTER TABLE t RENAME TO new_name
        // ALTER TABLE t RENAME COLUMN old TO new
        // ALTER TABLE t ADD [COLUMN] name TYPE [constraints]
        // ALTER TABLE t DROP [COLUMN] name
        let sql = sql.trim().trim_end_matches(';').trim();

        let rename_table_re = regex!(r"(?i)^ALTER\s+TABLE\s+(\w+)\s+RENAME\s+TO\s+(\w+)$");
        if let Some(caps) = rename_table_re.captures(sql) {
            return Ok(Statement::AlterTable {
                table: caps.get(1).unwrap().as_str().to_string(),
                action: AlterAction::RenameTable {
                    new_name: caps.get(2).unwrap().as_str().to_string(),
                },
            });
        }

        let rename_col_re =
            regex!(r"(?i)^ALTER\s+TABLE\s+(\w+)\s+RENAME\s+(?:COLUMN\s+)?(\w+)\s+TO\s+(\w+)$");
        if let Some(caps) = rename_col_re.captures(sql) {
            return Ok(Statement::AlterTable {
                table: caps.get(1).unwrap().as_str().to_string(),
                action: AlterAction::RenameColumn {
                    old_name: caps.get(2).unwrap().as_str().to_string(),
                    new_name: caps.get(3).unwrap().as_str().to_string(),
                },
            });
        }

        let add_col_re = regex!(r"(?i)^ALTER\s+TABLE\s+(\w+)\s+ADD\s+(?:COLUMN\s+)?(.+)$");
        if let Some(caps) = add_col_re.captures(sql) {
            let table = caps.get(1).unwrap().as_str().to_string();
            let col_def = caps.get(2).unwrap().as_str().trim();

            let (col_name, remainder) = self.parse_identifier(col_def)?;
            let parts = self.split_sql_parts(remainder.trim());
            if parts.is_empty() {
                return Err(VelociError::ParseError(format!(
                    "Missing data type in ADD COLUMN: {}",
                    col_def
                )));
            }
            let data_type = DataType::from_str(&parts[0]);
            let upper_parts: Vec<String> = parts.iter().map(|s| s.to_uppercase()).collect();
            if upper_parts.contains(&"PRIMARY".to_string()) {
                return Err(VelociError::ParseError(
                    "Cannot add a PRIMARY KEY column with ALTER TABLE".to_string(),
                ));
            }
            let not_null = upper_parts.contains(&"NOT".to_string())
                && upper_parts.contains(&"NULL".to_string());
            if not_null {
                return Err(VelociError::ParseError(
                    "Cannot add a NOT NULL column without a default value".to_string(),
                ));
            }
            let unique = upper_parts.contains(&"UNIQUE".to_string());

            return Ok(Statement::AlterTable {
                table,
                action: AlterAction::AddColumn {
                    column: Column {
                        name: col_name,
                        data_type,
                        primary_key: false,
                        not_null: false,
                        unique,
                    },
                },
            });
        }

        let drop_col_re = regex!(r"(?i)^ALTER\s+TABLE\s+(\w+)\s+DROP\s+(?:COLUMN\s+)?(\w+)$");
        if let Some(caps) = drop_col_re.captures(sql) {
            return Ok(Statement::AlterTable {
                table: caps.get(1).unwrap().as_str().to_string(),
                action: AlterAction::DropColumn {
                    name: caps.get(2).unwrap().as_str().to_string(),
                },
            });
        }

        Err(VelociError::ParseError(format!(
            "Invalid ALTER TABLE syntax: {}",
            sql
        )))
    }

    fn parse_insert(&self, sql: &str) -> Result<Statement> {
        // INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)
        // INSERT INTO users VALUES (1, 'Alice', 30)
        
        // The VALUES capture is greedy up to the final ')' so nested function
        // calls like vector32('[1, 2]') survive intact.
        let re = regex!(r"(?i)INSERT\s+INTO\s+(\w+)(?:\s*\(([^)]+)\))?\s+VALUES\s*\((.+)\)\s*;?\s*$");

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid INSERT syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();
        
        let columns = captures.get(2).map(|m| {
            m.as_str()
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        });

        let values_str = captures.get(3).unwrap().as_str();
        let values = self.parse_values(values_str)?;

        Ok(Statement::Insert {
            table: table_name,
            columns,
            values,
        })
    }

    fn parse_select(&self, sql: &str) -> Result<Statement> {
        // SELECT * FROM users WHERE age > 25
        // SELECT id, name FROM users
        // SELECT COUNT(*) FROM users
        // SELECT * FROM users WHERE age > 25 ORDER BY name DESC LIMIT 10

        // Strip trailing LIMIT n first, then ORDER BY clause, so the remainder
        // is a regular SELECT [...] FROM <table> [WHERE ...].
        let mut remaining = sql.trim().to_string();

        let limit_re = regex!(r"(?i)\s+LIMIT\s+(\d+)\s*;?\s*$");
        let limit = if let Some(caps) = limit_re.captures(&remaining) {
            let n = caps
                .get(1)
                .unwrap()
                .as_str()
                .parse::<u64>()
                .map_err(|e| VelociError::ParseError(format!("Invalid LIMIT value: {}", e)))?;
            remaining = limit_re.replace(&remaining, "").to_string();
            Some(n)
        } else {
            None
        };

        // ORDER BY accepts either a column name or an arbitrary expression
        // (e.g. a vector distance function containing commas and parens), so
        // it is located with a quote-aware scan rather than a regex.
        let order_by = if let Some(idx) = find_order_by(&remaining) {
            let clause = remaining[idx..].trim();
            // Strip the leading "ORDER BY" (already validated by find_order_by).
            let expr_start = {
                let re = regex!(r"(?i)^ORDER\s+BY\s+");
                re.find(clause)
                    .map(|m| m.end())
                    .ok_or_else(|| VelociError::ParseError("Invalid ORDER BY".to_string()))?
            };
            let mut expr = clause[expr_start..].trim().trim_end_matches(';').trim().to_string();

            let mut ascending = true;
            let upper_expr = expr.to_uppercase();
            if upper_expr.ends_with(" DESC") {
                ascending = false;
                expr.truncate(expr.len() - 5);
            } else if upper_expr.ends_with(" ASC") {
                expr.truncate(expr.len() - 4);
            }
            let expr = expr.trim().to_string();
            if expr.is_empty() {
                return Err(VelociError::ParseError("Empty ORDER BY expression".to_string()));
            }

            remaining = remaining[..idx].trim_end().to_string();
            Some(OrderBy { column: expr, ascending })
        } else {
            None
        };

        // Drop trailing semicolons left over from earlier stripping.
        let remaining = remaining.trim_end_matches(';').trim().to_string();

        let re = regex!(r"(?i)^SELECT\s+(.+?)\s+FROM\s+(\w+)(?:\s+WHERE\s+(.+))?$");

        let captures = re
            .captures(&remaining)
            .ok_or_else(|| VelociError::ParseError("Invalid SELECT syntax".to_string()))?;

        let columns_str = captures.get(1).unwrap().as_str().trim();
        let columns = if columns_str == "*" {
            vec!["*".to_string()]
        } else if columns_str.to_uppercase().starts_with("COUNT(") {
            vec![columns_str.to_string()]
        } else {
            // Paren-aware split so distance expressions like
            // vector_distance_cos(embedding, vector32('[1,2]')) stay whole.
            split_top_level_commas(columns_str)
        };

        let table_name = captures.get(2).unwrap().as_str().to_string();

        let where_clause = if let Some(where_match) = captures.get(3) {
            Some(self.parse_where_clause(where_match.as_str())?)
        } else {
            None
        };

        Ok(Statement::Select {
            table: table_name,
            columns,
            where_clause,
            order_by,
            limit,
        })
    }

    fn parse_update(&self, sql: &str) -> Result<Statement> {
        // UPDATE users SET age = 31 WHERE name = 'Alice'
        
        let re = regex!(r"(?i)UPDATE\s+(\w+)\s+SET\s+(.+?)(?:\s+WHERE\s+(.+))?$");

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid UPDATE syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();
        let assignments_str = captures.get(2).unwrap().as_str();

        let mut assignments = HashMap::new();
        for assignment in assignments_str.split(',') {
            let parts: Vec<&str> = assignment.split('=').collect();
            if parts.len() != 2 {
                return Err(VelociError::ParseError(format!(
                    "Invalid assignment: {}",
                    assignment
                )));
            }

            let column = parts[0].trim().to_string();
            let value = self.parse_value(parts[1].trim())?;
            assignments.insert(column, value);
        }

        let where_clause = if let Some(where_match) = captures.get(3) {
            Some(self.parse_where_clause(where_match.as_str())?)
        } else {
            None
        };

        Ok(Statement::Update {
            table: table_name,
            assignments,
            where_clause,
        })
    }

    fn parse_delete(&self, sql: &str) -> Result<Statement> {
        // DELETE FROM users WHERE id = 2
        
        let re = regex!(r"(?i)DELETE\s+FROM\s+(\w+)(?:\s+WHERE\s+(.+))?");

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid DELETE syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();

        let where_clause = if let Some(where_match) = captures.get(2) {
            Some(self.parse_where_clause(where_match.as_str())?)
        } else {
            None
        };

        Ok(Statement::Delete {
            table: table_name,
            where_clause,
        })
    }

    fn parse_where_clause(&self, clause: &str) -> Result<WhereClause> {
        // Split on AND (case-insensitive), respecting quoted strings
        let parts = self.split_on_and(clause);
        let mut conditions = Vec::new();

        let re = regex!(r"(\w+)\s*(>=|<=|!=|<>|LIKE|=|>|<)\s*(.+)");

        for part in &parts {
            let part = part.trim();
            let captures = re
                .captures(part)
                .ok_or_else(|| VelociError::ParseError(format!("Invalid WHERE condition: {}", part)))?;

            let column = captures.get(1).unwrap().as_str().to_string();
            let operator_str = captures.get(2).unwrap().as_str();
            let operator = Operator::from_str(operator_str)?;
            let value = self.parse_value(captures.get(3).unwrap().as_str().trim())?;

            conditions.push(Condition {
                column,
                operator,
                value,
            });
        }

        if conditions.is_empty() {
            return Err(VelociError::ParseError(format!("Empty WHERE clause: {}", clause)));
        }

        Ok(WhereClause { conditions })
    }

    fn split_on_and(&self, clause: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_string = false;
        let mut string_char = '\'';
        let chars: Vec<char> = clause.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            if in_string {
                if chars[i] == string_char {
                    in_string = false;
                }
                current.push(chars[i]);
                i += 1;
            } else if chars[i] == '\'' || chars[i] == '"' {
                in_string = true;
                string_char = chars[i];
                current.push(chars[i]);
                i += 1;
            } else if i + 4 < chars.len()
                && chars[i].is_whitespace()
                && (chars[i + 1] == 'A' || chars[i + 1] == 'a')
                && (chars[i + 2] == 'N' || chars[i + 2] == 'n')
                && (chars[i + 3] == 'D' || chars[i + 3] == 'd')
                && chars[i + 4].is_whitespace()
            {
                parts.push(current);
                current = String::new();
                i += 5; // skip " AND "
            } else {
                current.push(chars[i]);
                i += 1;
            }
        }

        if !current.trim().is_empty() {
            parts.push(current);
        }

        parts
    }

    fn parse_values(&self, values_str: &str) -> Result<Vec<Value>> {
        let mut values = Vec::new();
        let mut current = String::new();
        let mut in_string = false;
        let mut string_char = '\'';
        let mut escaped = false;
        let mut depth: usize = 0; // parens / brackets nesting outside strings

        for ch in values_str.chars() {
            if escaped {
                current.push(ch);
                escaped = false;
            } else if ch == '\\' && in_string {
                escaped = true;
                current.push(ch);
            } else if !in_string && (ch == '\'' || ch == '"') {
                in_string = true;
                string_char = ch;
                current.push(ch);
            } else if in_string && ch == string_char {
                in_string = false;
                current.push(ch);
            } else if !in_string && (ch == '(' || ch == '[') {
                depth += 1;
                current.push(ch);
            } else if !in_string && (ch == ')' || ch == ']') {
                depth = depth.saturating_sub(1);
                current.push(ch);
            } else if !in_string && depth == 0 && ch == ',' {
                values.push(self.parse_value(current.trim())?);
                current = String::new();
            } else {
                current.push(ch);
            }
        }

        if !current.trim().is_empty() {
            values.push(self.parse_value(current.trim())?);
        }

        Ok(values)
    }

    fn parse_value(&self, s: &str) -> Result<Value> {
        let s = s.trim();

        // NULL
        if s.to_uppercase() == "NULL" {
            return Ok(Value::Null);
        }

        // Vector constructor: vector32('[...]'), vector('[...]') or bare [...]
        if let Some(parsed) = crate::vector::parse_vector_constructor(s) {
            // Quoted '[...]' strings stay Text unless explicitly constructed,
            // so only accept unquoted forms here.
            if !s.starts_with('\'') && !s.starts_with('"') {
                return Ok(Value::Vector(parsed?));
            }
        }

        // String (quoted) - handle escaped quotes
        if (s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('"') && s.ends_with('"'))
        {
            let quote_char = s.chars().next().unwrap();
            let content = &s[1..s.len() - 1];

            // Handle escaped quotes
            let unescaped = content.replace(&format!("\\{}", quote_char), &quote_char.to_string())
                                   .replace("\\\\", "\\");

            return Ok(Value::Text(unescaped));
        }

        // Try integer
        if let Ok(i) = s.parse::<i64>() {
            return Ok(Value::Integer(i));
        }

        // Try float
        if let Ok(f) = s.parse::<f64>() {
            return Ok(Value::Float(f));
        }

        // BLOB literal (X'hexdigits' or x'hexdigits')
        if s.len() >= 3 && (s.starts_with("X'") || s.starts_with("x'")) && s.ends_with('\'') {
            let hex_part = &s[2..s.len() - 1];
            if hex_part.len() % 2 != 0 {
                return Err(VelociError::ParseError("Invalid BLOB literal: odd number of hex digits".to_string()));
            }

            let mut blob = Vec::new();
            for i in (0..hex_part.len()).step_by(2) {
                let byte_str = &hex_part[i..i + 2];
                match u8::from_str_radix(byte_str, 16) {
                    Ok(byte) => blob.push(byte),
                    Err(_) => return Err(VelociError::ParseError(format!("Invalid hex digit in BLOB: {}", byte_str))),
                }
            }
            return Ok(Value::Blob(blob));
        }

        // Default to text without quotes
        Ok(Value::Text(s.to_string()))
    }

    fn parse_identifier<'a>(&self, s: &'a str) -> Result<(String, &'a str)> {
        let s = s.trim();

        // Quoted identifier
        if s.starts_with('"') || s.starts_with('`') || s.starts_with('[') {
            let quote_char = s.chars().next().unwrap();
            let end_quote = match quote_char {
                '"' => '"',
                '`' => '`',
                '[' => ']',
                _ => return Err(VelociError::ParseError("Invalid quote character".to_string())),
            };

            let mut identifier = String::new();
            let mut escaped = false;

            // Iterate over chars with their byte positions
            let mut chars_iter = s.char_indices();
            
            // Skip the opening quote
            chars_iter.next();

            for (pos, ch) in chars_iter {
                if escaped {
                    identifier.push(ch);
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == end_quote {
                    // Calculate the remainder starting after the closing quote
                    let rest_start = pos + ch.len_utf8();
                    return Ok((identifier, &s[rest_start..]));
                } else {
                    identifier.push(ch);
                }
            }

            return Err(VelociError::ParseError("Unterminated quoted identifier".to_string()));
        }

        // Unquoted identifier (stops at first whitespace)
        if let Some(space_pos) = s.find(char::is_whitespace) {
            let (ident, rest) = s.split_at(space_pos);
            Ok((ident.to_string(), rest))
        } else {
            Ok((s.to_string(), ""))
        }
    }

    fn split_sql_parts(&self, s: &str) -> Vec<String> {
        let mut parts = Vec::new();
        let mut current = String::new();
        let mut in_string = false;
        let mut string_char = '"';
        let mut escaped = false;

        for ch in s.chars() {
            if escaped {
                current.push(ch);
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
                current.push(ch);
            } else if !in_string && (ch == '"' || ch == '\'') {
                in_string = true;
                string_char = ch;
                current.push(ch);
            } else if in_string && ch == string_char {
                in_string = false;
                current.push(ch);
            } else if !in_string && ch.is_whitespace() {
                if !current.is_empty() {
                    parts.push(current);
                    current = String::new();
                }
            } else {
                current.push(ch);
            }
        }

        if !current.is_empty() {
            parts.push(current);
        }

        parts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_table() {
        let parser = Parser::new();
        let stmt = parser
            .parse("CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)")
            .unwrap();

        match stmt {
            Statement::CreateTable { name, columns } => {
                assert_eq!(name, "users");
                assert_eq!(columns.len(), 3);
                assert_eq!(columns[0].name, "id");
                assert!(columns[0].primary_key);
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_insert() {
        let parser = Parser::new();
        let stmt = parser
            .parse("INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)")
            .unwrap();

        match stmt {
            Statement::Insert {
                table,
                columns,
                values,
            } => {
                assert_eq!(table, "users");
                assert!(columns.is_some());
                assert_eq!(values.len(), 3);
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_select() {
        let parser = Parser::new();
        let stmt = parser
            .parse("SELECT * FROM users WHERE age > 25")
            .unwrap();

        match stmt {
            Statement::Select {
                table,
                columns,
                where_clause,
                order_by,
                limit,
            } => {
                assert_eq!(table, "users");
                assert_eq!(columns, vec!["*"]);
                assert!(where_clause.is_some());
                assert!(order_by.is_none());
                assert!(limit.is_none());
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_select_order_by_limit() {
        let parser = Parser::new();
        let stmt = parser
            .parse("SELECT * FROM users WHERE age > 18 ORDER BY name DESC LIMIT 5")
            .unwrap();

        match stmt {
            Statement::Select {
                table,
                where_clause,
                order_by,
                limit,
                ..
            } => {
                assert_eq!(table, "users");
                assert!(where_clause.is_some());
                let order = order_by.unwrap();
                assert_eq!(order.column, "name");
                assert!(!order.ascending);
                assert_eq!(limit, Some(5));
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_select_order_by_default_ascending() {
        let parser = Parser::new();
        let stmt = parser.parse("SELECT * FROM t ORDER BY id").unwrap();
        match stmt {
            Statement::Select { order_by, limit, where_clause, .. } => {
                let order = order_by.unwrap();
                assert_eq!(order.column, "id");
                assert!(order.ascending);
                assert!(where_clause.is_none());
                assert!(limit.is_none());
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_select_limit_only() {
        let parser = Parser::new();
        let stmt = parser.parse("SELECT * FROM t LIMIT 3").unwrap();
        match stmt {
            Statement::Select { limit, order_by, where_clause, .. } => {
                assert_eq!(limit, Some(3));
                assert!(order_by.is_none());
                assert!(where_clause.is_none());
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_update() {
        let parser = Parser::new();
        let stmt = parser
            .parse("UPDATE users SET age = 31 WHERE name = 'Alice'")
            .unwrap();

        match stmt {
            Statement::Update {
                table,
                assignments,
                where_clause,
            } => {
                assert_eq!(table, "users");
                assert_eq!(assignments.len(), 1);
                assert!(where_clause.is_some());
            }
            _ => panic!("Wrong statement type"),
        }
    }

    #[test]
    fn test_parse_delete() {
        let parser = Parser::new();
        let stmt = parser.parse("DELETE FROM users WHERE id = 2").unwrap();

        match stmt {
            Statement::Delete {
                table,
                where_clause,
            } => {
                assert_eq!(table, "users");
                assert!(where_clause.is_some());
            }
            _ => panic!("Wrong statement type"),
        }
    }
}


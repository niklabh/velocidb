// Type definitions for VelociDB

use std::fmt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub type PageId = u64;
pub type TransactionId = u64;

/// Represents errors that can occur in VelociDB.
#[derive(Debug, Error)]
pub enum VelociError {
    /// IO error occurred.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    /// Custom IO error message.
    #[error("IO error: {0}")]
    IoError(String),
    
    /// Database file corruption detected.
    #[error("Database corruption: {0}")]
    Corruption(String),
    
    /// Database is busy (locked).
    #[error("Database is busy")]
    Busy,
    
    /// Item not found (table, row, etc.).
    #[error("Not found: {0}")]
    NotFound(String),
    
    /// Constraint violation (e.g., unique key).
    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),
    
    /// SQL parsing error.
    #[error("Parse error: {0}")]
    ParseError(String),
    
    /// Transaction error.
    #[error("Transaction error: {0}")]
    TransactionError(String),
    
    /// Type mismatch during value conversion.
    #[error("Type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
    
    /// Storage engine error.
    #[error("Storage error: {0}")]
    StorageError(String),
    
    /// Feature not implemented.
    #[error("Not implemented: {0}")]
    NotImplemented(String),
}

/// A specialized Result type for VelociDB operations.
pub type Result<T> = std::result::Result<T, VelociError>;

/// Represents a value in a database cell.
///
/// `Value` supports standard SQL types: NULL, Integer, Float/Real, Text, and Blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Value {
    /// SQL NULL value.
    Null,
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit floating point number.
    Float(f64),
    /// Alias for Float.
    Real(f64),
    /// UTF-8 text string.
    Text(String),
    /// Binary data.
    Blob(Vec<u8>),
}

impl Value {
    /// Converts the value to an integer if possible.
    pub fn as_integer(&self) -> Result<i64> {
        match self {
            Value::Integer(i) => Ok(*i),
            _ => Err(VelociError::TypeMismatch {
                expected: "Integer".to_string(),
                actual: format!("{:?}", self),
            }),
        }
    }

    /// Converts the value to text if possible.
    pub fn as_text(&self) -> Result<&str> {
        match self {
            Value::Text(s) => Ok(s),
            _ => Err(VelociError::TypeMismatch {
                expected: "Text".to_string(),
                actual: format!("{:?}", self),
            }),
        }
    }

    /// Converts the value to a float if possible.
    ///
    /// Handles both `Float`/`Real` variants and converts `Integer` to `f64`.
    pub fn as_float(&self) -> Result<f64> {
        match self {
            Value::Float(f) => Ok(*f),
            Value::Real(f) => Ok(*f),
            Value::Integer(i) => Ok(*i as f64),
            _ => Err(VelociError::TypeMismatch {
                expected: "Float".to_string(),
                actual: format!("{:?}", self),
            }),
        }
    }
    
    /// Returns the size of the value in bytes when serialized.
    pub fn size_bytes(&self) -> usize {
        match self {
            Value::Null => 1,
            Value::Integer(_) => 9, // 1 byte type + 8 bytes data
            Value::Float(_) => 9,   // 1 byte type + 8 bytes data
            Value::Real(_) => 9,    // 1 byte type + 8 bytes data
            Value::Text(s) => 1 + 4 + s.len(), // 1 byte type + 4 bytes length + data
            Value::Blob(b) => 1 + 4 + b.len(), // 1 byte type + 4 bytes length + data
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(i) => write!(f, "{}", i),
            Value::Float(fl) => write!(f, "{}", fl),
            Value::Real(fl) => write!(f, "{}", fl),
            Value::Text(s) => write!(f, "{}", s),
            Value::Blob(b) => write!(f, "<blob {} bytes>", b.len()),
        }
    }
}

/// Supported SQL data types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    Integer,
    Real,
    Text,
    Blob,
    Null,
}

impl DataType {
    /// Parses a data type from a string (case-insensitive).
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "INTEGER" | "INT" => DataType::Integer,
            "REAL" | "FLOAT" | "DOUBLE" => DataType::Real,
            "TEXT" | "VARCHAR" | "STRING" => DataType::Text,
            "BLOB" => DataType::Blob,
            _ => DataType::Text, // Default to TEXT
        }
    }
}

/// Metadata for a table column.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// Column name.
    pub name: String,
    /// Data type of the column.
    pub data_type: DataType,
    /// Whether the column is a primary key.
    pub primary_key: bool,
    /// Whether the column cannot contain NULL values.
    pub not_null: bool,
    /// Whether the column values must be unique.
    pub unique: bool,
}

/// A single row in a query result.
#[derive(Debug, Clone)]
pub struct Row {
    /// The values in the row, corresponding to the columns.
    pub values: Vec<Value>,
}

impl Row {
    /// Creates a new row from a vector of values.
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }
}

/// The result of a SQL query.
///
/// Contains the schema (columns) and the data (rows).
#[derive(Debug)]
pub struct QueryResult {
    /// The columns in the result set.
    pub columns: Vec<Column>,
    /// The rows returned by the query.
    pub rows: Vec<Row>,
}

impl QueryResult {
    /// Creates a new query result.
    pub fn new(columns: Vec<Column>, rows: Vec<Row>) -> Self {
        Self { columns, rows }
    }

    /// Creates an empty query result.
    pub fn empty() -> Self {
        Self {
            columns: vec![],
            rows: vec![],
        }
    }
}


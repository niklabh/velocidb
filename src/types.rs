// Type definitions for VelociDB

use std::fmt;
use thiserror::Error;

pub type PageId = u64;
pub type TransactionId = u64;

#[derive(Debug, Error)]
pub enum VelociError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    
    #[error("Database corruption: {0}")]
    Corruption(String),
    
    #[error("Database is busy")]
    Busy,
    
    #[error("Not found: {0}")]
    NotFound(String),
    
    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),
    
    #[error("Parse error: {0}")]
    ParseError(String),
    
    #[error("Transaction error: {0}")]
    TransactionError(String),
    
    #[error("Type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },
}

pub type Result<T> = std::result::Result<T, VelociError>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_integer(&self) -> Result<i64> {
        match self {
            Value::Integer(i) => Ok(*i),
            _ => Err(VelociError::TypeMismatch {
                expected: "Integer".to_string(),
                actual: format!("{:?}", self),
            }),
        }
    }

    pub fn as_text(&self) -> Result<&str> {
        match self {
            Value::Text(s) => Ok(s),
            _ => Err(VelociError::TypeMismatch {
                expected: "Text".to_string(),
                actual: format!("{:?}", self),
            }),
        }
    }

    pub fn as_float(&self) -> Result<f64> {
        match self {
            Value::Float(f) => Ok(*f),
            Value::Integer(i) => Ok(*i as f64),
            _ => Err(VelociError::TypeMismatch {
                expected: "Float".to_string(),
                actual: format!("{:?}", self),
            }),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Integer(i) => write!(f, "{}", i),
            Value::Float(fl) => write!(f, "{}", fl),
            Value::Text(s) => write!(f, "{}", s),
            Value::Blob(b) => write!(f, "<blob {} bytes>", b.len()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    Integer,
    Real,
    Text,
    Blob,
    Null,
}

impl DataType {
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

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub primary_key: bool,
    pub not_null: bool,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub struct Row {
    pub values: Vec<Value>,
}

impl Row {
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }
}

#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<Column>,
    pub rows: Vec<Row>,
}

impl QueryResult {
    pub fn new(columns: Vec<Column>, rows: Vec<Row>) -> Self {
        Self { columns, rows }
    }

    pub fn empty() -> Self {
        Self {
            columns: vec![],
            rows: vec![],
        }
    }
}


// VelociDB library interface

pub mod storage;
pub mod btree;
pub mod parser;
pub mod executor;
pub mod transaction;
pub mod types;

// Re-export commonly used types
pub use storage::Database;
pub use types::{QueryResult, Value, Row, Column};


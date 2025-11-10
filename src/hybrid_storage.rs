// Hybrid Row/Columnar Storage Layout
// Row-major for OLTP, columnar projections for OLAP

use crate::types::{Column, Result, Row, Value, VelociError};
use crate::simd::{VectorColumn, VectorBatch};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// Storage layout type
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StorageLayout {
    /// Row-oriented storage (NSM - N-ary Storage Model)
    RowMajor,
    /// Column-oriented storage (DSM - Decomposition Storage Model)
    ColumnMajor,
    /// Hybrid: Row-major with columnar projections
    Hybrid,
}

/// Row-oriented record (traditional)
#[derive(Debug, Clone)]
pub struct RowRecord {
    pub key: i64,
    pub values: Vec<Value>,
}

impl RowRecord {
    pub fn new(key: i64, values: Vec<Value>) -> Self {
        Self { key, values }
    }

    /// Get the size in bytes
    pub fn size_bytes(&self) -> usize {
        8 + self.values.iter().map(|v| v.size_bytes()).sum::<usize>()
    }
}

/// Column-oriented storage for a single column
#[derive(Debug, Clone)]
pub struct ColumnStorage {
    pub name: String,
    pub data: VectorColumn,
    pub null_bitmap: Vec<bool>,
}

impl ColumnStorage {
    pub fn new(name: String, capacity: usize) -> Self {
        Self {
            name,
            data: VectorColumn::Null(0),
            null_bitmap: Vec::with_capacity(capacity),
        }
    }

    pub fn from_integers(name: String, values: Vec<i64>, nulls: Vec<bool>) -> Self {
        Self {
            name,
            data: VectorColumn::Integer(values),
            null_bitmap: nulls,
        }
    }

    pub fn from_reals(name: String, values: Vec<f64>, nulls: Vec<bool>) -> Self {
        Self {
            name,
            data: VectorColumn::Real(values),
            null_bitmap: nulls,
        }
    }

    pub fn from_text(name: String, values: Vec<String>, nulls: Vec<bool>) -> Self {
        Self {
            name,
            data: VectorColumn::Text(values),
            null_bitmap: nulls,
        }
    }

    /// Get value at index
    pub fn get(&self, index: usize) -> Option<Value> {
        if index >= self.null_bitmap.len() {
            return None;
        }

        if self.null_bitmap[index] {
            return Some(Value::Null);
        }

        match &self.data {
            VectorColumn::Integer(vec) => vec.get(index).map(|&v| Value::Integer(v)),
            VectorColumn::Real(vec) => vec.get(index).map(|&v| Value::Real(v)),
            VectorColumn::Text(vec) => vec.get(index).map(|v| Value::Text(v.clone())),
            VectorColumn::Null(_) => Some(Value::Null),
        }
    }

    /// Set value at index
    pub fn set(&mut self, index: usize, value: Value) -> Result<()> {
        // Ensure capacity
        while self.null_bitmap.len() <= index {
            self.null_bitmap.push(false);
        }

        match value {
            Value::Null => {
                self.null_bitmap[index] = true;
            }
            Value::Integer(v) => {
                if let VectorColumn::Integer(ref mut vec) = self.data {
                    while vec.len() <= index {
                        vec.push(0);
                    }
                    vec[index] = v;
                    self.null_bitmap[index] = false;
                }
            }
            Value::Real(v) => {
                if let VectorColumn::Real(ref mut vec) = self.data {
                    while vec.len() <= index {
                        vec.push(0.0);
                    }
                    vec[index] = v;
                    self.null_bitmap[index] = false;
                }
            }
            Value::Text(v) => {
                if let VectorColumn::Text(ref mut vec) = self.data {
                    while vec.len() <= index {
                        vec.push(String::new());
                    }
                    vec[index] = v;
                    self.null_bitmap[index] = false;
                }
            }
            _ => return Err(VelociError::TypeMismatch {
                expected: "Integer, Real, or Text".to_string(),
                actual: format!("{:?}", value),
            }),
        }

        Ok(())
    }

    /// Get the number of values
    pub fn len(&self) -> usize {
        self.null_bitmap.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.null_bitmap.is_empty()
    }
}

/// Hybrid storage table
pub struct HybridTable {
    /// Table name
    pub name: String,
    /// Schema
    pub columns: Vec<Column>,
    /// Primary row storage (row-major)
    row_store: Arc<RwLock<HashMap<i64, RowRecord>>>,
    /// Columnar projections (column-major, materialized on demand)
    column_store: Arc<RwLock<HashMap<String, ColumnStorage>>>,
    /// Layout mode
    layout: StorageLayout,
    /// Statistics for adaptive layout
    stats: Arc<RwLock<TableStats>>,
}

impl HybridTable {
    pub fn new(name: String, columns: Vec<Column>, layout: StorageLayout) -> Self {
        Self {
            name,
            columns,
            row_store: Arc::new(RwLock::new(HashMap::new())),
            column_store: Arc::new(RwLock::new(HashMap::new())),
            layout,
            stats: Arc::new(RwLock::new(TableStats::default())),
        }
    }

    /// Insert a row
    pub fn insert_row(&self, key: i64, values: Vec<Value>) -> Result<()> {
        let record = RowRecord::new(key, values);
        self.row_store.write().insert(key, record);
        
        // Update stats
        self.stats.write().record_insert();

        // Invalidate columnar projections
        if self.layout == StorageLayout::Hybrid {
            self.column_store.write().clear();
        }

        Ok(())
    }

    /// Get a row by key
    pub fn get_row(&self, key: i64) -> Option<RowRecord> {
        self.stats.write().record_row_access();
        self.row_store.read().get(&key).cloned()
    }

    /// Scan all rows
    pub fn scan_rows(&self) -> Vec<RowRecord> {
        self.stats.write().record_full_scan();
        self.row_store.read().values().cloned().collect()
    }

    /// Get a columnar projection (materialized on demand)
    pub fn get_column_projection(&self, column_name: &str) -> Result<ColumnStorage> {
        self.stats.write().record_column_access();

        // Check if already materialized
        {
            let column_store = self.column_store.read();
            if let Some(col) = column_store.get(column_name) {
                return Ok(col.clone());
            }
        }

        // Materialize from row store
        let column_idx = self.columns.iter()
            .position(|c| c.name == column_name)
            .ok_or_else(|| VelociError::NotFound(format!("Column {} not found", column_name)))?;

        let row_store = self.row_store.read();
        let rows: Vec<_> = row_store.values().collect();

        // Determine column type from first non-null value
        let first_value = rows.iter()
            .find_map(|r| r.values.get(column_idx))
            .ok_or_else(|| VelociError::NotFound("No values in column".to_string()))?;

        let mut column_storage = match first_value {
            Value::Integer(_) => {
                let mut values = Vec::new();
                let mut nulls = Vec::new();
                
                for row in rows.iter() {
                    match row.values.get(column_idx) {
                        Some(Value::Integer(v)) => {
                            values.push(*v);
                            nulls.push(false);
                        }
                        Some(Value::Null) | None => {
                            values.push(0);
                            nulls.push(true);
                        }
                        _ => {
                            values.push(0);
                            nulls.push(true);
                        }
                    }
                }
                
                ColumnStorage::from_integers(column_name.to_string(), values, nulls)
            }
            Value::Real(_) => {
                let mut values = Vec::new();
                let mut nulls = Vec::new();
                
                for row in rows.iter() {
                    match row.values.get(column_idx) {
                        Some(Value::Real(v)) => {
                            values.push(*v);
                            nulls.push(false);
                        }
                        Some(Value::Null) | None => {
                            values.push(0.0);
                            nulls.push(true);
                        }
                        _ => {
                            values.push(0.0);
                            nulls.push(true);
                        }
                    }
                }
                
                ColumnStorage::from_reals(column_name.to_string(), values, nulls)
            }
            Value::Text(_) => {
                let mut values = Vec::new();
                let mut nulls = Vec::new();
                
                for row in rows.iter() {
                    match row.values.get(column_idx) {
                        Some(Value::Text(v)) => {
                            values.push(v.clone());
                            nulls.push(false);
                        }
                        Some(Value::Null) | None => {
                            values.push(String::new());
                            nulls.push(true);
                        }
                        _ => {
                            values.push(String::new());
                            nulls.push(true);
                        }
                    }
                }
                
                ColumnStorage::from_text(column_name.to_string(), values, nulls)
            }
            _ => return Err(VelociError::TypeMismatch {
                expected: "Integer, Real, or Text".to_string(),
                actual: format!("{:?}", first_value),
            }),
        };

        // Cache the projection
        self.column_store.write().insert(column_name.to_string(), column_storage.clone());

        Ok(column_storage)
    }

    /// Get multiple columnar projections as a vector batch
    pub fn get_vector_batch(&self, column_names: Vec<&str>) -> Result<VectorBatch> {
        let mut batch = VectorBatch::new();

        for column_name in column_names {
            let col_storage = self.get_column_projection(column_name)?;
            batch.add_column(col_storage.data);
        }

        let row_count = self.row_store.read().len();
        batch.set_row_count(row_count);

        Ok(batch)
    }

    /// Decide whether to use row-major or column-major access
    /// Based on access patterns
    pub fn adaptive_layout(&self) -> StorageLayout {
        let stats = self.stats.read();

        if stats.column_accesses > stats.row_accesses * 2 {
            // Analytical workload (OLAP)
            StorageLayout::ColumnMajor
        } else if stats.row_accesses > stats.column_accesses * 2 {
            // Transactional workload (OLTP)
            StorageLayout::RowMajor
        } else {
            // Mixed workload
            StorageLayout::Hybrid
        }
    }

    /// Get statistics
    pub fn get_stats(&self) -> TableStats {
        self.stats.read().clone()
    }
}

/// Table access statistics
#[derive(Debug, Clone, Default)]
pub struct TableStats {
    pub inserts: u64,
    pub row_accesses: u64,
    pub column_accesses: u64,
    pub full_scans: u64,
}

impl TableStats {
    pub fn record_insert(&mut self) {
        self.inserts += 1;
    }

    pub fn record_row_access(&mut self) {
        self.row_accesses += 1;
    }

    pub fn record_column_access(&mut self) {
        self.column_accesses += 1;
    }

    pub fn record_full_scan(&mut self) {
        self.full_scans += 1;
    }

    pub fn workload_type(&self) -> &str {
        if self.column_accesses > self.row_accesses * 2 {
            "OLAP (Analytical)"
        } else if self.row_accesses > self.column_accesses * 2 {
            "OLTP (Transactional)"
        } else {
            "Hybrid (Mixed)"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DataType;

    #[test]
    fn test_row_storage() {
        let columns = vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Integer,
                primary_key: true,
                not_null: true,
                unique: true,
            },
            Column {
                name: "name".to_string(),
                data_type: DataType::Text,
                primary_key: false,
                not_null: false,
                unique: false,
            },
        ];

        let table = HybridTable::new("users".to_string(), columns, StorageLayout::RowMajor);

        // Insert rows
        table.insert_row(1, vec![Value::Integer(1), Value::Text("Alice".to_string())]).unwrap();
        table.insert_row(2, vec![Value::Integer(2), Value::Text("Bob".to_string())]).unwrap();

        // Get row
        let row = table.get_row(1).unwrap();
        assert_eq!(row.key, 1);
        assert_eq!(row.values[1], Value::Text("Alice".to_string()));
    }

    #[test]
    fn test_column_projection() {
        let columns = vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Integer,
                primary_key: true,
                not_null: true,
                unique: true,
            },
            Column {
                name: "age".to_string(),
                data_type: DataType::Integer,
                primary_key: false,
                not_null: false,
                unique: false,
            },
        ];

        let table = HybridTable::new("users".to_string(), columns, StorageLayout::Hybrid);

        // Insert rows
        table.insert_row(1, vec![Value::Integer(1), Value::Integer(30)]).unwrap();
        table.insert_row(2, vec![Value::Integer(2), Value::Integer(25)]).unwrap();
        table.insert_row(3, vec![Value::Integer(3), Value::Integer(35)]).unwrap();

        // Get columnar projection
        let col = table.get_column_projection("age").unwrap();
        assert_eq!(col.len(), 3);
        assert_eq!(col.get(0), Some(Value::Integer(30)));
        assert_eq!(col.get(1), Some(Value::Integer(25)));
        assert_eq!(col.get(2), Some(Value::Integer(35)));
    }

    #[test]
    fn test_vector_batch() {
        let columns = vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Integer,
                primary_key: true,
                not_null: true,
                unique: true,
            },
            Column {
                name: "value".to_string(),
                data_type: DataType::Integer,
                primary_key: false,
                not_null: false,
                unique: false,
            },
        ];

        let table = HybridTable::new("test".to_string(), columns, StorageLayout::Hybrid);

        // Insert rows
        for i in 1..=10 {
            table.insert_row(i, vec![Value::Integer(i), Value::Integer(i * 10)]).unwrap();
        }

        // Get vector batch
        let batch = table.get_vector_batch(vec!["value"]).unwrap();
        assert_eq!(batch.row_count(), 10);
    }

    #[test]
    fn test_adaptive_layout() {
        let columns = vec![
            Column {
                name: "id".to_string(),
                data_type: DataType::Integer,
                primary_key: true,
                not_null: true,
                unique: true,
            },
        ];

        let table = HybridTable::new("test".to_string(), columns, StorageLayout::Hybrid);

        // Simulate row accesses
        for _ in 0..10 {
            table.stats.write().record_row_access();
        }

        let layout = table.adaptive_layout();
        assert_eq!(layout, StorageLayout::RowMajor);

        // Simulate column accesses
        for _ in 0..30 {
            table.stats.write().record_column_access();
        }

        let layout = table.adaptive_layout();
        assert_eq!(layout, StorageLayout::ColumnMajor);
    }
}


// Query executor

use crate::btree::BTree;
use crate::parser::{Statement, WhereClause};
use crate::storage::{Pager, Schema, TableSchema};
use crate::transaction::{LockManager, LockType, TransactionManager};
use crate::types::{Column, QueryResult, Result, Row, Value, VelociError};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Executor {
    pager: Arc<RwLock<Pager>>,
    btrees: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
    schema: Arc<RwLock<Schema>>,
    transaction_manager: Arc<TransactionManager>,
    lock_manager: Arc<LockManager>,
}

impl Executor {
    pub fn new(
        pager: Arc<RwLock<Pager>>,
        btrees: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
        schema: Arc<RwLock<Schema>>,
        transaction_manager: Arc<TransactionManager>,
    ) -> Self {
        Self {
            pager,
            btrees,
            schema,
            transaction_manager,
            lock_manager: Arc::new(LockManager::new()),
        }
    }

    pub fn execute(&self, statement: Statement) -> Result<()> {
        match statement {
            Statement::CreateTable { name, columns } => self.execute_create_table(&name, columns),
            Statement::DropTable { name } => self.execute_drop_table(&name),
            Statement::Insert {
                table,
                columns,
                values,
            } => self.execute_insert(&table, columns, values),
            Statement::Update {
                table,
                assignments,
                where_clause,
            } => self.execute_update(&table, assignments, where_clause),
            Statement::Delete {
                table,
                where_clause,
            } => self.execute_delete(&table, where_clause),
            _ => Err(VelociError::ParseError(
                "Statement should be executed with query()".to_string(),
            )),
        }
    }

    pub fn query(&self, statement: Statement) -> Result<QueryResult> {
        match statement {
            Statement::Select {
                table,
                columns,
                where_clause,
            } => self.execute_select(&table, columns, where_clause),
            _ => Err(VelociError::ParseError(
                "Statement is not a query".to_string(),
            )),
        }
    }

    fn execute_create_table(&self, name: &str, columns: Vec<Column>) -> Result<()> {
        // Allocate and initialize a root page for the table
        let root_page = {
            let mut pager = self.pager.write();
            let root_page = pager.allocate_page()?;
            
            // Initialize as B-Tree leaf node
            let mut page = crate::storage::Page::new();
            let header = crate::btree::NodeHeader::new_leaf();
            header.serialize(page.data_mut());
            pager.write_page(root_page, &page)?;
            
            root_page
        };

        // Create the table schema
        let table_schema = TableSchema {
            name: name.to_string(),
            columns,
            root_page,
        };

        // Add to schema
        self.schema.write().create_table(table_schema)?;

        // Create a new B-Tree for the table
        let btree = BTree::from_root(root_page, Arc::clone(&self.pager));
        self.btrees
            .write()
            .insert(name.to_string(), Arc::new(RwLock::new(btree)));

        Ok(())
    }

    fn execute_drop_table(&self, name: &str) -> Result<()> {
        self.schema.write().drop_table(name)?;
        self.btrees.write().remove(name);
        Ok(())
    }

    fn execute_insert(
        &self,
        table: &str,
        columns: Option<Vec<String>>,
        values: Vec<Value>,
    ) -> Result<()> {
        // Start transaction
        let txn = self.transaction_manager.begin();
        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Exclusive)?;

        // Get table schema
        let schema = self.schema.read();
        let table_schema = schema.get_table(table)?;

        // Validate columns and values
        let column_names = if let Some(ref cols) = columns {
            cols.clone()
        } else {
            table_schema
                .columns
                .iter()
                .map(|c| c.name.clone())
                .collect()
        };

        if column_names.len() != values.len() {
            return Err(VelociError::ConstraintViolation(
                "Column count doesn't match value count".to_string(),
            ));
        }

        // Find primary key
        let pk_index = table_schema
            .columns
            .iter()
            .position(|c| c.primary_key)
            .ok_or_else(|| VelociError::ConstraintViolation("No primary key defined".to_string()))?;

        let pk_col_name = &table_schema.columns[pk_index].name;
        let pk_value_index = column_names
            .iter()
            .position(|c| c == pk_col_name)
            .ok_or_else(|| {
                VelociError::ConstraintViolation("Primary key value not provided".to_string())
            })?;

        let pk_value = values[pk_value_index].as_integer()?;

        // Check for primary key uniqueness
        let btrees = self.btrees.read();
        let btree_arc = btrees.get(table).ok_or_else(|| {
            VelociError::NotFound(format!("Table '{}' not initialized", table))
        })?;
        let btree = btree_arc.read();

        if btree.search(pk_value)?.is_some() {
            return Err(VelociError::ConstraintViolation(format!(
                "Primary key {} already exists in table '{}'",
                pk_value, table
            )));
        }
        drop(btree);
        drop(btrees);

        // Validate NOT NULL constraints
        for (i, value) in values.iter().enumerate() {
            let col_name = &column_names[i];
            if let Some(col) = table_schema.columns.iter().find(|c| &c.name == col_name) {
                if col.not_null && matches!(value, Value::Null) {
                    return Err(VelociError::ConstraintViolation(format!(
                        "Column '{}' cannot be NULL", col_name
                    )));
                }
            }
        }

        // Create a row with all columns (fill missing with NULL)
        let mut row_values = vec![Value::Null; table_schema.columns.len()];
        for (i, col_name) in column_names.iter().enumerate() {
            if let Some(col_index) = table_schema.columns.iter().position(|c| &c.name == col_name) {
                row_values[col_index] = values[i].clone();
            }
        }

        let row = Row::new(row_values);

        // Get or create B-Tree
        let btrees = self.btrees.read();
        let btree_arc = btrees.get(table).ok_or_else(|| {
            VelociError::NotFound(format!("Table '{}' not initialized", table))
        })?;
        let mut btree = btree_arc.write();

        // Insert into B-Tree
        btree.insert(pk_value, &row)?;

        // Commit transaction
        self.transaction_manager.commit(&txn)?;
        self.lock_manager.release_lock(table, txn.id())?;

        Ok(())
    }

    fn execute_select(
        &self,
        table: &str,
        columns: Vec<String>,
        where_clause: Option<WhereClause>,
    ) -> Result<QueryResult> {
        // Start transaction
        let txn = self.transaction_manager.begin();
        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Shared)?;

        // Get table schema
        let schema = self.schema.read();
        let table_schema = schema.get_table(table)?;

        // Get B-Tree
        let btrees = self.btrees.read();
        let btree_arc = btrees.get(table).ok_or_else(|| {
            VelociError::NotFound(format!("Table '{}' not initialized", table))
        })?;
        let btree = btree_arc.read();

        // Scan all rows
        let all_rows = btree.scan()?;

        // Filter rows based on WHERE clause
        let filtered_rows: Vec<(i64, Row)> = if let Some(ref where_clause) = where_clause {
            all_rows
                .into_iter()
                .filter(|(_, row)| self.evaluate_where_clause(row, where_clause, table_schema).unwrap_or(false))
                .collect()
        } else {
            all_rows
        };

        // Project columns
        let result_columns = if columns.len() == 1 && columns[0] == "*" {
            table_schema.columns.clone()
        } else {
            columns
                .iter()
                .filter_map(|col_name| {
                    table_schema
                        .columns
                        .iter()
                        .find(|c| &c.name == col_name)
                        .cloned()
                })
                .collect()
        };

        let result_rows: Vec<Row> = filtered_rows
            .into_iter()
            .map(|(_, row)| {
                if columns.len() == 1 && columns[0] == "*" {
                    row
                } else {
                    let projected_values: Vec<Value> = columns
                        .iter()
                        .filter_map(|col_name| {
                            table_schema
                                .columns
                                .iter()
                                .position(|c| &c.name == col_name)
                                .and_then(|idx| row.values.get(idx).cloned())
                        })
                        .collect();
                    Row::new(projected_values)
                }
            })
            .collect();

        // Commit transaction
        self.transaction_manager.commit(&txn)?;
        self.lock_manager.release_lock(table, txn.id())?;

        Ok(QueryResult::new(result_columns, result_rows))
    }

    fn execute_update(
        &self,
        table: &str,
        assignments: HashMap<String, Value>,
        where_clause: Option<WhereClause>,
    ) -> Result<()> {
        // Start transaction
        let txn = self.transaction_manager.begin();
        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Exclusive)?;

        // Get table schema
        let schema = self.schema.read();
        let table_schema = schema.get_table(table)?;

        // Get B-Tree
        let btrees = self.btrees.read();
        let btree_arc = btrees.get(table).ok_or_else(|| {
            VelociError::NotFound(format!("Table '{}' not initialized", table))
        })?;
        let mut btree = btree_arc.write();

        // Scan all rows
        let all_rows = btree.scan()?;

        // Find rows to update
        let rows_to_update: Vec<(i64, Row)> = if let Some(ref where_clause) = where_clause {
            all_rows
                .into_iter()
                .filter(|(_, row)| self.evaluate_where_clause(row, where_clause, table_schema).unwrap_or(false))
                .collect()
        } else {
            all_rows
        };

        // Update each row
        for (key, row) in &rows_to_update {
            let mut updated_row = row.clone();

            // Find primary key column
            let pk_index = table_schema
                .columns
                .iter()
                .position(|c| c.primary_key)
                .ok_or_else(|| VelociError::ConstraintViolation("No primary key defined".to_string()))?;

            let mut new_pk_value = *key; // Default to existing key
            let mut pk_being_updated = false;

            // Apply updates to the row
            for (col_name, new_value) in &assignments {
                if let Some(col_index) = table_schema.columns.iter().position(|c| &c.name == col_name) {
                    // Check NOT NULL constraint
                    let col = &table_schema.columns[col_index];
                    if col.not_null && matches!(new_value, Value::Null) {
                        return Err(VelociError::ConstraintViolation(format!(
                            "Column '{}' cannot be NULL", col_name
                        )));
                    }

                    updated_row.values[col_index] = new_value.clone();

                    // Check if primary key is being updated
                    if col_index == pk_index {
                        new_pk_value = new_value.as_integer()?;
                        pk_being_updated = true;
                    }
                }
            }

            // If primary key is being updated, check for uniqueness
            if pk_being_updated && new_pk_value != *key {
                if btree.search(new_pk_value)?.is_some() {
                    return Err(VelociError::ConstraintViolation(format!(
                        "Primary key {} already exists in table '{}'",
                        new_pk_value, table
                    )));
                }
            }

            // Delete old row and insert updated row
            btree.delete(*key)?;
            btree.insert(new_pk_value, &updated_row)?;
        }

        // Commit transaction
        self.transaction_manager.commit(&txn)?;
        self.lock_manager.release_lock(table, txn.id())?;

        Ok(())
    }

    fn execute_delete(&self, table: &str, where_clause: Option<WhereClause>) -> Result<()> {
        // Start transaction
        let txn = self.transaction_manager.begin();
        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Exclusive)?;

        // Get table schema
        let schema = self.schema.read();
        let table_schema = schema.get_table(table)?;

        // Get B-Tree
        let btrees = self.btrees.read();
        let btree_arc = btrees.get(table).ok_or_else(|| {
            VelociError::NotFound(format!("Table '{}' not initialized", table))
        })?;
        let mut btree = btree_arc.write();

        // Scan all rows
        let all_rows = btree.scan()?;

        // Find rows to delete
        let rows_to_delete: Vec<i64> = if let Some(ref where_clause) = where_clause {
            all_rows
                .into_iter()
                .filter(|(_, row)| self.evaluate_where_clause(row, where_clause, table_schema).unwrap_or(false))
                .map(|(key, _)| key)
                .collect()
        } else {
            all_rows.into_iter().map(|(key, _)| key).collect()
        };

        // Delete each row
        for key in rows_to_delete {
            btree.delete(key)?;
        }

        // Commit transaction
        self.transaction_manager.commit(&txn)?;
        self.lock_manager.release_lock(table, txn.id())?;

        Ok(())
    }

    fn evaluate_where_clause(
        &self,
        row: &Row,
        where_clause: &WhereClause,
        table_schema: &TableSchema,
    ) -> Result<bool> {
        for condition in &where_clause.conditions {
            let col_index = table_schema
                .columns
                .iter()
                .position(|c| c.name == condition.column)
                .ok_or_else(|| {
                    VelociError::NotFound(format!("Column '{}' not found", condition.column))
                })?;

            let row_value = &row.values[col_index];
            let condition_value = &condition.value;

            if !condition.operator.evaluate(row_value, condition_value)? {
                return Ok(false);
            }
        }

        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;
    use crate::storage::Database;
    use tempfile::NamedTempFile;

    #[test]
    fn test_create_and_insert() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open(temp_file.path()).unwrap();

        db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO test (id, name) VALUES (1, 'Alice')")
            .unwrap();
    }

    #[test]
    fn test_select() {
        let temp_file = NamedTempFile::new().unwrap();
        let db = Database::open(temp_file.path()).unwrap();

        db.execute("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO test (id, name) VALUES (1, 'Alice')")
            .unwrap();

        let result = db.query("SELECT * FROM test").unwrap();
        assert_eq!(result.rows.len(), 1);
    }
}


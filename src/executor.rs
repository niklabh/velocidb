//! Query executor that translates AST statements into storage operations.
//!
//! Coordinates B-Tree, MVCC, lock manager, and schema to execute SQL
//! statements with ACID guarantees. WHERE filtering, ORDER BY sorting, and
//! vector distance computation are parallelized with rayon for larger row
//! sets.

use crate::btree::BTree;
use crate::cdc::{CdcManager, ChangeOp};
use crate::parser::{AlterAction, OrderBy, Statement, WhereClause};
use crate::storage::{Pager, Schema, TableSchema};
use crate::transaction::{LockManager, LockType, TransactionManager, Transaction};
use crate::types::{Column, DataType, QueryResult, Result, Row, Value, VelociError};
use crate::vector::{self, DistanceMetric};
use parking_lot::RwLock;
use rayon::prelude::*;
use std::collections::HashMap;
use std::sync::Arc;

/// Row-count threshold above which filtering/sorting switches to rayon.
const PARALLEL_THRESHOLD: usize = 1024;

pub struct Executor {
    pager: Arc<RwLock<Pager>>,
    btrees: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
    schema: Arc<RwLock<Schema>>,
    transaction_manager: Arc<TransactionManager>,
    lock_manager: Arc<LockManager>,
    active_transaction: RwLock<Option<Arc<Transaction>>>,
    cdc: Arc<CdcManager>,
}

impl Executor {
    pub fn new(
        pager: Arc<RwLock<Pager>>,
        btrees: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
        schema: Arc<RwLock<Schema>>,
        transaction_manager: Arc<TransactionManager>,
        cdc: Arc<CdcManager>,
    ) -> Self {
        Self {
            pager,
            btrees,
            schema,
            transaction_manager,
            lock_manager: Arc::new(LockManager::new()),
            active_transaction: RwLock::new(None),
            cdc,
        }
    }

    pub fn execute_statement(&self, statement: Statement) -> Result<()> {
        match statement {
            Statement::CreateTable { name, columns } => self.execute_create_table(&name, columns),
            Statement::DropTable { name } => self.execute_drop_table(&name),
            Statement::AlterTable { table, action } => self.execute_alter_table(&table, action),
            Statement::Insert { table, columns, values } => self.execute_insert(&table, columns, values),
            Statement::Update { table, assignments, where_clause } => self.execute_update(&table, assignments, where_clause),
            Statement::Delete { table, where_clause } => self.execute_delete(&table, where_clause),
            Statement::BeginTransaction => self.begin_transaction(),
            Statement::CommitTransaction => self.commit_transaction(),
            Statement::RollbackTransaction => self.rollback_transaction(),
            _ => Err(VelociError::ParseError("Statement should be executed with query()".to_string())),
        }
    }

    pub fn query_statement(&self, statement: Statement) -> Result<QueryResult> {
        match statement {
            Statement::Select { table, columns, where_clause, order_by, limit } => {
                self.execute_select(&table, columns, where_clause, order_by, limit)
            }
            _ => Err(VelociError::ParseError("Statement is not a query".to_string())),
        }
    }

    pub fn begin_transaction(&self) -> Result<()> {
        let mut active = self.active_transaction.write();
        if active.is_some() {
            return Err(VelociError::TransactionError(
                "Transaction already in progress".to_string(),
            ));
        }
        let txn = self.transaction_manager.begin();
        *active = Some(txn);
        Ok(())
    }

    pub fn commit_transaction(&self) -> Result<()> {
        let mut active = self.active_transaction.write();
        match active.take() {
            Some(txn) => {
                self.transaction_manager.commit(&txn)?;
                self.lock_manager.release_all_locks(txn.id());
                Ok(())
            }
            None => Err(VelociError::TransactionError(
                "No active transaction to commit".to_string(),
            )),
        }
    }

    pub fn rollback_transaction(&self) -> Result<()> {
        // NOTE: Until WAL undo is wired in, rolling back an explicit
        // transaction only releases locks. Any storage mutations the
        // transaction performed remain on disk. The WAL milestone will close
        // this gap.
        let mut active = self.active_transaction.write();
        match active.take() {
            Some(txn) => {
                self.transaction_manager.abort(&txn)?;
                self.lock_manager.release_all_locks(txn.id());
                Ok(())
            }
            None => Err(VelociError::TransactionError(
                "No active transaction to rollback".to_string(),
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

    fn execute_alter_table(&self, table: &str, action: AlterAction) -> Result<()> {
        match action {
            AlterAction::RenameTable { new_name } => {
                {
                    let schema = self.schema.read();
                    if schema.get_table(&new_name).is_ok() {
                        return Err(VelociError::ConstraintViolation(format!(
                            "Table '{}' already exists",
                            new_name
                        )));
                    }
                }
                let mut schema = self.schema.write();
                let mut table_schema = schema.get_table(table)?.clone();
                schema.drop_table(table)?;
                table_schema.name = new_name.clone();
                schema.create_table(table_schema)?;
                drop(schema);

                let mut btrees = self.btrees.write();
                if let Some(bt) = btrees.remove(table) {
                    btrees.insert(new_name, bt);
                }
                Ok(())
            }
            AlterAction::RenameColumn { old_name, new_name } => {
                let mut schema = self.schema.write();
                let table_schema = schema.get_table_mut(table)?;
                if table_schema.columns.iter().any(|c| c.name == new_name) {
                    return Err(VelociError::ConstraintViolation(format!(
                        "Column '{}' already exists in table '{}'",
                        new_name, table
                    )));
                }
                let col = table_schema
                    .columns
                    .iter_mut()
                    .find(|c| c.name == old_name)
                    .ok_or_else(|| {
                        VelociError::NotFound(format!(
                            "Column '{}' not found in table '{}'",
                            old_name, table
                        ))
                    })?;
                col.name = new_name;
                Ok(())
            }
            AlterAction::AddColumn { column } => {
                {
                    let schema = self.schema.read();
                    let table_schema = schema.get_table(table)?;
                    if table_schema.columns.iter().any(|c| c.name == column.name) {
                        return Err(VelociError::ConstraintViolation(format!(
                            "Column '{}' already exists in table '{}'",
                            column.name, table
                        )));
                    }
                }

                // Rewrite existing rows with a trailing NULL so row width
                // matches the new schema.
                {
                    let btrees = self.btrees.read();
                    let btree_arc = btrees.get(table).ok_or_else(|| {
                        VelociError::NotFound(format!("Table '{}' not initialized", table))
                    })?;
                    let mut btree = btree_arc.write();
                    let all_rows = btree.scan()?;
                    for (key, mut row) in all_rows {
                        row.values.push(Value::Null);
                        btree.delete(key)?;
                        btree.insert(key, &row)?;
                    }
                }

                self.schema
                    .write()
                    .get_table_mut(table)?
                    .columns
                    .push(column);
                Ok(())
            }
            AlterAction::DropColumn { name } => {
                let col_index = {
                    let schema = self.schema.read();
                    let table_schema = schema.get_table(table)?;
                    let idx = table_schema
                        .columns
                        .iter()
                        .position(|c| c.name == name)
                        .ok_or_else(|| {
                            VelociError::NotFound(format!(
                                "Column '{}' not found in table '{}'",
                                name, table
                            ))
                        })?;
                    if table_schema.columns[idx].primary_key {
                        return Err(VelociError::ConstraintViolation(
                            "Cannot drop the primary key column".to_string(),
                        ));
                    }
                    idx
                };

                {
                    let btrees = self.btrees.read();
                    let btree_arc = btrees.get(table).ok_or_else(|| {
                        VelociError::NotFound(format!("Table '{}' not initialized", table))
                    })?;
                    let mut btree = btree_arc.write();
                    let all_rows = btree.scan()?;
                    for (key, mut row) in all_rows {
                        if col_index < row.values.len() {
                            row.values.remove(col_index);
                        }
                        btree.delete(key)?;
                        btree.insert(key, &row)?;
                    }
                }

                self.schema
                    .write()
                    .get_table_mut(table)?
                    .columns
                    .remove(col_index);
                Ok(())
            }
        }
    }

    fn execute_insert(
        &self,
        table: &str,
        columns: Option<Vec<String>>,
        values: Vec<Value>,
    ) -> Result<()> {
        // Check if we're in an explicit transaction; if not, auto-commit
        let explicit_txn = self.active_transaction.read().clone();
        let txn: Arc<Transaction>;
        let auto_commit: bool;

        if let Some(ref active) = explicit_txn {
            txn = Arc::clone(active);
            auto_commit = false;
        } else {
            txn = self.transaction_manager.begin();
            auto_commit = true;
        }

        if !auto_commit {
            self.lock_manager
                .acquire_lock(table, txn.id(), LockType::Exclusive)?;
        } else {
            self.lock_manager
                .acquire_lock(table, txn.id(), LockType::Exclusive)?;
        }

        // Get table schema and immediately clone to release lock
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released here

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
            self.lock_manager.release_lock(table, txn.id())?;
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

        // Validate NOT NULL and vector-dimension constraints before acquiring
        // the BTree lock.
        for (i, value) in values.iter().enumerate() {
            let col_name = &column_names[i];
            if let Some(col) = table_schema.columns.iter().find(|c| &c.name == col_name) {
                if col.not_null && matches!(value, Value::Null) {
                    self.lock_manager.release_lock(table, txn.id())?;
                    return Err(VelociError::ConstraintViolation(format!(
                        "Column '{}' cannot be NULL", col_name
                    )));
                }
                if let DataType::Vector(dim) = col.data_type {
                    match value {
                        Value::Null => {}
                        Value::Vector(v) if v.len() == dim as usize => {}
                        Value::Vector(v) => {
                            self.lock_manager.release_lock(table, txn.id())?;
                            return Err(VelociError::ConstraintViolation(format!(
                                "Column '{}' expects a vector of dimension {}, got {}",
                                col_name, dim, v.len()
                            )));
                        }
                        other => {
                            self.lock_manager.release_lock(table, txn.id())?;
                            return Err(VelociError::TypeMismatch {
                                expected: format!("Vector({})", dim),
                                actual: format!("{:?}", other),
                            });
                        }
                    }
                }
            }
        }

        // Create row data
        let mut row_values = vec![Value::Null; table_schema.columns.len()];
        for (i, col_name) in column_names.iter().enumerate() {
            if let Some(col_index) = table_schema.columns.iter().position(|c| &c.name == col_name) {
                row_values[col_index] = values[i].clone();
            }
        }
        let row = Row::new(row_values);

        // Single acquisition of btrees collection and btree with write intent
        // No lock upgrade, no multiple acquisitions
        let result = {
            let btrees = self.btrees.read();
            let btree_arc = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;

            // Acquire write lock directly (no upgrade from read)
            let mut btree = btree_arc.write();

            // Check for primary key uniqueness
            if btree.search(pk_value)?.is_some() {
                Err(VelociError::ConstraintViolation(format!(
                    "Primary key {} already exists in table '{}'",
                    pk_value, table
                )))
            } else {
                // Insert into B-Tree for persistence
                btree.insert(pk_value, &row)?;
                Ok(())
            }
        }; // btrees and btree locks released here

        // Handle result
        if let Err(e) = result {
            self.lock_manager.release_lock(table, txn.id())?;
            self.transaction_manager.abort(&txn)?;
            return Err(e);
        }

        self.cdc
            .record(table, ChangeOp::Insert, pk_value, None, Some(row));

        // Only commit/release lock for auto-commit mode
        if auto_commit {
            self.transaction_manager.commit(&txn)?;
            self.lock_manager.release_lock(table, txn.id())?;
        }

        Ok(())
    }

    fn execute_select(
        &self,
        table: &str,
        columns: Vec<String>,
        where_clause: Option<WhereClause>,
        order_by: Option<OrderBy>,
        limit: Option<u64>,
    ) -> Result<QueryResult> {
        let explicit_txn = self.active_transaction.read().clone();
        let txn: Arc<Transaction>;
        let auto_commit: bool;

        if let Some(ref active) = explicit_txn {
            txn = Arc::clone(active);
            auto_commit = false;
        } else {
            txn = self.transaction_manager.begin();
            auto_commit = true;
        }

        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Shared)?;

        // Get table schema and release lock immediately
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released

        // The B-tree is authoritative. MVCC overlay is intentionally not used
        // until WAL-backed snapshot isolation lands.
        let all_rows: Vec<(i64, Row)> = {
            let btrees = self.btrees.read();
            let btree_arc = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;
            let btree = btree_arc.read();
            btree.scan()?
        };

        // Process data without holding any locks. Filtering runs in parallel
        // (rayon) once the row count crosses PARALLEL_THRESHOLD.
        let mut filtered_rows: Vec<(i64, Row)> = if let Some(ref where_clause) = where_clause {
            if all_rows.len() >= PARALLEL_THRESHOLD {
                all_rows
                    .into_par_iter()
                    .filter(|(_, row)| {
                        self.evaluate_where_clause(row, where_clause, &table_schema)
                            .unwrap_or(false)
                    })
                    .collect()
            } else {
                all_rows
                    .into_iter()
                    .filter(|(_, row)| {
                        self.evaluate_where_clause(row, where_clause, &table_schema)
                            .unwrap_or(false)
                    })
                    .collect()
            }
        } else {
            all_rows
        };

        // Apply ORDER BY. Two forms are supported:
        // 1. A vector distance expression -> exact KNN (parallel distances).
        // 2. A plain column -> comparison sort (parallel for large sets).
        if let Some(ref order) = order_by {
            if let Some(parsed) = vector::parse_distance_expr(&order.column) {
                let expr = parsed?;
                let col_index = table_schema
                    .columns
                    .iter()
                    .position(|c| c.name == expr.column)
                    .ok_or_else(|| {
                        VelociError::NotFound(format!(
                            "Vector column '{}' not found in table '{}'",
                            expr.column, table
                        ))
                    })?;

                if order.ascending {
                    // Nearest-first with a LIMIT is the classic KNN shape:
                    // use top-k selection instead of a full sort.
                    let k = limit.map(|n| n as usize).unwrap_or(usize::MAX).min(filtered_rows.len());
                    filtered_rows = vector::knn(
                        filtered_rows,
                        col_index,
                        &expr.query,
                        expr.metric,
                        k,
                    )
                    .into_iter()
                    .map(|(_, key, row)| (key, row))
                    .collect();
                } else {
                    let distances = vector::compute_distances(
                        &filtered_rows,
                        col_index,
                        &expr.query,
                        expr.metric,
                    );
                    let mut scored: Vec<(f64, i64, Row)> = filtered_rows
                        .into_iter()
                        .zip(distances)
                        .map(|((key, row), d)| (d, key, row))
                        .collect();
                    scored.sort_by(|a, b| {
                        b.0.partial_cmp(&a.0)
                            .unwrap_or(std::cmp::Ordering::Equal)
                            .then(a.1.cmp(&b.1))
                    });
                    filtered_rows = scored.into_iter().map(|(_, key, row)| (key, row)).collect();
                }
            } else {
                let col_index = table_schema
                    .columns
                    .iter()
                    .position(|c| c.name == order.column)
                    .ok_or_else(|| {
                        VelociError::NotFound(format!(
                            "ORDER BY column '{}' not found in table '{}'",
                            order.column, table
                        ))
                    })?;

                let cmp = |(ak, a): &(i64, Row), (bk, b): &(i64, Row)| {
                    let av = a.values.get(col_index).unwrap_or(&Value::Null);
                    let bv = b.values.get(col_index).unwrap_or(&Value::Null);
                    let primary = compare_values(av, bv);
                    let secondary = ak.cmp(bk);
                    let combined = primary.then(secondary);
                    if order.ascending { combined } else { combined.reverse() }
                };

                if filtered_rows.len() >= PARALLEL_THRESHOLD {
                    filtered_rows.par_sort_by(cmp);
                } else {
                    filtered_rows.sort_by(cmp);
                }
            }
        }

        // Apply LIMIT n (does not affect COUNT(*), which counts rows after WHERE).
        if let Some(n) = limit {
            filtered_rows.truncate(n as usize);
        }

        // Check for aggregate functions (COUNT)
        let is_count = columns.len() == 1
            && columns[0].to_uppercase().starts_with("COUNT(");

        if is_count {
            let count = filtered_rows.len() as i64;
            let result_columns = vec![Column {
                name: columns[0].clone(),
                data_type: crate::types::DataType::Integer,
                primary_key: false,
                not_null: true,
                unique: false,
            }];
            let result_rows = vec![Row::new(vec![Value::Integer(count)])];

            if auto_commit {
                self.transaction_manager.commit(&txn)?;
                self.lock_manager.release_lock(table, txn.id())?;
            }

            return Ok(QueryResult::new(result_columns, result_rows));
        }

        // Project columns. Each projection item is either '*', a table
        // column, or a vector distance expression computed per row.
        enum ProjItem {
            Star,
            Col(usize),
            Distance {
                col_index: usize,
                metric: DistanceMetric,
                query: Vec<f32>,
            },
        }

        let mut proj_items: Vec<(String, ProjItem)> = Vec::with_capacity(columns.len());
        for col_name in &columns {
            if col_name == "*" {
                proj_items.push(("*".to_string(), ProjItem::Star));
            } else if let Some(idx) = table_schema.columns.iter().position(|c| &c.name == col_name)
            {
                proj_items.push((col_name.clone(), ProjItem::Col(idx)));
            } else if let Some(parsed) = vector::parse_distance_expr(col_name) {
                let expr = parsed?;
                let col_index = table_schema
                    .columns
                    .iter()
                    .position(|c| c.name == expr.column)
                    .ok_or_else(|| {
                        VelociError::NotFound(format!(
                            "Vector column '{}' not found in table '{}'",
                            expr.column, table
                        ))
                    })?;
                proj_items.push((
                    col_name.clone(),
                    ProjItem::Distance {
                        col_index,
                        metric: expr.metric,
                        query: expr.query,
                    },
                ));
            } else {
                return Err(VelociError::NotFound(format!(
                    "Column '{}' not found in table '{}'",
                    col_name, table
                )));
            }
        }

        let mut result_columns: Vec<Column> = Vec::new();
        for (name, item) in &proj_items {
            match item {
                ProjItem::Star => result_columns.extend(table_schema.columns.iter().cloned()),
                ProjItem::Col(idx) => result_columns.push(table_schema.columns[*idx].clone()),
                ProjItem::Distance { .. } => result_columns.push(Column {
                    name: name.clone(),
                    data_type: DataType::Real,
                    primary_key: false,
                    not_null: false,
                    unique: false,
                }),
            }
        }

        let result_rows: Vec<Row> = filtered_rows
            .into_iter()
            .map(|(_, row)| {
                let mut projected: Vec<Value> = Vec::with_capacity(result_columns.len());
                for (_, item) in &proj_items {
                    match item {
                        ProjItem::Star => projected.extend(row.values.iter().cloned()),
                        ProjItem::Col(idx) => {
                            projected.push(row.values.get(*idx).cloned().unwrap_or(Value::Null))
                        }
                        ProjItem::Distance {
                            col_index,
                            metric,
                            query,
                        } => {
                            let d = row
                                .values
                                .get(*col_index)
                                .and_then(|v| vector::value_as_vector(v).ok())
                                .and_then(|v| metric.distance(&v, query).ok());
                            projected.push(match d {
                                Some(d) => Value::Float(d),
                                None => Value::Null,
                            });
                        }
                    }
                }
                Row::new(projected)
            })
            .collect();

        if auto_commit {
            self.transaction_manager.commit(&txn)?;
            self.lock_manager.release_lock(table, txn.id())?;
        }

        Ok(QueryResult::new(result_columns, result_rows))
    }

    fn execute_update(
        &self,
        table: &str,
        assignments: HashMap<String, Value>,
        where_clause: Option<WhereClause>,
    ) -> Result<()> {
        let explicit_txn = self.active_transaction.read().clone();
        let txn: Arc<Transaction>;
        let auto_commit: bool;
        if let Some(ref active) = explicit_txn {
            txn = Arc::clone(active);
            auto_commit = false;
        } else {
            txn = self.transaction_manager.begin();
            auto_commit = true;
        }
        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Exclusive)?;

        // Get table schema and release lock
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released

        // Change events collected during the update, recorded to CDC only
        // after the whole statement succeeds.
        let mut cdc_events: Vec<(i64, Row, Row)> = Vec::new();

        // Perform update with single btree lock acquisition
        let result = {
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
                    .filter(|(_, row)| self.evaluate_where_clause(row, where_clause, &table_schema).unwrap_or(false))
                    .collect()
            } else {
                all_rows
            };

            // Find primary key column
            let pk_index = table_schema
                .columns
                .iter()
                .position(|c| c.primary_key)
                .ok_or_else(|| VelociError::ConstraintViolation("No primary key defined".to_string()))?;

            // Update each row
            for (key, row) in &rows_to_update {
                let mut updated_row = row.clone();
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

                if self.cdc.is_enabled() {
                    cdc_events.push((*key, row.clone(), updated_row));
                }
            }
            
            Ok::<(), VelociError>(())
        }; // btrees and btree locks released

        // Handle errors
        if let Err(e) = result {
            self.lock_manager.release_lock(table, txn.id())?;
            self.transaction_manager.abort(&txn)?;
            return Err(e);
        }

        for (key, before, after) in cdc_events {
            self.cdc
                .record(table, ChangeOp::Update, key, Some(before), Some(after));
        }

        if auto_commit {
            self.transaction_manager.commit(&txn)?;
            self.lock_manager.release_lock(table, txn.id())?;
        }

        Ok(())
    }

    #[allow(clippy::let_and_return)]
    fn execute_delete(&self, table: &str, where_clause: Option<WhereClause>) -> Result<()> {
        let explicit_txn = self.active_transaction.read().clone();
        let txn: Arc<Transaction>;
        let auto_commit: bool;
        if let Some(ref active) = explicit_txn {
            txn = Arc::clone(active);
            auto_commit = false;
        } else {
            txn = self.transaction_manager.begin();
            auto_commit = true;
        }
        self.lock_manager
            .acquire_lock(table, txn.id(), LockType::Exclusive)?;

        // Get table schema and release lock
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released

        // Perform delete with single btree lock acquisition
        let mut cdc_events: Vec<(i64, Row)> = Vec::new();
        let result: Result<()> = {
            let btrees = self.btrees.read();
            let btree_arc = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;
            let mut btree = btree_arc.write();

            let all_rows = btree.scan()?;
            let rows_to_delete: Vec<(i64, Row)> = if let Some(ref where_clause) = where_clause {
                all_rows
                    .into_iter()
                    .filter(|(_, row)| self.evaluate_where_clause(row, where_clause, &table_schema).unwrap_or(false))
                    .collect()
            } else {
                all_rows
            };

            for (key, row) in rows_to_delete {
                btree.delete(key)?;
                if self.cdc.is_enabled() {
                    cdc_events.push((key, row));
                }
            }
            Ok(())
        };

        if let Err(e) = result {
            self.lock_manager.release_lock(table, txn.id())?;
            self.transaction_manager.abort(&txn)?;
            return Err(e);
        }

        for (key, before) in cdc_events {
            self.cdc
                .record(table, ChangeOp::Delete, key, Some(before), None);
        }

        if auto_commit {
            self.transaction_manager.commit(&txn)?;
            self.lock_manager.release_lock(table, txn.id())?;
        }

        Ok(())
    }

    fn evaluate_where_clause(
        &self,
        row: &Row,
        where_clause: &WhereClause,
        table_schema: &TableSchema,
    ) -> Result<bool> {
        // (helper) See `compare_values` below for sort ordering semantics.
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

/// Order two `Value`s for ORDER BY purposes.
///
/// SQL NULL semantics here: NULL sorts after all non-NULL values (the SQLite
/// default for ASC and the conventional "NULLS LAST" behaviour).
fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        (Value::Integer(x), Value::Integer(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.cmp(y),
        (Value::Blob(x), Value::Blob(y)) => x.cmp(y),
        // Mixed numeric: promote to f64.
        (lhs, rhs)
            if matches!(lhs, Value::Float(_) | Value::Real(_) | Value::Integer(_))
                && matches!(rhs, Value::Float(_) | Value::Real(_) | Value::Integer(_)) =>
        {
            let lf = lhs.as_float().unwrap_or(0.0);
            let rf = rhs.as_float().unwrap_or(0.0);
            lf.partial_cmp(&rf).unwrap_or(Ordering::Equal)
        }
        // Different incomparable types: fall back to discriminant order so that
        // the sort is at least total and deterministic.
        _ => format!("{:?}", a).cmp(&format!("{:?}", b)),
    }
}

#[cfg(test)]
mod tests {
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


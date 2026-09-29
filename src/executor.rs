//! Query executor that translates AST statements into storage operations.
//!
//! Coordinates B-Tree, MVCC, lock manager, and schema to execute SQL
//! statements with ACID guarantees. WHERE filtering, ORDER BY sorting, and
//! vector distance computation are parallelized with rayon for larger row
//! sets.

use crate::btree::BTree;
use crate::cdc::{CdcManager, ChangeOp};
use crate::index::{self, IndexSchema};
use crate::parser::{AlterAction, Operator, OrderBy, Statement, WhereClause};
use crate::storage::{Pager, Schema, TableSchema};
use crate::transaction::{LockManager, LockType, Transaction, TransactionManager};
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
    /// Secondary index B-trees, keyed by index name.
    indexes: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
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
        indexes: Arc<RwLock<HashMap<String, Arc<RwLock<BTree>>>>>,
        schema: Arc<RwLock<Schema>>,
        transaction_manager: Arc<TransactionManager>,
        cdc: Arc<CdcManager>,
    ) -> Self {
        Self {
            pager,
            btrees,
            indexes,
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
            Statement::CreateIndex {
                name,
                table,
                column,
                if_not_exists,
            } => self.with_table_lock(&table, LockType::Exclusive, || {
                self.execute_create_index(&name, &table, &column, if_not_exists)
            }),
            Statement::DropIndex { name, if_exists } => {
                let table = self
                    .schema
                    .read()
                    .table_of_index(&name)
                    .map(|t| t.name.clone());
                match table {
                    Some(table) => self.with_table_lock(&table, LockType::Exclusive, || {
                        self.execute_drop_index(&table, &name)
                    }),
                    None if if_exists => Ok(()),
                    None => Err(VelociError::NotFound(format!("Index '{}' not found", name))),
                }
            }
            Statement::AlterTable { table, action } => self.execute_alter_table(&table, action),
            Statement::Insert {
                table,
                columns,
                values,
            } => self.with_table_lock(&table, LockType::Exclusive, || {
                self.execute_insert(&table, columns, values)
            }),
            Statement::Update {
                table,
                assignments,
                where_clause,
            } => self.with_table_lock(&table, LockType::Exclusive, || {
                self.execute_update(&table, assignments, where_clause)
            }),
            Statement::Delete {
                table,
                where_clause,
            } => self.with_table_lock(&table, LockType::Exclusive, || {
                self.execute_delete(&table, where_clause)
            }),
            Statement::BeginTransaction => self.begin_transaction(),
            Statement::CommitTransaction => self.commit_transaction(),
            Statement::RollbackTransaction => self.rollback_transaction(),
            _ => Err(VelociError::ParseError(
                "Statement should be executed with query()".to_string(),
            )),
        }
    }

    pub fn query_statement(&self, statement: Statement) -> Result<QueryResult> {
        match statement {
            Statement::Select {
                table,
                columns,
                where_clause,
                order_by,
                limit,
            } => self.with_table_lock(&table, LockType::Shared, || {
                self.execute_select(&table, columns, where_clause, order_by, limit)
            }),
            _ => Err(VelociError::ParseError(
                "Statement is not a query".to_string(),
            )),
        }
    }

    /// Runs `f` holding a `lock` on `table`.
    ///
    /// Inside an explicit transaction the lock is taken on behalf of that
    /// transaction and held until COMMIT / ROLLBACK, even if `f` fails (the
    /// caller undoes the failed statement's storage writes). Otherwise a
    /// single-statement transaction is started and always ended — committed
    /// on success, aborted on error — with its lock released on every path.
    fn with_table_lock<T>(
        &self,
        table: &str,
        lock: LockType,
        f: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if let Some(txn) = self.active_transaction.read().clone() {
            self.lock_manager.acquire_lock(table, txn.id(), lock)?;
            return f();
        }

        let txn = self.transaction_manager.begin();
        if let Err(e) = self.lock_manager.acquire_lock(table, txn.id(), lock) {
            let _ = self.transaction_manager.abort(&txn);
            return Err(e);
        }
        let result = f();
        let ended = match result {
            Ok(_) => self.transaction_manager.commit(&txn),
            Err(_) => self.transaction_manager.abort(&txn),
        };
        self.lock_manager.release_all_locks(txn.id());
        let value = result?;
        ended?;
        Ok(value)
    }

    /// Whether an explicit transaction is in progress.
    pub fn in_transaction(&self) -> bool {
        self.active_transaction.read().is_some()
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
        // Storage is undone by the caller (`Database::rollback` aborts the
        // transaction's WAL group); this only ends the transaction and
        // releases its locks.
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
            indexes: Vec::new(),
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
        let index_names: Vec<String> = {
            let schema = self.schema.read();
            schema
                .get_table(name)?
                .indexes
                .iter()
                .map(|i| i.name.clone())
                .collect()
        };
        self.schema.write().drop_table(name)?;
        self.btrees.write().remove(name);
        let mut indexes = self.indexes.write();
        for index_name in index_names {
            indexes.remove(&index_name);
        }
        Ok(())
    }

    fn execute_create_index(
        &self,
        name: &str,
        table: &str,
        column: &str,
        if_not_exists: bool,
    ) -> Result<()> {
        let (table_schema, exists) = {
            let schema = self.schema.read();
            let exists = schema.table_of_index(name).is_some();
            (schema.get_table(table)?.clone(), exists)
        };
        if exists {
            return if if_not_exists {
                Ok(())
            } else {
                Err(VelociError::ConstraintViolation(format!(
                    "Index '{}' already exists",
                    name
                )))
            };
        }
        let col_index = column_index(&table_schema, column)?;

        // Build the index from the table's current rows.
        let mut index_tree = BTree::new(Arc::clone(&self.pager))?;
        {
            let btrees = self.btrees.read();
            let table_tree = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;
            let rows = table_tree.read().scan()?;
            for (pk, row) in rows {
                if let Some(value) = row.values.get(col_index) {
                    index::insert_entry(&mut index_tree, value, pk)?;
                }
            }
        }

        let root_page = index_tree.root_page();
        self.indexes
            .write()
            .insert(name.to_string(), Arc::new(RwLock::new(index_tree)));
        self.schema
            .write()
            .get_table_mut(table)?
            .indexes
            .push(IndexSchema {
                name: name.to_string(),
                column: column.to_string(),
                root_page,
            });
        Ok(())
    }

    fn execute_drop_index(&self, table: &str, name: &str) -> Result<()> {
        self.schema
            .write()
            .get_table_mut(table)?
            .indexes
            .retain(|i| i.name != name);
        self.indexes.write().remove(name);
        Ok(())
    }

    /// Handles for `table`'s indexes, each with the position of its column.
    /// Taken before any B-tree lock; lock order is table tree, then indexes.
    fn table_indexes(&self, table: &TableSchema) -> Result<Vec<TableIndex>> {
        let handles = self.indexes.read();
        table
            .indexes
            .iter()
            .map(|i| {
                let tree = handles.get(&i.name).cloned().ok_or_else(|| {
                    VelociError::Corruption(format!("No B-tree for index '{}'", i.name))
                })?;
                Ok(TableIndex {
                    column: column_index(table, &i.column)?,
                    column_name: i.column.clone(),
                    tree,
                })
            })
            .collect()
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
                col.name = new_name.clone();
                for index in table_schema.indexes.iter_mut() {
                    if index.column == old_name {
                        index.column = new_name.clone();
                    }
                }
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
                    if let Some(index) = table_schema.index_on(&name) {
                        return Err(VelociError::ConstraintViolation(format!(
                            "Cannot drop column '{}': index '{}' uses it (DROP INDEX first)",
                            name, index.name
                        )));
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
            return Err(VelociError::ConstraintViolation(
                "Column count doesn't match value count".to_string(),
            ));
        }

        // Find primary key
        let pk_index = table_schema
            .columns
            .iter()
            .position(|c| c.primary_key)
            .ok_or_else(|| {
                VelociError::ConstraintViolation("No primary key defined".to_string())
            })?;

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
                    return Err(VelociError::ConstraintViolation(format!(
                        "Column '{}' cannot be NULL",
                        col_name
                    )));
                }
                if let DataType::Vector(dim) = col.data_type {
                    match value {
                        Value::Null => {}
                        Value::Vector(v) if v.len() == dim as usize => {}
                        Value::Vector(v) => {
                            return Err(VelociError::ConstraintViolation(format!(
                                "Column '{}' expects a vector of dimension {}, got {}",
                                col_name,
                                dim,
                                v.len()
                            )));
                        }
                        other => {
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
            if let Some(col_index) = table_schema
                .columns
                .iter()
                .position(|c| &c.name == col_name)
            {
                row_values[col_index] = values[i].clone();
            }
        }
        let row = Row::new(row_values);
        let indexes = self.table_indexes(&table_schema)?;

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
            } else if let Err(e) = check_insert_unique(&table_schema, &btree, &indexes, &row) {
                Err(e)
            } else {
                // Insert into B-Tree for persistence
                btree.insert(pk_value, &row)?;
                for idx in &indexes {
                    index::insert_entry(&mut idx.tree.write(), &row.values[idx.column], pk_value)?;
                }
                Ok(())
            }
        }; // btrees and btree locks released here

        // Handle result
        result?;

        self.cdc
            .stage(table, ChangeOp::Insert, pk_value, None, Some(row));

        // Only commit/release lock for auto-commit mode

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
        // Get table schema and release lock immediately
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released

        let indexes = self.table_indexes(&table_schema)?;

        // The B-tree is authoritative. MVCC overlay is intentionally not used
        // until WAL-backed snapshot isolation lands.
        let all_rows: Vec<(i64, Row)> = {
            let btrees = self.btrees.read();
            let btree_arc = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;
            let btree = btree_arc.read();
            candidate_rows(&btree, where_clause.as_ref(), &table_schema, &indexes)?
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
                    let k = limit
                        .map(|n| n as usize)
                        .unwrap_or(usize::MAX)
                        .min(filtered_rows.len());
                    filtered_rows =
                        vector::knn(filtered_rows, col_index, &expr.query, expr.metric, k)
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
                    if order.ascending {
                        combined
                    } else {
                        combined.reverse()
                    }
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
        let is_count = columns.len() == 1 && columns[0].to_uppercase().starts_with("COUNT(");

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
            } else if let Some(idx) = table_schema
                .columns
                .iter()
                .position(|c| &c.name == col_name)
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

        Ok(QueryResult::new(result_columns, result_rows))
    }

    fn execute_update(
        &self,
        table: &str,
        assignments: HashMap<String, Value>,
        where_clause: Option<WhereClause>,
    ) -> Result<()> {
        // Get table schema and release lock
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released

        let indexes = self.table_indexes(&table_schema)?;

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

            let all_rows = candidate_rows(&btree, where_clause.as_ref(), &table_schema, &indexes)?;

            // Find rows to update
            let rows_to_update: Vec<(i64, Row)> = if let Some(ref where_clause) = where_clause {
                all_rows
                    .into_iter()
                    .filter(|(_, row)| {
                        self.evaluate_where_clause(row, where_clause, &table_schema)
                            .unwrap_or(false)
                    })
                    .collect()
            } else {
                all_rows
            };

            // Find primary key column
            let pk_index = table_schema
                .columns
                .iter()
                .position(|c| c.primary_key)
                .ok_or_else(|| {
                    VelociError::ConstraintViolation("No primary key defined".to_string())
                })?;

            // Compute every updated row first so UNIQUE can be checked
            // against the table's final state before anything is mutated.
            let mut updates: Vec<(i64, i64, Row, Row)> = Vec::with_capacity(rows_to_update.len());
            for (key, row) in &rows_to_update {
                let mut updated_row = row.clone();
                let mut new_pk_value = *key; // Default to existing key
                let mut pk_being_updated = false;

                // Apply updates to the row
                for (col_name, new_value) in &assignments {
                    if let Some(col_index) = table_schema
                        .columns
                        .iter()
                        .position(|c| &c.name == col_name)
                    {
                        // Check NOT NULL constraint
                        let col = &table_schema.columns[col_index];
                        if col.not_null && matches!(new_value, Value::Null) {
                            return Err(VelociError::ConstraintViolation(format!(
                                "Column '{}' cannot be NULL",
                                col_name
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
                if pk_being_updated && new_pk_value != *key && btree.search(new_pk_value)?.is_some()
                {
                    return Err(VelociError::ConstraintViolation(format!(
                        "Primary key {} already exists in table '{}'",
                        new_pk_value, table
                    )));
                }

                updates.push((*key, new_pk_value, row.clone(), updated_row));
            }

            let touched_unique: Vec<usize> = unique_columns(&table_schema)
                .into_iter()
                .filter(|&ci| assignments.contains_key(&table_schema.columns[ci].name))
                .collect();
            if !touched_unique.is_empty() && !updates.is_empty() {
                let updated_keys: std::collections::HashSet<i64> =
                    updates.iter().map(|(key, ..)| *key).collect();
                let untouched = btree.scan()?;
                let final_rows: Vec<&Row> = untouched
                    .iter()
                    .filter(|(key, _)| !updated_keys.contains(key))
                    .map(|(_, row)| row)
                    .chain(updates.iter().map(|(_, _, _, updated)| updated))
                    .collect();
                check_unique(&table_schema, &touched_unique, &final_rows)?;
            }

            // Delete old row and insert updated row
            for (key, new_pk_value, row, updated_row) in updates {
                btree.delete(key)?;
                btree.insert(new_pk_value, &updated_row)?;
                for idx in &indexes {
                    let (old, new) = (&row.values[idx.column], &updated_row.values[idx.column]);
                    if key != new_pk_value || old != new {
                        let mut tree = idx.tree.write();
                        index::remove_entry(&mut tree, old, key)?;
                        index::insert_entry(&mut tree, new, new_pk_value)?;
                    }
                }

                if self.cdc.is_enabled() {
                    cdc_events.push((key, row, updated_row));
                }
            }

            Ok::<(), VelociError>(())
        }; // btrees and btree locks released

        // Handle errors
        result?;

        for (key, before, after) in cdc_events {
            self.cdc
                .stage(table, ChangeOp::Update, key, Some(before), Some(after));
        }

        Ok(())
    }

    #[allow(clippy::let_and_return)]
    fn execute_delete(&self, table: &str, where_clause: Option<WhereClause>) -> Result<()> {
        // Get table schema and release lock
        let table_schema = {
            let schema = self.schema.read();
            schema.get_table(table)?.clone()
        }; // schema lock released

        let indexes = self.table_indexes(&table_schema)?;

        // Perform delete with single btree lock acquisition
        let mut cdc_events: Vec<(i64, Row)> = Vec::new();
        let result: Result<()> = {
            let btrees = self.btrees.read();
            let btree_arc = btrees.get(table).ok_or_else(|| {
                VelociError::NotFound(format!("Table '{}' not initialized", table))
            })?;
            let mut btree = btree_arc.write();

            let all_rows = candidate_rows(&btree, where_clause.as_ref(), &table_schema, &indexes)?;
            let rows_to_delete: Vec<(i64, Row)> = if let Some(ref where_clause) = where_clause {
                all_rows
                    .into_iter()
                    .filter(|(_, row)| {
                        self.evaluate_where_clause(row, where_clause, &table_schema)
                            .unwrap_or(false)
                    })
                    .collect()
            } else {
                all_rows
            };

            for (key, row) in rows_to_delete {
                btree.delete(key)?;
                for idx in &indexes {
                    index::remove_entry(&mut idx.tree.write(), &row.values[idx.column], key)?;
                }
                if self.cdc.is_enabled() {
                    cdc_events.push((key, row));
                }
            }
            Ok(())
        };

        result?;

        for (key, before) in cdc_events {
            self.cdc
                .stage(table, ChangeOp::Delete, key, Some(before), None);
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

/// A secondary index of the table being accessed.
struct TableIndex {
    /// Position of the indexed column in the table's rows.
    column: usize,
    column_name: String,
    tree: Arc<RwLock<BTree>>,
}

fn column_index(table: &TableSchema, column: &str) -> Result<usize> {
    table
        .columns
        .iter()
        .position(|c| c.name == column)
        .ok_or_else(|| {
            VelociError::NotFound(format!(
                "Column '{}' not found in table '{}'",
                column, table.name
            ))
        })
}

/// Rows that can possibly match `where_clause`, in primary-key order:
/// - `<pk> = <integer>`: a B-tree lookup;
/// - `<indexed column> = <value>`: an index probe, then a lookup per match;
/// - otherwise every row.
///
/// Callers still evaluate the full clause on the result.
fn candidate_rows(
    btree: &BTree,
    where_clause: Option<&WhereClause>,
    table: &TableSchema,
    indexes: &[TableIndex],
) -> Result<Vec<(i64, Row)>> {
    let pk_name = table
        .columns
        .iter()
        .find(|c| c.primary_key)
        .map(|c| c.name.as_str());
    let pk_key = where_clause.zip(pk_name).and_then(|(wc, pk)| {
        wc.conditions
            .iter()
            .find_map(|c| match (&c.operator, &c.value) {
                (Operator::Equal, Value::Integer(key)) if c.column == pk => Some(*key),
                _ => None,
            })
    });
    if let Some(key) = pk_key {
        return Ok(btree
            .search(key)?
            .map(|row| (key, row))
            .into_iter()
            .collect());
    }

    for condition in where_clause.map_or(&[][..], |wc| &wc.conditions) {
        if condition.operator != Operator::Equal {
            continue;
        }
        let Some(idx) = indexes.iter().find(|i| i.column_name == condition.column) else {
            continue;
        };
        if let Some(pks) = index::probe(&idx.tree.read(), &condition.value)? {
            let mut rows = Vec::with_capacity(pks.len());
            for pk in pks {
                match btree.search(pk)? {
                    Some(row) => rows.push((pk, row)),
                    None => {
                        return Err(VelociError::Corruption(format!(
                            "Index on {}.{} points at missing row {}",
                            table.name, condition.column, pk
                        )))
                    }
                }
            }
            return Ok(rows);
        }
    }

    btree.scan()
}

/// Indices of `UNIQUE` columns other than the primary key (the B-tree
/// enforces primary-key uniqueness itself).
fn unique_columns(table: &TableSchema) -> Vec<usize> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(_, c)| c.unique && !c.primary_key)
        .map(|(i, _)| i)
        .collect()
}

fn unique_violation(table: &TableSchema, col_index: usize, value: &Value) -> VelociError {
    VelociError::ConstraintViolation(format!(
        "UNIQUE constraint failed: {}.{} (duplicate value {})",
        table.name, table.columns[col_index].name, value
    ))
}

/// Rejects `row` if a UNIQUE column's non-NULL value already exists in the
/// table. Uses an index on the column when there is one; otherwise scans
/// the table (only when it has UNIQUE columns).
fn check_insert_unique(
    table: &TableSchema,
    btree: &BTree,
    indexes: &[TableIndex],
    row: &Row,
) -> Result<()> {
    let mut cols: Vec<usize> = Vec::new();
    for ci in unique_columns(table) {
        let value = match row.values.get(ci) {
            None | Some(Value::Null) => continue,
            Some(v) => v,
        };
        let probed = match indexes.iter().find(|i| i.column == ci) {
            Some(idx) => index::probe(&idx.tree.read(), value)?,
            None => None,
        };
        match probed {
            Some(pks) => {
                for pk in pks {
                    if let Some(existing) = btree.search(pk)? {
                        let v = &existing.values[ci];
                        if compare_values(v, value) == std::cmp::Ordering::Equal {
                            return Err(unique_violation(table, ci, v));
                        }
                    }
                }
            }
            // No index, or a value the index cannot look up: scan.
            None => cols.push(ci),
        }
    }
    if cols.is_empty() {
        return Ok(());
    }
    for (_, existing) in btree.scan()? {
        for &ci in &cols {
            if let Some(v) = existing.values.get(ci) {
                if !matches!(v, Value::Null)
                    && compare_values(v, &row.values[ci]) == std::cmp::Ordering::Equal
                {
                    return Err(unique_violation(table, ci, v));
                }
            }
        }
    }
    Ok(())
}

/// Rejects `rows` (a table's complete final contents) if any column in
/// `cols` holds the same non-NULL value twice. NULLs never conflict.
fn check_unique(table: &TableSchema, cols: &[usize], rows: &[&Row]) -> Result<()> {
    for &ci in cols {
        let mut values: Vec<&Value> = rows
            .iter()
            .filter_map(|r| r.values.get(ci))
            .filter(|v| !matches!(v, Value::Null))
            .collect();
        values.sort_by(|a, b| compare_values(a, b));
        if let Some(pair) = values
            .windows(2)
            .find(|w| compare_values(w[0], w[1]) == std::cmp::Ordering::Equal)
        {
            return Err(unique_violation(table, ci, pair[0]));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::Parser;
    use crate::storage::Database;
    use tempfile::NamedTempFile;

    /// The index must narrow the candidate rows, not just return correct
    /// results (a silent fallback to a scan would too).
    #[test]
    fn test_index_probe_narrows_candidates() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let exec = Executor::new(
            Arc::clone(&pager),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(HashMap::new())),
            Arc::new(RwLock::new(Schema::new())),
            Arc::new(TransactionManager::new()),
            Arc::new(CdcManager::new()),
        );
        pager.write().begin_group().unwrap();
        let run = |sql: &str| {
            exec.execute_statement(Parser::new().parse(sql).unwrap())
                .unwrap()
        };
        run("CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER, w TEXT)");
        for i in 0..200 {
            run(&format!(
                "INSERT INTO t VALUES ({}, {}, 'w{}')",
                i,
                i % 10,
                i
            ));
        }
        run("CREATE INDEX t_v ON t (v)");

        let candidates = |sql: &str| {
            let Statement::Select { where_clause, .. } = Parser::new().parse(sql).unwrap() else {
                unreachable!()
            };
            let table = exec.schema.read().get_table("t").unwrap().clone();
            let indexes = exec.table_indexes(&table).unwrap();
            let btrees = exec.btrees.read();
            let tree = btrees.get("t").unwrap().read();
            candidate_rows(&tree, where_clause.as_ref(), &table, &indexes)
                .unwrap()
                .into_iter()
                .map(|(pk, _)| pk)
                .collect::<Vec<i64>>()
        };
        assert_eq!(
            candidates("SELECT * FROM t WHERE v = 3"),
            (0..20).map(|i| i * 10 + 3).collect::<Vec<_>>()
        );
        assert_eq!(
            candidates("SELECT * FROM t WHERE w = 'w5' AND v = 5").len(),
            20
        );
        assert_eq!(
            candidates("SELECT * FROM t WHERE v = 3 AND id = 13"),
            vec![13]
        );
        // Unindexed column, non-equality, or unprobeable value: full scan.
        assert_eq!(candidates("SELECT * FROM t WHERE w = 'w5'").len(), 200);
        assert_eq!(candidates("SELECT * FROM t WHERE v > 3").len(), 200);
        assert_eq!(candidates("SELECT * FROM t WHERE v = NULL").len(), 200);
    }

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

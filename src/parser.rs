// SQL Parser

use crate::types::{Column, DataType, Result, Value, VelociError};
use regex::Regex;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable {
        name: String,
        columns: Vec<Column>,
    },
    DropTable {
        name: String,
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    pub conditions: Vec<Condition>,
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
        match (self, left, right) {
            (Operator::Equal, Value::Integer(a), Value::Integer(b)) => Ok(a == b),
            (Operator::NotEqual, Value::Integer(a), Value::Integer(b)) => Ok(a != b),
            (Operator::GreaterThan, Value::Integer(a), Value::Integer(b)) => Ok(a > b),
            (Operator::LessThan, Value::Integer(a), Value::Integer(b)) => Ok(a < b),
            (Operator::GreaterThanOrEqual, Value::Integer(a), Value::Integer(b)) => Ok(a >= b),
            (Operator::LessThanOrEqual, Value::Integer(a), Value::Integer(b)) => Ok(a <= b),
            
            (Operator::Equal, Value::Text(a), Value::Text(b)) => Ok(a == b),
            (Operator::NotEqual, Value::Text(a), Value::Text(b)) => Ok(a != b),
            (Operator::Like, Value::Text(a), Value::Text(pattern)) => {
                let regex_pattern = pattern
                    .replace("%", ".*")
                    .replace("_", ".");
                let regex = Regex::new(&format!("^{}$", regex_pattern))
                    .map_err(|e| VelociError::ParseError(format!("Invalid LIKE pattern: {}", e)))?;
                Ok(regex.is_match(a))
            }
            
            (Operator::Equal, Value::Null, Value::Null) => Ok(true),
            (Operator::NotEqual, Value::Null, _) | (Operator::NotEqual, _, Value::Null) => Ok(true),
            _ => Err(VelociError::TypeMismatch {
                expected: format!("{:?}", right),
                actual: format!("{:?}", left),
            }),
        }
    }
}

pub struct Parser {
    // Parser state can be added here if needed
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
        } else if upper.starts_with("INSERT INTO") {
            self.parse_insert(sql)
        } else if upper.starts_with("SELECT") {
            self.parse_select(sql)
        } else if upper.starts_with("UPDATE") {
            self.parse_update(sql)
        } else if upper.starts_with("DELETE FROM") {
            self.parse_delete(sql)
        } else {
            Err(VelociError::ParseError(format!(
                "Unsupported statement: {}",
                sql
            )))
        }
    }

    fn parse_create_table(&self, sql: &str) -> Result<Statement> {
        // CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)
        let re = Regex::new(r"(?i)CREATE\s+TABLE\s+(\w+)\s*\((.+)\)")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid CREATE TABLE syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();
        let columns_str = captures.get(2).unwrap().as_str();

        let mut columns = Vec::new();
        for col_def in columns_str.split(',') {
            let parts: Vec<&str> = col_def.trim().split_whitespace().collect();
            if parts.len() < 2 {
                return Err(VelociError::ParseError(format!(
                    "Invalid column definition: {}",
                    col_def
                )));
            }

            let col_name = parts[0].to_string();
            let data_type = DataType::from_str(parts[1]);
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
        let re = Regex::new(r"(?i)DROP\s+TABLE\s+(\w+)")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid DROP TABLE syntax".to_string()))?;

        let table_name = captures.get(1).unwrap().as_str().to_string();

        Ok(Statement::DropTable { name: table_name })
    }

    fn parse_insert(&self, sql: &str) -> Result<Statement> {
        // INSERT INTO users (id, name, age) VALUES (1, 'Alice', 30)
        // INSERT INTO users VALUES (1, 'Alice', 30)
        
        let re = Regex::new(r"(?i)INSERT\s+INTO\s+(\w+)(?:\s*\(([^)]+)\))?\s+VALUES\s*\(([^)]+)\)")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

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
        
        let re = Regex::new(r"(?i)SELECT\s+(.+?)\s+FROM\s+(\w+)(?:\s+WHERE\s+(.+))?")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

        let captures = re
            .captures(sql)
            .ok_or_else(|| VelociError::ParseError("Invalid SELECT syntax".to_string()))?;

        let columns_str = captures.get(1).unwrap().as_str().trim();
        let columns = if columns_str == "*" {
            vec!["*".to_string()]
        } else {
            columns_str
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
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
        })
    }

    fn parse_update(&self, sql: &str) -> Result<Statement> {
        // UPDATE users SET age = 31 WHERE name = 'Alice'
        
        let re = Regex::new(r"(?i)UPDATE\s+(\w+)\s+SET\s+(.+?)(?:\s+WHERE\s+(.+))?$")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

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
        
        let re = Regex::new(r"(?i)DELETE\s+FROM\s+(\w+)(?:\s+WHERE\s+(.+))?")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

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
        // Simple WHERE clause parser - supports single conditions for now
        // age > 25
        // name = 'Alice'
        
        let re = Regex::new(r"(\w+)\s*(=|!=|<>|>|<|>=|<=|LIKE)\s*(.+)")
            .map_err(|e| VelociError::ParseError(format!("Regex error: {}", e)))?;

        let captures = re
            .captures(clause)
            .ok_or_else(|| VelociError::ParseError(format!("Invalid WHERE clause: {}", clause)))?;

        let column = captures.get(1).unwrap().as_str().to_string();
        let operator_str = captures.get(2).unwrap().as_str();
        let operator = Operator::from_str(operator_str)?;
        let value = self.parse_value(captures.get(3).unwrap().as_str())?;

        Ok(WhereClause {
            conditions: vec![Condition {
                column,
                operator,
                value,
            }],
        })
    }

    fn parse_values(&self, values_str: &str) -> Result<Vec<Value>> {
        values_str
            .split(',')
            .map(|s| self.parse_value(s.trim()))
            .collect()
    }

    fn parse_value(&self, s: &str) -> Result<Value> {
        let s = s.trim();

        // NULL
        if s.to_uppercase() == "NULL" {
            return Ok(Value::Null);
        }

        // String (quoted)
        if (s.starts_with('\'') && s.ends_with('\''))
            || (s.starts_with('"') && s.ends_with('"'))
        {
            let text = s[1..s.len() - 1].to_string();
            return Ok(Value::Text(text));
        }

        // Try integer
        if let Ok(i) = s.parse::<i64>() {
            return Ok(Value::Integer(i));
        }

        // Try float
        if let Ok(f) = s.parse::<f64>() {
            return Ok(Value::Float(f));
        }

        // Default to text without quotes
        Ok(Value::Text(s.to_string()))
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
            } => {
                assert_eq!(table, "users");
                assert_eq!(columns, vec!["*"]);
                assert!(where_clause.is_some());
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


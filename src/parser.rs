//! SQL parser supporting DDL (CREATE TABLE, DROP TABLE, ALTER TABLE), DML
//! (INSERT, UPDATE, DELETE), and DQL (SELECT with WHERE, COUNT(\*)
//! aggregation, and vector distance expressions).
//!
//! Parses SQL text into an AST of [`Statement`] variants consumed by the executor.

mod lexer;

use crate::types::{Column, DataType, Result, Value, VelociError};
use lexer::{Token, TokenKind};
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
            (op, left, right)
                if matches!(
                    (left, right),
                    (
                        Value::Float(_) | Value::Real(_) | Value::Integer(_),
                        Value::Float(_) | Value::Real(_) | Value::Integer(_)
                    )
                ) && (!matches!(left, Value::Integer(_))
                    || !matches!(right, Value::Integer(_))) =>
            {
                let a = left.as_float().map_err(|_| VelociError::TypeMismatch {
                    expected: "numeric".to_string(),
                    actual: format!("{:?}", left),
                })?;
                let b = right.as_float().map_err(|_| VelociError::TypeMismatch {
                    expected: "numeric".to_string(),
                    actual: format!("{:?}", right),
                })?;
                match op {
                    Operator::Equal => Ok(a == b),
                    Operator::NotEqual => Ok(a != b),
                    Operator::GreaterThan => Ok(a > b),
                    Operator::LessThan => Ok(a < b),
                    Operator::GreaterThanOrEqual => Ok(a >= b),
                    Operator::LessThanOrEqual => Ok(a <= b),
                    Operator::Like => Err(VelociError::ParseError(
                        "LIKE not supported for numeric types".to_string(),
                    )),
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
                        '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^'
                        | '$' | '\\' => {
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

/// Recursive-descent SQL parser over the tokens produced by [`lexer`].
///
/// Grammar (keywords case-insensitive, one optional trailing `;`):
///
/// ```text
/// CREATE TABLE name ( coldef [, coldef]* )
///     coldef   := ident type [PRIMARY KEY | NOT NULL | NULL | UNIQUE]*
///     type     := word [ ( number [, number] ) ]
/// DROP TABLE name
/// ALTER TABLE name RENAME TO name
///                | RENAME [COLUMN] ident TO ident
///                | ADD [COLUMN] coldef
///                | DROP [COLUMN] ident
/// INSERT INTO name [ ( ident [, ident]* ) ] VALUES ( value [, value]* )
/// SELECT item [, item]* FROM name [WHERE cond [AND cond]*]
///     [ORDER BY item [ASC | DESC]] [LIMIT integer]
///     item     := * | ident | ident ( ... )
/// UPDATE name SET ident = value [, ident = value]* [WHERE ...]
/// DELETE FROM name [WHERE ...]
/// BEGIN | COMMIT | END | ROLLBACK [TRANSACTION]
///
/// cond  := ident (= | != | <> | < | <= | > | >= | LIKE) value
/// value := NULL | [+|-] number | 'string' | "string" | X'hex'
///        | [ ... ] | vector( ... ) | vector32( ... ) | vector64( ... )
/// ident := word | "quoted" | `quoted` | [bracketed]
/// ```
///
/// Select-list items and the ORDER BY expression are returned as the exact
/// source text they span; the executor interprets function calls in them
/// (`COUNT(*)`, `vector_distance_*`).
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
        let tokens = lexer::tokenize(sql)?;
        let mut cursor = Cursor {
            sql,
            tokens,
            pos: 0,
        };
        let statement = cursor.statement()?;
        cursor.eat(&TokenKind::Semicolon);
        if let Some(tok) = cursor.peek() {
            return Err(cursor.error_at(tok, "Unexpected trailing input"));
        }
        Ok(statement)
    }
}

/// Words that may not be used as a bare table or column name because they
/// would make clause boundaries ambiguous.
const RESERVED: &[&str] = &[
    "AND", "BY", "FROM", "LIKE", "LIMIT", "NOT", "NULL", "OR", "ORDER", "SET", "VALUES", "WHERE",
];

struct Cursor<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Cursor<'a> {
    // ----- token helpers -------------------------------------------------

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_kind(&self) -> Option<&TokenKind> {
        self.peek().map(|t| &t.kind)
    }

    fn peek_kind_at(&self, offset: usize) -> Option<&TokenKind> {
        self.tokens.get(self.pos + offset).map(|t| &t.kind)
    }

    fn advance(&mut self) -> Option<Token> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.peek_kind() == Some(kind) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, kind: &TokenKind, what: &str) -> Result<Token> {
        if self.peek_kind() == Some(kind) {
            Ok(self.advance().unwrap())
        } else {
            Err(self.error(&format!("Expected {}", what)))
        }
    }

    /// Consumes a numeric literal token, returning its text.
    fn number_token(&mut self, what: &str) -> Result<String> {
        match self.peek_kind() {
            Some(TokenKind::Number(n)) => {
                let n = n.clone();
                self.pos += 1;
                Ok(n)
            }
            _ => Err(self.error(&format!("Expected {}", what))),
        }
    }

    fn is_keyword_at(&self, offset: usize, keyword: &str) -> bool {
        matches!(self.peek_kind_at(offset), Some(TokenKind::Word(w)) if w.eq_ignore_ascii_case(keyword))
    }

    fn is_keyword(&self, keyword: &str) -> bool {
        self.is_keyword_at(0, keyword)
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        if self.is_keyword(keyword) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<()> {
        if self.eat_keyword(keyword) {
            Ok(())
        } else {
            Err(self.error(&format!("Expected {}", keyword)))
        }
    }

    /// Error pointing at the current token (or end of input).
    fn error(&self, msg: &str) -> VelociError {
        match self.peek() {
            Some(tok) => self.error_at(tok, msg),
            None => VelociError::ParseError(format!("{} at end of input", msg)),
        }
    }

    fn error_at(&self, tok: &Token, msg: &str) -> VelociError {
        VelociError::ParseError(format!(
            "{} at position {} near '{}'",
            msg,
            tok.start,
            &self.sql[tok.start..tok.end]
        ))
    }

    /// Source text from the start of token `from` to the end of the token
    /// before the cursor.
    fn source_since(&self, from: usize) -> &'a str {
        let start = self.tokens[from].start;
        let end = self.tokens[self.pos - 1].end;
        &self.sql[start..end]
    }

    /// Consumes tokens up to and including the bracket matching the opening
    /// `(` or `[` at the cursor.
    fn skip_balanced(&mut self) -> Result<()> {
        let open = self
            .advance()
            .expect("caller checked for an opening bracket");
        let mut stack = vec![open.kind.clone()];
        while let Some(tok) = self.advance() {
            match tok.kind {
                TokenKind::LParen | TokenKind::LBracket => stack.push(tok.kind),
                TokenKind::RParen | TokenKind::RBracket => {
                    let expected = match stack.pop() {
                        Some(TokenKind::LParen) => TokenKind::RParen,
                        _ => TokenKind::RBracket,
                    };
                    if tok.kind != expected {
                        return Err(self.error_at(&tok, "Mismatched bracket"));
                    }
                    if stack.is_empty() {
                        return Ok(());
                    }
                }
                _ => {}
            }
        }
        Err(self.error_at(&open, "Unclosed bracket"))
    }

    // ----- identifiers ---------------------------------------------------

    /// A table or column name: bare word, `"quoted"`, `` `quoted` `` or
    /// `[bracketed]`.
    fn identifier(&mut self, what: &str) -> Result<String> {
        match self.peek_kind() {
            Some(TokenKind::Word(w)) => {
                if RESERVED.iter().any(|r| w.eq_ignore_ascii_case(r)) {
                    return Err(self.error(&format!(
                        "Expected {}, found reserved word (quote it to use it as a name)",
                        what
                    )));
                }
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            Some(TokenKind::QuotedIdent(s))
            | Some(TokenKind::String {
                value: s,
                quote: '"',
            }) => {
                let s = s.clone();
                self.pos += 1;
                Ok(s)
            }
            Some(TokenKind::LBracket) => {
                // [name with spaces]: take the raw text between the brackets.
                let open = self.advance().unwrap();
                while let Some(tok) = self.advance() {
                    if tok.kind == TokenKind::RBracket {
                        let name = self.sql[open.end..tok.start].trim();
                        if name.is_empty() {
                            return Err(self.error_at(&tok, &format!("Expected {}", what)));
                        }
                        return Ok(name.to_string());
                    }
                }
                Err(self.error_at(&open, "Unterminated bracketed identifier"))
            }
            _ => Err(self.error(&format!("Expected {}", what))),
        }
    }

    // ----- statements ----------------------------------------------------

    fn statement(&mut self) -> Result<Statement> {
        let Some(TokenKind::Word(first)) = self.peek_kind() else {
            return Err(self.error("Expected a SQL statement"));
        };
        match first.to_ascii_uppercase().as_str() {
            "CREATE" => self.create_table(),
            "DROP" => self.drop_table(),
            "ALTER" => self.alter_table(),
            "INSERT" => self.insert(),
            "SELECT" => self.select(),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            "BEGIN" => self.transaction_control(Statement::BeginTransaction),
            "COMMIT" | "END" => self.transaction_control(Statement::CommitTransaction),
            "ROLLBACK" => self.transaction_control(Statement::RollbackTransaction),
            _ => Err(self.error("Unsupported statement")),
        }
    }

    fn transaction_control(&mut self, statement: Statement) -> Result<Statement> {
        self.advance();
        self.eat_keyword("TRANSACTION");
        Ok(statement)
    }

    fn create_table(&mut self) -> Result<Statement> {
        self.expect_keyword("CREATE")?;
        self.expect_keyword("TABLE")?;
        let name = self.identifier("table name")?;
        self.expect(&TokenKind::LParen, "'(' after table name")?;
        let mut columns = vec![self.column_def()?];
        while self.eat(&TokenKind::Comma) {
            columns.push(self.column_def()?);
        }
        self.expect(&TokenKind::RParen, "',' or ')' in column list")?;
        Ok(Statement::CreateTable { name, columns })
    }

    fn column_def(&mut self) -> Result<Column> {
        let name = self.identifier("column name")?;
        let data_type = self.data_type(&name)?;
        let mut column = Column {
            name,
            data_type,
            primary_key: false,
            not_null: false,
            unique: false,
        };
        loop {
            if self.eat_keyword("PRIMARY") {
                self.expect_keyword("KEY")?;
                column.primary_key = true;
                column.not_null = true;
            } else if self.eat_keyword("NOT") {
                self.expect_keyword("NULL")?;
                column.not_null = true;
            } else if self.eat_keyword("NULL") {
                // Explicitly nullable: the default.
            } else if self.eat_keyword("UNIQUE") {
                column.unique = true;
            } else if matches!(
                self.peek_kind(),
                Some(TokenKind::Comma | TokenKind::RParen | TokenKind::Semicolon) | None
            ) {
                return Ok(column);
            } else {
                return Err(self.error(&format!(
                    "Unsupported constraint on column '{}'",
                    column.name
                )));
            }
        }
    }

    /// `INTEGER`, `VARCHAR(255)`, `F32_BLOB(3)`, ... Unknown names map to
    /// TEXT (see [`DataType::from_str`]).
    fn data_type(&mut self, column: &str) -> Result<DataType> {
        let Some(TokenKind::Word(type_name)) = self.peek_kind() else {
            return Err(self.error(&format!("Missing data type for column '{}'", column)));
        };
        let mut spelled = type_name.clone();
        self.pos += 1;
        if self.eat(&TokenKind::LParen) {
            let mut args = Vec::new();
            loop {
                args.push(self.number_token("a number in type arguments")?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "')' after type arguments")?;
            spelled = format!("{}({})", spelled, args.join(","));
        }
        Ok(DataType::from_str(&spelled))
    }

    fn drop_table(&mut self) -> Result<Statement> {
        self.expect_keyword("DROP")?;
        self.expect_keyword("TABLE")?;
        let name = self.identifier("table name")?;
        Ok(Statement::DropTable { name })
    }

    fn alter_table(&mut self) -> Result<Statement> {
        self.expect_keyword("ALTER")?;
        self.expect_keyword("TABLE")?;
        let table = self.identifier("table name")?;

        let action = if self.eat_keyword("RENAME") {
            if self.eat_keyword("TO") {
                AlterAction::RenameTable {
                    new_name: self.identifier("new table name")?,
                }
            } else {
                self.eat_keyword("COLUMN");
                let old_name = self.identifier("column name")?;
                self.expect_keyword("TO")?;
                let new_name = self.identifier("new column name")?;
                AlterAction::RenameColumn { old_name, new_name }
            }
        } else if self.eat_keyword("ADD") {
            self.eat_keyword("COLUMN");
            let column = self.column_def()?;
            if column.primary_key {
                return Err(VelociError::ParseError(
                    "Cannot add a PRIMARY KEY column with ALTER TABLE".to_string(),
                ));
            }
            if column.not_null {
                return Err(VelociError::ParseError(
                    "Cannot add a NOT NULL column without a default value".to_string(),
                ));
            }
            AlterAction::AddColumn { column }
        } else if self.eat_keyword("DROP") {
            self.eat_keyword("COLUMN");
            AlterAction::DropColumn {
                name: self.identifier("column name")?,
            }
        } else {
            return Err(self.error("Expected RENAME, ADD or DROP after ALTER TABLE"));
        };

        Ok(Statement::AlterTable { table, action })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_keyword("INSERT")?;
        self.expect_keyword("INTO")?;
        let table = self.identifier("table name")?;

        let columns = if self.eat(&TokenKind::LParen) {
            let mut columns = vec![self.identifier("column name")?];
            while self.eat(&TokenKind::Comma) {
                columns.push(self.identifier("column name")?);
            }
            self.expect(&TokenKind::RParen, "',' or ')' in column list")?;
            Some(columns)
        } else {
            None
        };

        self.expect_keyword("VALUES")?;
        self.expect(&TokenKind::LParen, "'(' after VALUES")?;
        let mut values = vec![self.value()?];
        while self.eat(&TokenKind::Comma) {
            values.push(self.value()?);
        }
        self.expect(&TokenKind::RParen, "',' or ')' in VALUES list")?;
        if self.peek_kind() == Some(&TokenKind::Comma) {
            return Err(self.error("Multi-row VALUES is not supported"));
        }

        Ok(Statement::Insert {
            table,
            columns,
            values,
        })
    }

    fn select(&mut self) -> Result<Statement> {
        self.expect_keyword("SELECT")?;

        let mut columns = vec![self.select_item()?];
        while self.eat(&TokenKind::Comma) {
            columns.push(self.select_item()?);
        }

        self.expect_keyword("FROM")?;
        let table = self.identifier("table name")?;
        let where_clause = self.where_clause()?;

        let order_by = if self.eat_keyword("ORDER") {
            self.expect_keyword("BY")?;
            let column = self.select_item()?;
            if column == "*" {
                return Err(VelociError::ParseError(
                    "ORDER BY * is not valid".to_string(),
                ));
            }
            let ascending = if self.eat_keyword("DESC") {
                false
            } else {
                self.eat_keyword("ASC");
                true
            };
            Some(OrderBy { column, ascending })
        } else {
            None
        };

        let limit = if self.eat_keyword("LIMIT") {
            let n = self.number_token("a non-negative integer after LIMIT")?;
            Some(
                n.parse::<u64>()
                    .map_err(|_| VelociError::ParseError(format!("Invalid LIMIT value: {}", n)))?,
            )
        } else {
            None
        };

        Ok(Statement::Select {
            table,
            columns,
            where_clause,
            order_by,
            limit,
        })
    }

    /// `*`, a column name, or a function call such as `COUNT(*)` or
    /// `vector_distance_cos(col, vector32('[...]'))`, returned as written.
    fn select_item(&mut self) -> Result<String> {
        if self.eat(&TokenKind::Star) {
            return Ok("*".to_string());
        }
        let start = self.pos;
        if matches!(self.peek_kind(), Some(TokenKind::Word(_)))
            && self.peek_kind_at(1) == Some(&TokenKind::LParen)
        {
            self.pos += 1;
            self.skip_balanced()?;
            return Ok(self.source_since(start).to_string());
        }
        self.identifier("column name or expression")
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_keyword("UPDATE")?;
        let table = self.identifier("table name")?;
        self.expect_keyword("SET")?;

        let mut assignments = HashMap::new();
        loop {
            let column = self.identifier("column name")?;
            self.expect(&TokenKind::Eq, "'=' in SET assignment")?;
            let value = self.value()?;
            assignments.insert(column, value);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        let where_clause = self.where_clause()?;
        Ok(Statement::Update {
            table,
            assignments,
            where_clause,
        })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_keyword("DELETE")?;
        self.expect_keyword("FROM")?;
        let table = self.identifier("table name")?;
        let where_clause = self.where_clause()?;
        Ok(Statement::Delete {
            table,
            where_clause,
        })
    }

    // ----- WHERE ---------------------------------------------------------

    fn where_clause(&mut self) -> Result<Option<WhereClause>> {
        if !self.eat_keyword("WHERE") {
            return Ok(None);
        }
        let mut conditions = vec![self.condition()?];
        while self.eat_keyword("AND") {
            conditions.push(self.condition()?);
        }
        if self.is_keyword("OR") {
            return Err(self.error("OR in WHERE is not supported yet"));
        }
        Ok(Some(WhereClause { conditions }))
    }

    fn condition(&mut self) -> Result<Condition> {
        if self.peek_kind() == Some(&TokenKind::LParen) {
            return Err(self.error("Parenthesized WHERE conditions are not supported yet"));
        }
        let column = self.identifier("column name in WHERE")?;
        let operator = match self.peek_kind() {
            Some(TokenKind::Eq) => Operator::Equal,
            Some(TokenKind::NotEq) => Operator::NotEqual,
            Some(TokenKind::Lt) => Operator::LessThan,
            Some(TokenKind::LtEq) => Operator::LessThanOrEqual,
            Some(TokenKind::Gt) => Operator::GreaterThan,
            Some(TokenKind::GtEq) => Operator::GreaterThanOrEqual,
            Some(TokenKind::Word(w)) if w.eq_ignore_ascii_case("LIKE") => Operator::Like,
            _ => return Err(self.error("Expected a comparison operator")),
        };
        self.pos += 1;
        let value = self.value()?;
        Ok(Condition {
            column,
            operator,
            value,
        })
    }

    // ----- literals ------------------------------------------------------

    fn value(&mut self) -> Result<Value> {
        let Some(tok) = self.peek().cloned() else {
            return Err(self.error("Expected a value"));
        };
        match &tok.kind {
            TokenKind::Word(w) if w.eq_ignore_ascii_case("NULL") => {
                self.pos += 1;
                Ok(Value::Null)
            }
            TokenKind::Word(w)
                if ["vector", "vector32", "vector64"]
                    .iter()
                    .any(|f| w.eq_ignore_ascii_case(f))
                    && self.peek_kind_at(1) == Some(&TokenKind::LParen) =>
            {
                let start = self.pos;
                self.pos += 1;
                self.skip_balanced()?;
                self.vector_literal(start)
            }
            TokenKind::LBracket => {
                let start = self.pos;
                self.skip_balanced()?;
                self.vector_literal(start)
            }
            TokenKind::String { value, .. } => {
                self.pos += 1;
                Ok(Value::Text(value.clone()))
            }
            TokenKind::Blob(hex) => {
                self.pos += 1;
                parse_blob(hex)
            }
            TokenKind::Number(_) | TokenKind::Minus | TokenKind::Plus => self.number(),
            TokenKind::Word(_) => Err(self.error_at(
                &tok,
                "Expected a value (column references are not supported here; quote strings with '...')",
            )),
            _ => Err(self.error_at(&tok, "Expected a value")),
        }
    }

    fn vector_literal(&self, start: usize) -> Result<Value> {
        let text = self.source_since(start);
        match crate::vector::parse_vector_constructor(text) {
            Some(parsed) => Ok(Value::Vector(parsed?)),
            None => Err(VelociError::ParseError(format!(
                "Invalid vector literal: {}",
                text
            ))),
        }
    }

    fn number(&mut self) -> Result<Value> {
        let negative = if self.eat(&TokenKind::Minus) {
            true
        } else {
            self.eat(&TokenKind::Plus);
            false
        };
        let text = self.number_token("a number")?;
        let text = if negative { format!("-{}", text) } else { text };
        if let Ok(i) = text.parse::<i64>() {
            return Ok(Value::Integer(i));
        }
        text.parse::<f64>()
            .map(Value::Float)
            .map_err(|_| VelociError::ParseError(format!("Invalid number: {}", text)))
    }
}

fn parse_blob(hex: &str) -> Result<Value> {
    if hex.len() % 2 != 0 {
        return Err(VelociError::ParseError(
            "Invalid BLOB literal: odd number of hex digits".to_string(),
        ));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|byte| u8::from_str_radix(byte, 16).ok())
                .ok_or_else(|| {
                    VelociError::ParseError(format!("Invalid hex digit in BLOB: {}", hex))
                })
        })
        .collect::<Result<Vec<u8>>>()
        .map(Value::Blob)
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
        let stmt = parser.parse("SELECT * FROM users WHERE age > 25").unwrap();

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
            Statement::Select {
                order_by,
                limit,
                where_clause,
                ..
            } => {
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
            Statement::Select {
                limit,
                order_by,
                where_clause,
                ..
            } => {
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
    fn test_parse_errors_point_at_offending_token() {
        let parser = Parser::new();
        let err = |sql: &str| match parser.parse(sql) {
            Err(VelociError::ParseError(msg)) => msg,
            other => panic!("expected a parse error for {:?}, got {:?}", sql, other),
        };
        assert!(err("SELECT * FROM t LIMIT").ends_with("at end of input"));
        assert!(err("INSERT INTO t VALUES (1, -").ends_with("at end of input"));
        assert!(err("CREATE TABLE t (a VARCHAR(").ends_with("at end of input"));
        assert!(err("SELECT * FROM t WHERE a = 1 OR b = 2").contains("near 'OR'"));
        assert!(err("SELECT * FROM t LIMIT x").contains("position 22 near 'x'"));
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

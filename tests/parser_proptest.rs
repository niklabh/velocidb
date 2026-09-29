//! Property tests for the SQL parser.
//!
//! - Round-trip: a random `Statement` rendered to SQL (random whitespace,
//!   comments, keyword case, identifier quoting and literal spelling) parses
//!   back to the same `Statement`.
//! - Splitting: several rendered statements joined by `;` split back into
//!   the same statements.
//! - Robustness: arbitrary input never panics the lexer, parser or splitter.

use proptest::prelude::*;
use std::collections::HashMap;
use velocidb::parser::{
    split_statements, AlterAction, Condition, Operator, OrderBy, Parser, Statement, WhereClause,
};
use velocidb::types::{Column, DataType, Value};

// Must match `RESERVED` in src/parser.rs.
const RESERVED: &[&str] = &[
    "AND", "BY", "FROM", "LIKE", "LIMIT", "NOT", "NULL", "OR", "ORDER", "SET", "VALUES", "WHERE",
];

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Deterministic stream of rendering choices drawn from a proptest seed.
struct Style(u64);

impl Style {
    fn pick(&mut self, n: u64) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    }

    fn flip(&mut self) -> bool {
        self.pick(2) == 0
    }

    fn keyword(&mut self, kw: &str) -> String {
        match self.pick(3) {
            0 => kw.to_uppercase(),
            1 => kw.to_lowercase(),
            _ => kw
                .chars()
                .enumerate()
                .map(|(i, c)| {
                    if i % 2 == 0 {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    }
                })
                .collect(),
        }
    }
}

fn is_plain_ident(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.iter().any(|r| r.eq_ignore_ascii_case(name))
        // `CREATE INDEX if ...` / `DROP INDEX if ...` read IF as the keyword.
        && !name.eq_ignore_ascii_case("IF")
}

fn ident(name: &str, s: &mut Style) -> String {
    let plain = is_plain_ident(name);
    match s.pick(4) {
        0 | 1 if plain => name.to_string(),
        2 if plain => format!("[{}]", name),
        3 => format!("`{}`", name.replace('`', "``")),
        _ => format!("\"{}\"", name.replace('"', "\"\"")),
    }
}

fn string_literal(text: &str, s: &mut Style) -> String {
    let quote = if s.pick(4) == 0 { '"' } else { '\'' };
    let body = text
        .replace('\\', "\\\\")
        .replace(quote, &format!("{0}{0}", quote));
    format!("{0}{1}{0}", quote, body)
}

fn vector_components(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{:?}", x)).collect();
    format!("[{}]", parts.join(", "))
}

fn value(v: &Value, s: &mut Style) -> String {
    match v {
        Value::Null => s.keyword("null"),
        Value::Integer(i) => {
            if *i >= 0 && s.pick(4) == 0 {
                format!("+{}", i)
            } else {
                i.to_string()
            }
        }
        Value::Float(f) => format!("{:?}", f),
        Value::Text(t) => string_literal(t, s),
        Value::Blob(b) => {
            let hex: String = b.iter().map(|byte| format!("{:02x}", byte)).collect();
            let hex = if s.flip() { hex.to_uppercase() } else { hex };
            format!("{}'{}'", if s.flip() { 'X' } else { 'x' }, hex)
        }
        Value::Vector(v) => match s.pick(3) {
            0 => vector_components(v),
            1 => format!("vector32('{}')", vector_components(v)),
            _ => format!("{}('{}')", s.keyword("vector"), vector_components(v)),
        },
        other => panic!("generator produced unsupported value {:?}", other),
    }
}

fn operator(op: &Operator, s: &mut Style) -> String {
    match op {
        Operator::Equal => if s.flip() { "=" } else { "==" }.to_string(),
        Operator::NotEqual => if s.flip() { "!=" } else { "<>" }.to_string(),
        Operator::GreaterThan => ">".to_string(),
        Operator::LessThan => "<".to_string(),
        Operator::GreaterThanOrEqual => ">=".to_string(),
        Operator::LessThanOrEqual => "<=".to_string(),
        Operator::Like => s.keyword("like"),
    }
}

fn data_type(dt: &DataType, s: &mut Style) -> String {
    let name = match dt {
        DataType::Integer => ["INTEGER", "INT"][s.pick(2) as usize].to_string(),
        DataType::Real => ["REAL", "FLOAT", "DOUBLE"][s.pick(3) as usize].to_string(),
        DataType::Text => match s.pick(3) {
            0 => "TEXT".to_string(),
            1 => "STRING".to_string(),
            _ => format!("VARCHAR({})", s.pick(300)),
        },
        DataType::Blob => "BLOB".to_string(),
        DataType::Vector(n) => {
            let f = ["F32_BLOB", "VECTOR"][s.pick(2) as usize];
            format!("{}({})", f, n)
        }
        DataType::Null => unreachable!("not generated"),
    };
    s.keyword(&name)
}

fn column_def(c: &Column, s: &mut Style, out: &mut Vec<String>) {
    out.push(ident(&c.name, s));
    out.push(data_type(&c.data_type, s));
    let mut constraints = Vec::new();
    if c.primary_key {
        constraints.push(vec!["PRIMARY", "KEY"]);
    }
    if c.not_null && (!c.primary_key || s.flip()) {
        constraints.push(vec!["NOT", "NULL"]);
    }
    if !c.not_null && s.pick(4) == 0 {
        constraints.push(vec!["NULL"]);
    }
    if c.unique {
        constraints.push(vec!["UNIQUE"]);
    }
    // Constraint order is free.
    if s.flip() {
        constraints.reverse();
    }
    for words in constraints {
        for w in words {
            out.push(s.keyword(w));
        }
    }
}

fn where_clause(w: &Option<WhereClause>, s: &mut Style, out: &mut Vec<String>) {
    if let Some(w) = w {
        out.push(s.keyword("WHERE"));
        for (i, c) in w.conditions.iter().enumerate() {
            if i > 0 {
                out.push(s.keyword("AND"));
            }
            out.push(ident(&c.column, s));
            out.push(operator(&c.operator, s));
            out.push(value(&c.value, s));
        }
    }
}

fn select_item(item: &str, s: &mut Style) -> String {
    if item == "*" || item.contains('(') {
        item.to_string()
    } else {
        ident(item, s)
    }
}

/// Renders a statement as a list of SQL tokens.
fn render_tokens(stmt: &Statement, s: &mut Style) -> Vec<String> {
    let mut out = Vec::new();
    let kw = |s: &mut Style, out: &mut Vec<String>, words: &[&str]| {
        for w in words {
            out.push(s.keyword(w));
        }
    };
    match stmt {
        Statement::CreateTable { name, columns } => {
            kw(s, &mut out, &["CREATE", "TABLE"]);
            out.push(ident(name, s));
            out.push("(".into());
            for (i, c) in columns.iter().enumerate() {
                if i > 0 {
                    out.push(",".into());
                }
                column_def(c, s, &mut out);
            }
            out.push(")".into());
        }
        Statement::DropTable { name } => {
            kw(s, &mut out, &["DROP", "TABLE"]);
            out.push(ident(name, s));
        }
        Statement::CreateIndex {
            name,
            table,
            column,
            if_not_exists,
        } => {
            kw(s, &mut out, &["CREATE", "INDEX"]);
            if *if_not_exists {
                kw(s, &mut out, &["IF", "NOT", "EXISTS"]);
            }
            out.push(ident(name, s));
            kw(s, &mut out, &["ON"]);
            out.push(ident(table, s));
            out.push("(".into());
            out.push(ident(column, s));
            out.push(")".into());
        }
        Statement::DropIndex { name, if_exists } => {
            kw(s, &mut out, &["DROP", "INDEX"]);
            if *if_exists {
                kw(s, &mut out, &["IF", "EXISTS"]);
            }
            out.push(ident(name, s));
        }
        Statement::AlterTable { table, action } => {
            kw(s, &mut out, &["ALTER", "TABLE"]);
            out.push(ident(table, s));
            match action {
                AlterAction::RenameTable { new_name } => {
                    kw(s, &mut out, &["RENAME", "TO"]);
                    out.push(ident(new_name, s));
                }
                AlterAction::RenameColumn { old_name, new_name } => {
                    kw(s, &mut out, &["RENAME"]);
                    if s.flip() {
                        kw(s, &mut out, &["COLUMN"]);
                    }
                    out.push(ident(old_name, s));
                    kw(s, &mut out, &["TO"]);
                    out.push(ident(new_name, s));
                }
                AlterAction::AddColumn { column } => {
                    kw(s, &mut out, &["ADD"]);
                    if s.flip() {
                        kw(s, &mut out, &["COLUMN"]);
                    }
                    column_def(column, s, &mut out);
                }
                AlterAction::DropColumn { name } => {
                    kw(s, &mut out, &["DROP"]);
                    if s.flip() {
                        kw(s, &mut out, &["COLUMN"]);
                    }
                    out.push(ident(name, s));
                }
            }
        }
        Statement::Insert {
            table,
            columns,
            values,
        } => {
            kw(s, &mut out, &["INSERT", "INTO"]);
            out.push(ident(table, s));
            if let Some(columns) = columns {
                out.push("(".into());
                for (i, c) in columns.iter().enumerate() {
                    if i > 0 {
                        out.push(",".into());
                    }
                    out.push(ident(c, s));
                }
                out.push(")".into());
            }
            kw(s, &mut out, &["VALUES"]);
            out.push("(".into());
            for (i, v) in values.iter().enumerate() {
                if i > 0 {
                    out.push(",".into());
                }
                out.push(value(v, s));
            }
            out.push(")".into());
        }
        Statement::Select {
            table,
            columns,
            where_clause: w,
            order_by,
            limit,
        } => {
            kw(s, &mut out, &["SELECT"]);
            for (i, c) in columns.iter().enumerate() {
                if i > 0 {
                    out.push(",".into());
                }
                out.push(select_item(c, s));
            }
            kw(s, &mut out, &["FROM"]);
            out.push(ident(table, s));
            where_clause(w, s, &mut out);
            if let Some(order) = order_by {
                kw(s, &mut out, &["ORDER", "BY"]);
                out.push(select_item(&order.column, s));
                if !order.ascending {
                    kw(s, &mut out, &["DESC"]);
                } else if s.flip() {
                    kw(s, &mut out, &["ASC"]);
                }
            }
            if let Some(n) = limit {
                kw(s, &mut out, &["LIMIT"]);
                out.push(n.to_string());
            }
        }
        Statement::Update {
            table,
            assignments,
            where_clause: w,
        } => {
            kw(s, &mut out, &["UPDATE"]);
            out.push(ident(table, s));
            kw(s, &mut out, &["SET"]);
            for (i, (col, v)) in assignments.iter().enumerate() {
                if i > 0 {
                    out.push(",".into());
                }
                out.push(ident(col, s));
                out.push("=".into());
                out.push(value(v, s));
            }
            where_clause(w, s, &mut out);
        }
        Statement::Delete {
            table,
            where_clause: w,
        } => {
            kw(s, &mut out, &["DELETE", "FROM"]);
            out.push(ident(table, s));
            where_clause(w, s, &mut out);
        }
        Statement::BeginTransaction => {
            kw(s, &mut out, &["BEGIN"]);
            if s.flip() {
                kw(s, &mut out, &["TRANSACTION"]);
            }
        }
        Statement::CommitTransaction => {
            let word = if s.flip() { "COMMIT" } else { "END" };
            kw(s, &mut out, &[word]);
            if s.flip() {
                kw(s, &mut out, &["TRANSACTION"]);
            }
        }
        Statement::RollbackTransaction => {
            kw(s, &mut out, &["ROLLBACK"]);
            if s.flip() {
                kw(s, &mut out, &["TRANSACTION"]);
            }
        }
    }
    out
}

fn is_punct(tok: &str) -> bool {
    matches!(
        tok,
        "(" | ")" | "," | "=" | "==" | "!=" | "<>" | "<" | ">" | "<=" | ">="
    )
}

/// Joins tokens with random whitespace / comments; punctuation may touch
/// its neighbours.
fn join(tokens: &[String], s: &mut Style) -> String {
    let mut sql = String::new();
    for (i, tok) in tokens.iter().enumerate() {
        if i > 0 {
            let may_touch = is_punct(tok) || is_punct(&tokens[i - 1]);
            let sep = match s.pick(if may_touch { 6 } else { 5 }) {
                0 | 1 => " ",
                2 => "\n  ",
                3 => "\t",
                4 => " -- comment; with 'quote'\n",
                _ => "",
            };
            sql.push_str(sep);
        }
        sql.push_str(tok);
    }
    sql
}

fn render(stmt: &Statement, seed: u64) -> String {
    let mut s = Style(seed | 1);
    let tokens = render_tokens(stmt, &mut s);
    let mut sql = join(&tokens, &mut s);
    if s.pick(3) == 0 {
        sql.push(';');
    }
    sql
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

fn name() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => "[a-z_][a-z0-9_]{0,7}",
        1 => "[A-Za-z0-9_ .;'\"`-]{1,8}",
    ]
    .prop_filter("names must be non-empty and not blank", |n| {
        !n.trim().is_empty()
    })
}

fn any_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Integer),
        any::<f64>()
            .prop_filter("finite", |f| f.is_finite())
            .prop_map(Value::Float),
        ".{0,12}".prop_map(Value::Text),
        prop::collection::vec(any::<u8>(), 0..6).prop_map(Value::Blob),
        prop::collection::vec(any::<f32>().prop_filter("finite", |f| f.is_finite()), 0..5)
            .prop_map(Value::Vector),
    ]
}

fn operator_strategy() -> impl Strategy<Value = Operator> {
    prop_oneof![
        Just(Operator::Equal),
        Just(Operator::NotEqual),
        Just(Operator::GreaterThan),
        Just(Operator::LessThan),
        Just(Operator::GreaterThanOrEqual),
        Just(Operator::LessThanOrEqual),
        Just(Operator::Like),
    ]
}

fn where_strategy() -> impl Strategy<Value = Option<WhereClause>> {
    prop::option::of(
        prop::collection::vec(
            (name(), operator_strategy(), any_value()).prop_map(|(column, operator, value)| {
                Condition {
                    column,
                    operator,
                    value,
                }
            }),
            1..4,
        )
        .prop_map(|conditions| WhereClause { conditions }),
    )
}

fn data_type_strategy() -> impl Strategy<Value = DataType> {
    prop_oneof![
        Just(DataType::Integer),
        Just(DataType::Real),
        Just(DataType::Text),
        Just(DataType::Blob),
        (1u32..2048).prop_map(DataType::Vector),
    ]
}

fn column_strategy(allow_pk: bool) -> impl Strategy<Value = Column> {
    (
        name(),
        data_type_strategy(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
    )
        .prop_map(move |(name, data_type, pk, not_null, unique)| {
            let primary_key = pk && allow_pk;
            Column {
                name,
                data_type,
                primary_key,
                // PRIMARY KEY implies NOT NULL; ADD COLUMN rejects NOT NULL.
                not_null: allow_pk && (primary_key || not_null),
                unique,
            }
        })
}

fn select_item_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        4 => name(),
        1 => Just("COUNT(*)".to_string()),
        1 => Just("vector_distance_cos(embedding, vector32('[1, 0.5]'))".to_string()),
    ]
}

fn statement() -> impl Strategy<Value = Statement> {
    let alter = prop_oneof![
        name().prop_map(|new_name| AlterAction::RenameTable { new_name }),
        (name(), name())
            .prop_map(|(old_name, new_name)| AlterAction::RenameColumn { old_name, new_name }),
        column_strategy(false).prop_map(|column| AlterAction::AddColumn { column }),
        name().prop_map(|name| AlterAction::DropColumn { name }),
    ];
    prop_oneof![
        (name(), prop::collection::vec(column_strategy(true), 1..5))
            .prop_map(|(name, columns)| Statement::CreateTable { name, columns }),
        name().prop_map(|name| Statement::DropTable { name }),
        (name(), name(), name(), any::<bool>()).prop_map(|(name, table, column, if_not_exists)| {
            Statement::CreateIndex {
                name,
                table,
                column,
                if_not_exists,
            }
        }),
        (name(), any::<bool>())
            .prop_map(|(name, if_exists)| Statement::DropIndex { name, if_exists }),
        (name(), alter).prop_map(|(table, action)| Statement::AlterTable { table, action }),
        (
            name(),
            prop::option::of(prop::collection::vec(name(), 1..4)),
            prop::collection::vec(any_value(), 1..5),
        )
            .prop_map(|(table, columns, values)| Statement::Insert {
                table,
                columns,
                values
            }),
        (
            name(),
            prop_oneof![
                Just(vec!["*".to_string()]),
                prop::collection::vec(select_item_strategy(), 1..4),
            ],
            where_strategy(),
            prop::option::of((select_item_strategy(), any::<bool>())),
            prop::option::of(any::<u64>()),
        )
            .prop_map(|(table, columns, where_clause, order, limit)| {
                Statement::Select {
                    table,
                    columns,
                    where_clause,
                    order_by: order.map(|(column, ascending)| OrderBy { column, ascending }),
                    limit,
                }
            }),
        (
            name(),
            prop::collection::hash_map(name(), any_value(), 1..4),
            where_strategy(),
        )
            .prop_map(|(table, assignments, where_clause)| Statement::Update {
                table,
                assignments: assignments.into_iter().collect::<HashMap<_, _>>(),
                where_clause,
            }),
        (name(), where_strategy()).prop_map(|(table, where_clause)| Statement::Delete {
            table,
            where_clause
        }),
        Just(Statement::BeginTransaction),
        Just(Statement::CommitTransaction),
        Just(Statement::RollbackTransaction),
    ]
}

/// Keyword / punctuation soup: likely to reach deep parser states.
fn sql_soup() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        prop::sample::select(vec![
            "SELECT",
            "FROM",
            "WHERE",
            "AND",
            "OR",
            "ORDER",
            "BY",
            "LIMIT",
            "INSERT",
            "INTO",
            "VALUES",
            "UPDATE",
            "SET",
            "DELETE",
            "CREATE",
            "TABLE",
            "ALTER",
            "DROP",
            "ADD",
            "COLUMN",
            "RENAME",
            "TO",
            "PRIMARY",
            "KEY",
            "NOT",
            "NULL",
            "UNIQUE",
            "BEGIN",
            "COMMIT",
            "ROLLBACK",
            "TRANSACTION",
            "DESC",
            "ASC",
            "LIKE",
            "COUNT",
            "vector32",
            "VECTOR",
            "F32_BLOB",
            "INTEGER",
            "(",
            ")",
            "[",
            "]",
            ",",
            ";",
            "*",
            "=",
            "!=",
            "<>",
            "<",
            "<=",
            ">",
            ">=",
            "-",
            "+",
            ".",
            "'",
            "\"",
            "`",
            "''",
            "x'",
            "X'00'",
            "--",
            "\\",
            "t",
            "id",
            "1",
            "-1",
            "1.5",
            "1e",
            "'a'",
            "\n",
        ])
        .prop_map(str::to_string),
        "[a-z0-9]{1,3}",
    ];
    prop::collection::vec(piece, 0..24).prop_map(|pieces| pieces.join(" "))
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2000,
        // Integration tests have no lib.rs next to them to persist into.
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    #[test]
    fn roundtrip(stmt in statement(), seed in any::<u64>()) {
        let sql = render(&stmt, seed);
        let parsed = Parser::new().parse(&sql);
        prop_assert!(parsed.is_ok(), "failed to parse {:?}: {:?}", sql, parsed);
        prop_assert_eq!(parsed.unwrap(), stmt, "sql: {:?}", sql);
    }

    #[test]
    fn split_roundtrip(
        stmts in prop::collection::vec(statement(), 1..5),
        seed in any::<u64>(),
    ) {
        let mut sql = String::new();
        for (i, stmt) in stmts.iter().enumerate() {
            let mut rendered = render(stmt, seed.wrapping_add(i as u64));
            if !rendered.ends_with(';') {
                rendered.push(';');
            }
            sql.push_str(&rendered);
            sql.push_str(if i % 2 == 0 { "\n" } else { " ;; " });
        }
        let parts = split_statements(&sql);
        prop_assert!(parts.is_ok(), "failed to split {:?}", sql);
        let parts = parts.unwrap();
        prop_assert_eq!(parts.len(), stmts.len(), "sql: {:?}", sql);
        for (part, stmt) in parts.iter().zip(&stmts) {
            prop_assert_eq!(&Parser::new().parse(part).unwrap(), stmt, "part: {:?}", part);
        }
    }

    #[test]
    fn arbitrary_input_never_panics(sql in ".{0,64}") {
        let _ = Parser::new().parse(&sql);
        let _ = split_statements(&sql);
        let _ = velocidb::parser::has_complete_statement(&sql);
    }

    #[test]
    fn sql_soup_never_panics(sql in sql_soup()) {
        let _ = Parser::new().parse(&sql);
        let _ = split_statements(&sql);
        let _ = velocidb::parser::has_complete_statement(&sql);
    }
}

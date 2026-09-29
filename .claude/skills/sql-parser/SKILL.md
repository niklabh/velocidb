---
name: sql-parser
description: Extending VelociDB's SQL parser in src/parser.rs - the lexer (src/parser/lexer.rs), the recursive-descent Cursor, value parsing, and the golden tests. Use when adding SQL syntax, new statements or operators, changing WHERE/ORDER BY/VALUES parsing, or debugging parse errors.
---

# SQL Parser

`src/parser/lexer.rs` turns SQL into `Token`s (kind + byte span).
`src/parser.rs` parses them by recursive descent (`Cursor`) into the
`Statement` enum consumed by the executor. The grammar is documented on
`Parser` in `src/parser.rs`; keep that comment in sync.

## Golden rules

1. **Never scan or split SQL text.** Work on tokens. When the executor needs
   an expression as a string (select-list items, ORDER BY, vector
   constructors), skip it as tokens (`Cursor::skip_balanced`) and take the
   exact source slice with `Cursor::source_since`.
2. **Reject, don't guess.** Unsupported syntax must be a parse error with a
   position (`Cursor::error`), never silently reinterpreted. The old regex
   parser turned `WHERE a = 1 OR b = 2` into `a = '1 OR b = 2'`; the golden
   tests exist to keep that class of bug out.
3. Keywords are `TokenKind::Word`s matched case-insensitively
   (`eat_keyword` / `expect_keyword`). Words that delimit clauses are in
   `RESERVED` and cannot be bare names (quote them instead).
4. `Parser::parse` accepts one optional trailing `;` and errors on anything
   after it.

## Adding syntax checklist

- New statement: add a `Statement` variant, a branch in `Cursor::statement`,
  a method, executor handling in `execute_statement`/`query_statement`, and
  — if it mutates schema — extend `needs_schema_save` in `Database::execute`
  (`src/storage.rs`).
- New operator: a `TokenKind` in the lexer if it is punctuation, then
  `Cursor::condition`, `Operator::from_str` and `Operator::evaluate`.
- New value form: extend `Cursor::value`. `vector*(...)` / `[...]` go to
  `vector::parse_vector_constructor`; a double-quoted token is text in value
  position and an identifier in name position (compatibility).
- Aggregates / expressions are still opaque strings (`OrderBy.column`, the
  SELECT column list) interpreted by the executor
  (`vector::parse_distance_expr`, `COUNT(` prefix). A typed expression AST is
  roadmap P1; until then follow that pattern.

## Known limitations (documented; don't accidentally regress)

- No JOIN, GROUP BY, sub-queries, OR / parentheses in WHERE, `AS`,
  multi-row VALUES, `DEFAULT`.
- Aggregates: only `COUNT(*)`.
- WHERE compares a column with a literal only (no column-to-column).

## Testing

- `tests/parser_golden.rs` + `tests/golden/parser.golden`: add every new
  syntax form (and a malformed variant) to `CASES`, run
  `UPDATE_GOLDEN=1 cargo test --test parser_golden`, and review the diff of
  the `.golden` file — unintended lines changing means a regression.
- Lexer unit tests live in `src/parser/lexer.rs`.
- Add an end-to-end test in `tests/` too; the executor is where mis-parses
  surface.

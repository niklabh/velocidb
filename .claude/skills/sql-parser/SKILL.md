---
name: sql-parser
description: Extending VelociDB's regex-based SQL parser in src/parser.rs - statement dispatch, quote/paren-aware splitting helpers, value parsing, and known fragility. Use when adding SQL syntax, new statements or operators, changing WHERE/ORDER BY/VALUES parsing, or debugging "Invalid ... syntax" / mis-split errors.
---

# SQL Parser

`src/parser.rs` is a hand-rolled parser: statement dispatch by keyword prefix
in `Parser::parse`, one regex-based `parse_*` method per statement, producing
the `Statement` enum consumed by the executor. There is no tokenizer — this
makes it fragile in specific, known ways.

## Golden rules

1. **Never split on `,` or scan for keywords with naive string ops.** SQL
   values contain commas inside quotes (`'a, b'`), brackets (`[1, 2]`), and
   nested calls (`vector32('[1, 2]')`). Use the existing helpers:
   - `split_top_level_commas` — quote/paren/bracket-aware comma split
     (SELECT columns)
   - `parse_values` — same awareness plus escape handling (INSERT values)
   - `split_on_and` — quote-aware AND split (WHERE)
   - `find_order_by` — quote-aware ORDER BY locator (handles arbitrary
     expressions; a regex cannot)
2. **Regexes must anchor greedily to the end** when the tail can contain `)`.
   The INSERT regex uses `VALUES\s*\((.+)\)\s*;?\s*$` — a lazy `[^)]+` breaks
   nested function calls.
3. **Strip clauses right-to-left in SELECT**: LIMIT first, then ORDER BY,
   then match `SELECT ... FROM ... [WHERE ...]` on the remainder. New trailing
   clauses (e.g. OFFSET) must slot into that stripping order.
4. Keyword matching is case-insensitive (`(?i)` or `to_uppercase`); keep it
   that way.

## Adding syntax checklist

- New statement: add a `Statement` variant, a keyword branch in
  `Parser::parse`, a `parse_*` method, executor handling in
  `execute_statement`/`query_statement`, and — if it mutates schema —
  extend `needs_schema_save` in `Database::execute` (`src/storage.rs`).
- New operator: extend `Operator::from_str` AND `Operator::evaluate` AND the
  operator alternation regex in `parse_where_clause` (longest operators
  first: `>=` before `>`).
- New value form: extend `parse_value`. Order matters — NULL, then vector
  constructors, then quoted strings, then integer, then float, then blob
  `X'..'`, with bare text as the fallback.
- Expressions (like vector distance calls) are intentionally kept as opaque
  strings in the AST (`OrderBy.column`, SELECT column list); the executor
  interprets them via `vector::parse_distance_expr`. Follow that pattern
  rather than growing a full expression AST piecemeal.

## Known limitations (documented; don't accidentally regress)

- No JOIN, GROUP BY, sub-queries, OR in WHERE.
- Aggregates: only `COUNT(*)` (detected by prefix in `parse_select`).
- Table/column names in regexes are `\w+` — no quoted identifiers in
  statements other than CREATE TABLE column definitions.

## Testing

Unit tests live at the bottom of `src/parser.rs` — add one per new syntax
form (positive + at least one malformed input), plus an end-to-end test in
`tests/` since the executor is where mis-parses usually surface.

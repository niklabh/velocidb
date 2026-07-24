# VelociDB Agent Skills

Skills that teach AI coding agents (Claude Code, Cursor, etc.) how VelociDB
works, modeled on [Turso's `.claude/skills`](https://github.com/tursodatabase/turso/tree/main/.claude/skills).
Each skill is a `SKILL.md` with YAML frontmatter (`name`, `description`) that
agents load when working on the matching subsystem.

| Skill | Use when working on |
|-------|---------------------|
| [storage-format](storage-format/SKILL.md) | Page layout, WAL record format, row type tags, schema encoding |
| [transaction-correctness](transaction-correctness/SKILL.md) | Write groups, commit protocol, lock ordering, writer mutex |
| [async-io-model](async-io-model/SKILL.md) | Async API (`spawn_blocking` facade), rayon parallelism thresholds |
| [vector-search](vector-search/SKILL.md) | `F32_BLOB(n)` columns, distance functions, exact parallel KNN |
| [cdc](cdc/SKILL.md) | Change Data Capture log and executor hooks |
| [sql-parser](sql-parser/SKILL.md) | Extending the regex-based parser safely |
| [testing](testing/SKILL.md) | Test suites, commands, and required patterns |
| [code-quality](code-quality/SKILL.md) | Conventions, error handling, experimental-modules policy |
| [debugging](debugging/SKILL.md) | REPL repros, file inspection, failure-mode map |

Keep skills accurate: when you change an on-disk format, a threshold, an
invariant, or a public API described here, update the corresponding skill in
the same change.

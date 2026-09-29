---
name: storage-format
description: On-disk formats for VelociDB - page layout, WAL record format, B-tree node layout, row serialization type tags, and the schema page encoding. Use when modifying src/storage.rs, src/wal.rs, src/btree.rs row (de)serialization, adding a new Value variant or DataType, or debugging corruption / "Invalid type tag" / schema-truncation errors.
---

# VelociDB Storage Format

All multi-byte integers are little-endian. Changing any format below is a
breaking change for existing database files — update BOTH the writer and the
reader, and add a reopen test in `tests/` (see `test_vector_schema_survives_reopen`).

## Files

- `<db>`: the data file, an array of 4 KB pages (`PAGE_SIZE` in `src/storage.rs`).
- `<db>-wal`: write-ahead log (path built by `wal_path_for` in `src/wal.rs`; suffix
  is `-wal`, no dot).

## Page allocation

- Page 0: reserved root page.
- Page 1: head of the schema page chain; overflow pages anywhere (see below).
- Remaining pages: B-tree nodes, one node per page.

## WAL record format (`src/wal.rs`)

```text
[type: u8] [group_id: u64] [len: u32] [payload: len bytes] [crc32: u32]
```

- `type 1` = PAGE_WRITE, payload = `[page_id: u64][page_data: 4096]`
- `type 2` = COMMIT, empty payload
- CRC32 covers everything before it. Recovery stops at the first CRC mismatch
  or truncation (torn tail = never written). Only groups with a COMMIT record
  are replayed.
- Records are appended only by `Pager::commit_group` (one PAGE_WRITE per
  buffered page, sorted by page id, then COMMIT). Nothing is logged while a
  group — including a multi-statement transaction — is still open.

## B-tree node layout (`src/btree.rs`)

Every node starts with an 8-byte `NodeHeader`:

```text
[node_type: u8 (0=internal, 1=leaf)] [num_keys: u16] [parent: u32] [pad: u8]
```

`parent` is legacy: it is written as whatever the node had (0 for new
nodes) and never read — insert/delete navigate by the path recorded from
the root. Files written before 0.4 may hold stale parent values.

Internal nodes: `[child_0: u64] ([key_i: i64][child_{i+1}: u64])*`, at most
`BTREE_ORDER = 64` keys; child_i holds keys in `[key_{i-1}, key_i)`.
Leaves: cells `[key: i64][len: u32][row bytes]` in key order, limited by
bytes (`LEAF_CAPACITY = PAGE_SIZE - 8`); a row larger than a page is
rejected with "Row too large". Keys are `i64` primary keys.

## Row serialization type tags (`serialize_row` / `deserialize_row` in `src/btree.rs`)

| Tag | Type    | Payload                          |
|-----|---------|----------------------------------|
| 0   | Null    | none                             |
| 1   | Integer | i64                              |
| 2   | Float   | f64                              |
| 3   | Text    | `[len: u32][utf8 bytes]`         |
| 4   | Blob    | `[len: u32][bytes]`              |
| 5   | Vector  | `[dim: u32][dim x f32]`          |

Adding a `Value` variant requires: a new tag here, a `size_bytes` arm in
`src/types.rs`, a `Display` arm, and (if it has a column type) schema
persistence below.

## Schema page encoding (`save_schema` / `load_schema` in `src/storage.rs`)

The schema buffer is stored in a linked chain of pages starting at page 1:

```text
[chunk_len | 0x8000_0000: u32][next_page: u64 (0 = last)][chunk bytes]
```

`save_schema` reuses the chain's existing pages and allocates new ones as
it grows; it never writes a page the chain does not own. `schema_chain`
walks the chain (with cycle / bounds checks).

Legacy format (0.3 and earlier, read-only): `[chunk_len: u32][chunk]` on
*consecutive* pages from 1, ending at the first chunk shorter than
`PAGE_SIZE - 4`; recognized by the flag bit being clear. Writing it
overwrote B-tree pages once the schema outgrew one page, so it is never
written again — the next schema save converts to the chain (fixture:
`tests/fixtures/legacy_schema_v0_3.db`).

The concatenated buffer is:

```text
[num_tables: u32]
per table:
  [name_len: u32][name][num_cols: u32]
  per column:
    [col_name_len: u32][col_name]
    [data_type: u8]            -- 0=Integer 1=Real 2=Text 3=Blob 4=Null 5=Vector
    if data_type == 5: [dim: u32]
    [flags: u8]                -- bit0 primary_key, bit1 not_null, bit2 unique
    [root_page: u64]           -- B-tree root (same for every column of a table)
```

After the tables comes an optional index section (files from before
secondary indexes end after the tables; trailing bytes without the magic
are ignored because older versions could leave a stale page there):

```text
["VDBIDX01"][num_indexes: u32]
per index:
  [table_len: u32][table][name_len: u32][name][col_len: u32][col][root_page: u64]
```


## Secondary index B-trees (`src/index.rs`)

Ordinary B-trees: key = `(value_hash(value) << 32) | (pk & 0xFFFF_FFFF)`,
row = `[Integer(pk)]`. `value_hash` is FNV-1a over a tag byte plus the
`f64` bits (numbers, `-0.0` → `0.0`) or UTF-8 bytes (text), folded to 32
bits; it is persisted in keys, so **never change it** without a format
migration (a unit test pins one value). NULL / blob / vector values have
no entry.

The schema is re-saved after any CREATE/DROP/ALTER TABLE, CREATE/DROP INDEX and whenever a
statement changes a B-tree root page (detected by `snapshot_roots` diffing in
`Database::execute`).

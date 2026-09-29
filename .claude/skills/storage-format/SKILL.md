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
- Page 1..N: schema pages (chained; see below).
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

`BTREE_ORDER = 64` max keys, `MIN_KEYS = 32`. Keys are `i64` primary keys.
Leaf cells hold serialized rows.

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

Schema pages start at page 1 and chain: each page is
`[chunk_len: u32][chunk bytes]`; a chunk shorter than `PAGE_SIZE - 4` is the
last page. The concatenated buffer is:

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

The schema is re-saved after any CREATE/DROP/ALTER TABLE and whenever a
statement changes a B-tree root page (detected by `snapshot_roots` diffing in
`Database::execute`).

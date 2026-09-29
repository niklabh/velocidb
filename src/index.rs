//! Secondary indexes: single-column equality indexes stored as B-trees.
//!
//! An index is a [`BTree`] whose keys are
//! `(hash32(value) << 32) | (pk as u32)` and whose rows are `[pk]`. All
//! entries for values with the same hash form one contiguous key range (a
//! bucket), so an equality probe is a [`BTree::range`] over one bucket.
//! Colliding values share a bucket; callers re-check the full `WHERE` on the
//! table rows, so collisions cost time, never correctness.
//!
//! Putting the pk in the low bits makes an entry directly addressable, so
//! maintenance on UPDATE / DELETE is a point lookup even for low-cardinality
//! columns. Two pks of the same bucket that agree in their low 32 bits
//! collide; the later one takes the next free slot in the bucket, and removal
//! falls back to scanning the bucket when the home slot does not hold its pk.
//!
//! Only values that `=` can match are indexed: integers and floats (hashed
//! by their `f64` value, so `1` and `1.0` share an entry key, matching
//! [`crate::parser::Operator::Equal`]) and text. NULL, blobs and vectors are
//! never equal to anything under `=`, so rows holding them have no entry.

use crate::btree::BTree;
use crate::types::{Result, Row, Value, VelociError};

/// Metadata for one index, persisted with its table's schema.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSchema {
    pub name: String,
    pub column: String,
    pub root_page: crate::types::PageId,
}

const BUCKET_MASK: i64 = 0xFFFF_FFFF;

/// Stable 32-bit hash of the value's equality class, or `None` if `=` can
/// never match it.
pub fn value_hash(value: &Value) -> Option<u32> {
    // FNV-1a: stable across processes and Rust versions (unlike std's
    // hasher), which matters because hashes are persisted as keys.
    fn fnv1a(tag: u8, bytes: &[u8]) -> u32 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in std::iter::once(&tag).chain(bytes) {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (h ^ (h >> 32)) as u32
    }

    match value {
        Value::Integer(_) | Value::Float(_) | Value::Real(_) => {
            let f = value.as_float().ok()?;
            // -0.0 == 0.0; NaN never compares equal, so any bucket will do.
            let f = if f == 0.0 { 0.0 } else { f };
            Some(fnv1a(1, &f.to_bits().to_le_bytes()))
        }
        Value::Text(s) => Some(fnv1a(2, s.as_bytes())),
        _ => None,
    }
}

fn bucket_bounds(hash: u32) -> (i64, i64) {
    let lo = ((hash as u64) << 32) as i64;
    (lo, lo | BUCKET_MASK)
}

fn entry_pk(row: &Row) -> Result<i64> {
    match row.values.first() {
        Some(Value::Integer(pk)) => Ok(*pk),
        other => Err(VelociError::Corruption(format!(
            "Invalid index entry: {:?}",
            other
        ))),
    }
}

/// Adds the entry for (`value`, `pk`). Values that `=` cannot match are
/// skipped.
pub fn insert_entry(index: &mut BTree, value: &Value, pk: i64) -> Result<()> {
    let Some(hash) = value_hash(value) else {
        return Ok(());
    };
    let (lo, hi) = bucket_bounds(hash);
    let home = lo | (pk & BUCKET_MASK);
    let row = Row::new(vec![Value::Integer(pk)]);
    if index.search(home)?.is_none() {
        return index.insert(home, &row);
    }
    // Slot taken by a pk with the same low bits: use the first free slot
    // after it, wrapping within the bucket.
    let used: std::collections::HashSet<i64> =
        index.range(lo, hi)?.into_iter().map(|(k, _)| k).collect();
    let slot = (home..=hi)
        .chain(lo..home)
        .find(|k| !used.contains(k))
        .ok_or_else(|| VelociError::StorageError("Index bucket is full".to_string()))?;
    index.insert(slot, &row)
}

/// Removes the entry for (`value`, `pk`), if present.
pub fn remove_entry(index: &mut BTree, value: &Value, pk: i64) -> Result<()> {
    let Some(hash) = value_hash(value) else {
        return Ok(());
    };
    let (lo, hi) = bucket_bounds(hash);
    let home = lo | (pk & BUCKET_MASK);
    if let Some(row) = index.search(home)? {
        if entry_pk(&row)? == pk {
            index.delete(home)?;
            return Ok(());
        }
    }
    // Displaced by a collision. The home slot may be empty by now (its
    // occupant was removed), so scan the whole bucket rather than probing.
    for (key, row) in index.range(lo, hi)? {
        if entry_pk(&row)? == pk {
            index.delete(key)?;
            break;
        }
    }
    Ok(())
}

/// Primary keys of rows whose indexed value may equal `value` (a superset
/// when hashes collide), sorted and de-duplicated. `None` if `value` cannot
/// be looked up (the caller should scan).
pub fn probe(index: &BTree, value: &Value) -> Result<Option<Vec<i64>>> {
    let Some(hash) = value_hash(value) else {
        return Ok(None);
    };
    let (lo, hi) = bucket_bounds(hash);
    let mut pks = index
        .range(lo, hi)?
        .iter()
        .map(|(_, row)| entry_pk(row))
        .collect::<Result<Vec<i64>>>()?;
    pks.sort_unstable();
    pks.dedup();
    Ok(Some(pks))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Pager;
    use parking_lot::RwLock;
    use std::sync::Arc;
    use tempfile::NamedTempFile;

    #[test]
    fn test_value_hash_matches_equality() {
        assert_eq!(
            value_hash(&Value::Integer(1)),
            value_hash(&Value::Float(1.0))
        );
        assert_eq!(
            value_hash(&Value::Real(0.0)),
            value_hash(&Value::Float(-0.0))
        );
        assert_ne!(
            value_hash(&Value::Integer(1)),
            value_hash(&Value::Text("1".into()))
        );
        assert_eq!(value_hash(&Value::Null), None);
        assert_eq!(value_hash(&Value::Blob(vec![1])), None);
        // Stable across releases: this value is persisted in index keys.
        assert_eq!(value_hash(&Value::Text("abc".into())), Some(0xc701_5b64));
    }

    #[test]
    fn test_entries_with_colliding_low_bits() {
        let temp = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp.path()).unwrap()));
        pager.write().begin_group().unwrap();
        let mut index = BTree::new(Arc::clone(&pager)).unwrap();
        let v = Value::Text("same".into());
        // Same low 32 bits, same bucket.
        let pks = [5i64, 5 + (1 << 32), 5 + (2 << 32), 6];
        for pk in pks {
            insert_entry(&mut index, &v, pk).unwrap();
        }
        let mut sorted = pks.to_vec();
        sorted.sort();
        assert_eq!(probe(&index, &v).unwrap(), Some(sorted));

        // Remove the one in the home slot; the displaced ones stay findable.
        remove_entry(&mut index, &v, 5).unwrap();
        remove_entry(&mut index, &v, 5 + (2 << 32)).unwrap();
        assert_eq!(probe(&index, &v).unwrap(), Some(vec![6, 5 + (1 << 32)]));
        remove_entry(&mut index, &v, 5 + (1 << 32)).unwrap();
        remove_entry(&mut index, &v, 6).unwrap();
        remove_entry(&mut index, &v, 6).unwrap(); // absent: no-op
        assert_eq!(probe(&index, &v).unwrap(), Some(vec![]));
        assert_eq!(probe(&index, &Value::Null).unwrap(), None);
    }
}

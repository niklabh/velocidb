//! B-Tree index for primary key lookups and range scans.
//!
//! Supports insert, delete, point lookup, and range scan operations.
//! Keys are stored as `i64` with associated `Row` values in leaf pages.

use crate::storage::{Page, Pager, PAGE_SIZE};
use crate::types::{PageId, Result, Row, Value, VelociError};
use parking_lot::RwLock;
use std::sync::Arc;

const BTREE_ORDER: usize = 64; // Max keys per internal node
const MIN_KEYS: usize = BTREE_ORDER / 2; // Internal nodes below this are rebalanced
/// A leaf's cells: (key, serialized row).
type Cells = Vec<(i64, Vec<u8>)>;

/// Per-cell bytes in a leaf besides the row body: key (8) + body length (4).
const CELL_OVERHEAD: usize = 12;
/// Bytes available for cells in a leaf page.
const LEAF_CAPACITY: usize = PAGE_SIZE - NodeHeader::SIZE;
/// Guard against cycles in a corrupt file; 64-way fan-out never gets close.
const MAX_DEPTH: usize = 64;

fn cells_size(entries: &[(i64, Vec<u8>)]) -> usize {
    entries
        .iter()
        .map(|(_, body)| CELL_OVERHEAD + body.len())
        .sum()
}

/// The split index (both sides non-empty) that best balances bytes between
/// two leaves while both fit in a page, if any.
fn balanced_split(entries: &[(i64, Vec<u8>)]) -> Option<usize> {
    let total = cells_size(entries);
    let mut left = 0;
    let mut best: Option<(usize, usize)> = None;
    for (i, (_, body)) in entries
        .iter()
        .enumerate()
        .take(entries.len().saturating_sub(1))
    {
        left += CELL_OVERHEAD + body.len();
        let right = total - left;
        if left <= LEAF_CAPACITY && right <= LEAF_CAPACITY {
            let imbalance = left.abs_diff(right);
            if best.map_or(true, |(_, b)| imbalance < b) {
                best = Some((i + 1, imbalance));
            }
        }
    }
    best.map(|(i, _)| i)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum NodeType {
    Internal = 0,
    Leaf = 1,
}

#[repr(C)]
#[derive(Clone)]
pub struct NodeHeader {
    node_type: u8,
    num_keys: u16,
    parent: u32,
    _padding: u8,
}

impl NodeHeader {
    const SIZE: usize = 8;

    fn new(node_type: NodeType) -> Self {
        Self {
            node_type: node_type as u8,
            num_keys: 0,
            parent: 0,
            _padding: 0,
        }
    }

    pub fn new_leaf() -> Self {
        Self::new(NodeType::Leaf)
    }

    pub fn serialize(&self, buffer: &mut [u8]) {
        buffer[0] = self.node_type;
        buffer[1..3].copy_from_slice(&self.num_keys.to_le_bytes());
        buffer[3..7].copy_from_slice(&self.parent.to_le_bytes());
    }

    pub fn deserialize(buffer: &[u8]) -> Result<Self> {
        if buffer.len() < Self::SIZE {
            return Err(VelociError::Corruption(format!(
                "Buffer too small for NodeHeader: {} < {}",
                buffer.len(),
                Self::SIZE
            )));
        }

        Ok(Self {
            node_type: buffer[0],
            num_keys: u16::from_le_bytes([buffer[1], buffer[2]]),
            parent: u32::from_le_bytes([buffer[3], buffer[4], buffer[5], buffer[6]]),
            _padding: 0,
        })
    }
}

pub struct BTree {
    root_page: Arc<RwLock<PageId>>,
    pager: Arc<RwLock<Pager>>,
}

impl BTree {
    pub fn new(pager: Arc<RwLock<Pager>>) -> Result<Self> {
        let mut pager_lock = pager.write();
        let root_page = pager_lock.allocate_page()?;

        // Initialize root as leaf node
        let mut page = Page::new();
        let header = NodeHeader::new(NodeType::Leaf);
        header.serialize(page.data_mut());
        pager_lock.write_page(root_page, &page)?;

        drop(pager_lock);

        Ok(Self {
            root_page: Arc::new(RwLock::new(root_page)),
            pager,
        })
    }

    pub fn from_root(root_page: PageId, pager: Arc<RwLock<Pager>>) -> Self {
        Self {
            root_page: Arc::new(RwLock::new(root_page)),
            pager,
        }
    }

    /// Inserts `row` under `key`. Keys are expected to be unique; callers
    /// check with [`BTree::search`] first.
    ///
    /// Splits are propagated along the root-to-leaf path recorded while
    /// descending; the `parent` field of node headers is never read (it may
    /// be stale in files written by older versions).
    pub fn insert(&mut self, key: i64, row: &Row) -> Result<()> {
        let serialized = self.serialize_row(row)?;
        if CELL_OVERHEAD + serialized.len() > LEAF_CAPACITY {
            return Err(VelociError::StorageError(format!(
                "Row too large: {} bytes serialized (max {})",
                serialized.len(),
                LEAF_CAPACITY - CELL_OVERHEAD
            )));
        }

        let mut pager = self.pager.write();
        // Each pass either stores the cell or splits the leaf it belongs in,
        // so the loop ends once the target leaf has room.
        loop {
            let root = *self.root_page.read();
            let (leaf_id, path) = self.find_leaf_path(&mut pager, root, key)?;
            let (header, mut entries) = self.read_leaf(&mut pager, leaf_id)?;

            let pos = entries.partition_point(|(k, _)| *k <= key);
            entries.insert(pos, (key, serialized.clone()));
            if cells_size(&entries) <= LEAF_CAPACITY {
                return self.write_leaf(&mut pager, leaf_id, &header, &entries);
            }

            if let Some(split) = balanced_split(&entries) {
                // Split with the new cell included; both halves fit.
                let right = entries.split_off(split);
                return self.split_leaf(&mut pager, leaf_id, &header, &entries, &right, &path);
            }

            // No 2-way split of the cells plus the new one fits (several very
            // large cells). Split the existing cells, then retry.
            entries.remove(pos);
            let split = balanced_split(&entries).ok_or_else(|| {
                VelociError::Corruption(format!("Leaf {} cannot be split", leaf_id))
            })?;
            let right = entries.split_off(split);
            self.split_leaf(&mut pager, leaf_id, &header, &entries, &right, &path)?;
        }
    }

    /// Removes `key`; returns whether it was present.
    ///
    /// Underfull nodes on the recorded root-to-leaf path are merged with or
    /// rebalanced against a sibling: leaves by bytes (their cells vary in
    /// size), internal nodes by key count.
    pub fn delete(&mut self, key: i64) -> Result<bool> {
        let mut pager = self.pager.write();
        let root = *self.root_page.read();
        let (leaf_id, mut path) = self.find_leaf_path(&mut pager, root, key)?;
        let (header, mut entries) = self.read_leaf(&mut pager, leaf_id)?;

        let Some(pos) = entries.iter().position(|(k, _)| *k == key) else {
            return Ok(false);
        };
        entries.remove(pos);
        self.write_leaf(&mut pager, leaf_id, &header, &entries)?;

        let mut node = leaf_id;
        let mut is_leaf = true;
        while let Some(parent) = path.pop() {
            if !self.is_underfull(&mut pager, node, is_leaf)? {
                break;
            }
            self.rebalance_child(&mut pager, parent, node, is_leaf)?;
            node = parent;
            is_leaf = false;
        }

        // Collapse a root left with a single child.
        loop {
            let root = *self.root_page.read();
            let arc = pager.read_page(root)?;
            let header = NodeHeader::deserialize(arc.read().data())?;
            if header.node_type != NodeType::Internal as u8 || header.num_keys > 0 {
                break;
            }
            let (_, _, children) = self.read_internal(&mut pager, root)?;
            *self.root_page.write() = children[0];
        }

        Ok(true)
    }

    /// Descends from `root` to the leaf that holds (or would hold) `key`,
    /// returning it and the internal pages above it, root first.
    fn find_leaf_path(
        &self,
        pager: &mut Pager,
        root: PageId,
        key: i64,
    ) -> Result<(PageId, Vec<PageId>)> {
        let mut path = Vec::new();
        let mut page_id = root;
        loop {
            let arc = pager.read_page(page_id)?;
            let header = NodeHeader::deserialize(arc.read().data())?;
            if header.node_type == NodeType::Leaf as u8 {
                return Ok((page_id, path));
            }
            if path.len() > MAX_DEPTH {
                return Err(VelociError::Corruption(format!(
                    "B-tree deeper than {} levels (cycle?)",
                    MAX_DEPTH
                )));
            }
            let (_, keys, children) = self.read_internal(pager, page_id)?;
            path.push(page_id);
            page_id = children[keys.partition_point(|k| *k <= key)];
        }
    }

    fn read_leaf(&self, pager: &mut Pager, page_id: PageId) -> Result<(NodeHeader, Cells)> {
        let arc = pager.read_page(page_id)?;
        let page = arc.read();
        let data = page.data();
        let header = NodeHeader::deserialize(data)?;
        if header.node_type != NodeType::Leaf as u8 {
            return Err(VelociError::Corruption(format!(
                "Page {} is not a leaf",
                page_id
            )));
        }
        let mut entries = Vec::with_capacity(header.num_keys as usize);
        let mut offset = NodeHeader::SIZE;
        for _ in 0..header.num_keys {
            let cell = data
                .get(offset..offset + CELL_OVERHEAD)
                .ok_or_else(|| VelociError::Corruption(format!("Leaf {} truncated", page_id)))?;
            let key = i64::from_le_bytes(cell[0..8].try_into().unwrap());
            let size = u32::from_le_bytes(cell[8..12].try_into().unwrap()) as usize;
            let body = data
                .get(offset + CELL_OVERHEAD..offset + CELL_OVERHEAD + size)
                .ok_or_else(|| VelociError::Corruption(format!("Leaf {} truncated", page_id)))?;
            entries.push((key, body.to_vec()));
            offset += CELL_OVERHEAD + size;
        }
        Ok((header, entries))
    }

    fn write_leaf(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        header: &NodeHeader,
        entries: &[(i64, Vec<u8>)],
    ) -> Result<()> {
        if cells_size(entries) > LEAF_CAPACITY {
            return Err(VelociError::StorageError(format!(
                "Leaf {} overflow",
                page_id
            )));
        }
        let mut page = Page::new();
        let mut header = header.clone();
        header.node_type = NodeType::Leaf as u8;
        header.num_keys = entries.len() as u16;
        header.serialize(page.data_mut());
        let data = page.data_mut();
        let mut offset = NodeHeader::SIZE;
        for (key, body) in entries {
            data[offset..offset + 8].copy_from_slice(&key.to_le_bytes());
            data[offset + 8..offset + 12].copy_from_slice(&(body.len() as u32).to_le_bytes());
            data[offset + CELL_OVERHEAD..offset + CELL_OVERHEAD + body.len()].copy_from_slice(body);
            offset += CELL_OVERHEAD + body.len();
        }
        pager.write_page(page_id, &page)
    }

    fn read_internal(
        &self,
        pager: &mut Pager,
        page_id: PageId,
    ) -> Result<(NodeHeader, Vec<i64>, Vec<PageId>)> {
        let arc = pager.read_page(page_id)?;
        let page = arc.read();
        let header = NodeHeader::deserialize(page.data())?;
        if header.node_type != NodeType::Internal as u8
            || NodeHeader::SIZE + 8 + header.num_keys as usize * 16 > PAGE_SIZE
        {
            return Err(VelociError::Corruption(format!(
                "Page {} is not a valid internal node",
                page_id
            )));
        }
        let (keys, children) = self.parse_internal_node(page.data(), &header)?;
        Ok((header, keys, children))
    }

    fn write_internal(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        header: &NodeHeader,
        keys: &[i64],
        children: &[PageId],
    ) -> Result<()> {
        let mut page = Page::new();
        let mut header = header.clone();
        header.node_type = NodeType::Internal as u8;
        header.num_keys = keys.len() as u16;
        header.serialize(page.data_mut());
        self.write_internal_body(&mut page, keys, children)?;
        pager.write_page(page_id, &page)
    }

    /// Writes `left` back to `page_id`, `right` to a new page, and links the
    /// new page into the parent.
    fn split_leaf(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        header: &NodeHeader,
        left: &[(i64, Vec<u8>)],
        right: &[(i64, Vec<u8>)],
        path: &[PageId],
    ) -> Result<()> {
        let right_id = pager.allocate_page()?;
        self.write_leaf(pager, page_id, header, left)?;
        self.write_leaf(pager, right_id, &NodeHeader::new_leaf(), right)?;
        self.insert_separator(pager, path, page_id, right[0].0, right_id)
    }

    /// Adds `key` / `right` just after child `left` in the last page of
    /// `path` (the parent of `left`), splitting upward as needed. An empty
    /// path means `left` is the root, so a new root is created.
    fn insert_separator(
        &self,
        pager: &mut Pager,
        path: &[PageId],
        left: PageId,
        key: i64,
        right: PageId,
    ) -> Result<()> {
        let Some((&parent, ancestors)) = path.split_last() else {
            let root_id = pager.allocate_page()?;
            let header = NodeHeader::new(NodeType::Internal);
            self.write_internal(pager, root_id, &header, &[key], &[left, right])?;
            *self.root_page.write() = root_id;
            return Ok(());
        };

        let (header, mut keys, mut children) = self.read_internal(pager, parent)?;
        let index = children.iter().position(|&c| c == left).ok_or_else(|| {
            VelociError::Corruption(format!("Page {} is not a child of {}", left, parent))
        })?;
        keys.insert(index, key);
        children.insert(index + 1, right);

        if keys.len() <= BTREE_ORDER {
            return self.write_internal(pager, parent, &header, &keys, &children);
        }

        let mid = keys.len() / 2;
        let promoted = keys[mid];
        let right_keys = keys.split_off(mid + 1);
        keys.truncate(mid);
        let right_children = children.split_off(mid + 1);
        let sibling = pager.allocate_page()?;
        self.write_internal(pager, parent, &header, &keys, &children)?;
        self.write_internal(
            pager,
            sibling,
            &NodeHeader::new(NodeType::Internal),
            &right_keys,
            &right_children,
        )?;
        self.insert_separator(pager, ancestors, parent, promoted, sibling)
    }

    fn is_underfull(&self, pager: &mut Pager, page_id: PageId, is_leaf: bool) -> Result<bool> {
        if is_leaf {
            let (_, entries) = self.read_leaf(pager, page_id)?;
            Ok(cells_size(&entries) < LEAF_CAPACITY / 4)
        } else {
            let (_, keys, _) = self.read_internal(pager, page_id)?;
            Ok(keys.len() < MIN_KEYS)
        }
    }

    /// Fixes underfull `child` of `parent` by merging it with an adjacent
    /// sibling, or by rebalancing the pair when the merge would not fit.
    fn rebalance_child(
        &self,
        pager: &mut Pager,
        parent: PageId,
        child: PageId,
        is_leaf: bool,
    ) -> Result<()> {
        let (parent_header, mut keys, mut children) = self.read_internal(pager, parent)?;
        let index = children.iter().position(|&c| c == child).ok_or_else(|| {
            VelociError::Corruption(format!("Page {} is not a child of {}", child, parent))
        })?;
        if children.len() < 2 {
            return Ok(()); // Single-child root; collapsed by the caller.
        }
        // Pair the child with its left sibling if it has one.
        let li = if index > 0 { index - 1 } else { index };
        let (left_id, right_id) = (children[li], children[li + 1]);

        if is_leaf {
            let (left_header, mut left) = self.read_leaf(pager, left_id)?;
            let (_, right) = self.read_leaf(pager, right_id)?;
            left.extend(right);
            if cells_size(&left) <= LEAF_CAPACITY {
                self.write_leaf(pager, left_id, &left_header, &left)?;
                keys.remove(li);
                children.remove(li + 1);
            } else {
                let split = balanced_split(&left).ok_or_else(|| {
                    VelociError::Corruption("Sibling leaves cannot be rebalanced".to_string())
                })?;
                let right = left.split_off(split);
                keys[li] = right[0].0;
                self.write_leaf(pager, left_id, &left_header, &left)?;
                self.write_leaf(pager, right_id, &NodeHeader::new_leaf(), &right)?;
            }
        } else {
            let (left_header, mut left_keys, mut left_children) =
                self.read_internal(pager, left_id)?;
            let (right_header, right_keys, right_children) = self.read_internal(pager, right_id)?;
            left_keys.push(keys[li]);
            left_keys.extend(right_keys);
            left_children.extend(right_children);
            if left_keys.len() <= BTREE_ORDER {
                self.write_internal(pager, left_id, &left_header, &left_keys, &left_children)?;
                keys.remove(li);
                children.remove(li + 1);
            } else {
                let mid = left_keys.len() / 2;
                keys[li] = left_keys[mid];
                let new_right_keys = left_keys.split_off(mid + 1);
                left_keys.truncate(mid);
                let new_right_children = left_children.split_off(mid + 1);
                self.write_internal(pager, left_id, &left_header, &left_keys, &left_children)?;
                self.write_internal(
                    pager,
                    right_id,
                    &right_header,
                    &new_right_keys,
                    &new_right_children,
                )?;
            }
        }

        self.write_internal(pager, parent, &parent_header, &keys, &children)
    }

    /// Checks structural invariants (test support): key order, separator
    /// bounds, uniform leaf depth, and node sizes. Returns the entry count.
    #[cfg(test)]
    fn check_invariants(&self) -> Result<usize> {
        let mut pager = self.pager.write();
        let root = *self.root_page.read();
        let mut leaf_depth = None;
        self.check_node(&mut pager, root, None, None, 0, &mut leaf_depth, true)
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn check_node(
        &self,
        pager: &mut Pager,
        page_id: PageId,
        lo: Option<i64>,
        hi: Option<i64>,
        depth: usize,
        leaf_depth: &mut Option<usize>,
        is_root: bool,
    ) -> Result<usize> {
        let fail = |msg: String| Err(VelociError::Corruption(msg));
        let in_bounds = |k: i64| lo.map_or(true, |l| k >= l) && hi.map_or(true, |h| k < h);
        let arc = pager.read_page(page_id)?;
        let header = NodeHeader::deserialize(arc.read().data())?;
        drop(arc);
        if header.node_type == NodeType::Leaf as u8 {
            let (_, entries) = self.read_leaf(pager, page_id)?;
            if *leaf_depth.get_or_insert(depth) != depth {
                return fail(format!("leaf {} at depth {}", page_id, depth));
            }
            if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
                return fail(format!("leaf {} keys out of order", page_id));
            }
            if let Some((k, _)) = entries.iter().find(|(k, _)| !in_bounds(*k)) {
                return fail(format!(
                    "leaf {} key {} outside {:?}..{:?}",
                    page_id, k, lo, hi
                ));
            }
            return Ok(entries.len());
        }
        let (_, keys, children) = self.read_internal(pager, page_id)?;
        if keys.is_empty() || keys.len() > BTREE_ORDER {
            return fail(format!("internal {} has {} keys", page_id, keys.len()));
        }
        if !is_root && keys.len() < MIN_KEYS / 2 {
            return fail(format!(
                "internal {} underfull: {} keys",
                page_id,
                keys.len()
            ));
        }
        if !keys.windows(2).all(|w| w[0] < w[1]) || !keys.iter().all(|k| in_bounds(*k)) {
            return fail(format!("internal {} keys out of order or bounds", page_id));
        }
        let mut total = 0;
        for (i, &child) in children.iter().enumerate() {
            let clo = if i == 0 { lo } else { Some(keys[i - 1]) };
            let chi = if i == keys.len() { hi } else { Some(keys[i]) };
            total += self.check_node(pager, child, clo, chi, depth + 1, leaf_depth, false)?;
        }
        Ok(total)
    }

    pub fn search(&self, key: i64) -> Result<Option<Row>> {
        let mut pager = self.pager.write();
        let root_page = *self.root_page.read();

        let leaf_page_id = self.find_leaf(&mut pager, root_page, key)?;
        let page_arc = pager.read_page(leaf_page_id)?;
        let page = page_arc.read();

        let header = NodeHeader::deserialize(page.data())?;
        let num_keys = header.num_keys as usize;

        // Binary search for the key
        let mut offset = NodeHeader::SIZE;
        for _ in 0..num_keys {
            let stored_key = i64::from_le_bytes(
                page.data()[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );

            let size = u32::from_le_bytes(
                page.data()[offset + 8..offset + 12]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
            ) as usize;

            if stored_key == key {
                let data_start = offset + 12;
                let data_end = data_start + size;
                let data = &page.data()[data_start..data_end];
                return Ok(Some(self.deserialize_row(data)?));
            }

            offset += 12 + size;
        }

        Ok(None)
    }

    pub fn scan(&self) -> Result<Vec<(i64, Row)>> {
        let mut results = Vec::new();
        let root_page = *self.root_page.read();

        // Recursively collect all entries from all leaf nodes
        self.scan_node(root_page, &mut results)?;

        Ok(results)
    }

    /// All entries with `lo <= key <= hi`, in key order. Only subtrees whose
    /// key interval overlaps `[lo, hi]` are visited.
    pub fn range(&self, lo: i64, hi: i64) -> Result<Vec<(i64, Row)>> {
        let mut results = Vec::new();
        if lo <= hi {
            let root_page = *self.root_page.read();
            self.range_node(root_page, lo, hi, &mut results)?;
        }
        Ok(results)
    }

    fn range_node(
        &self,
        page_id: PageId,
        lo: i64,
        hi: i64,
        results: &mut Vec<(i64, Row)>,
    ) -> Result<()> {
        let page_data = {
            let mut pager = self.pager.write();
            let page_arc = pager.read_page(page_id)?;
            let page = page_arc.read();
            page.data().to_vec()
        };
        let header = NodeHeader::deserialize(&page_data)?;
        let read_u64 = |offset: usize| -> Result<u64> {
            page_data
                .get(offset..offset + 8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(|| VelociError::Corruption("Truncated B-tree node".to_string()))
        };

        if header.node_type == NodeType::Leaf as u8 {
            let mut offset = NodeHeader::SIZE;
            for _ in 0..header.num_keys {
                let key = read_u64(offset)? as i64;
                let size = page_data
                    .get(offset + 8..offset + 12)
                    .and_then(|b| b.try_into().ok())
                    .map(u32::from_le_bytes)
                    .ok_or_else(|| VelociError::Corruption("Invalid size".to_string()))?
                    as usize;
                if key > hi {
                    break;
                }
                if key >= lo {
                    let data = page_data
                        .get(offset + 12..offset + 12 + size)
                        .ok_or_else(|| VelociError::Corruption("Invalid cell".to_string()))?;
                    results.push((key, self.deserialize_row(data)?));
                }
                offset += 12 + size;
            }
            return Ok(());
        }

        // Internal node: [child_0][key_0][child_1]...[key_{n-1}][child_n];
        // child_i holds keys in [key_{i-1}, key_i).
        let num_keys = header.num_keys as usize;
        let mut offset = NodeHeader::SIZE;
        let mut child = read_u64(offset)? as PageId;
        offset += 8;
        let mut lower: Option<i64> = None;
        for i in 0..=num_keys {
            let (upper, next_child) = if i < num_keys {
                let key = read_u64(offset)? as i64;
                let next = read_u64(offset + 8)? as PageId;
                offset += 16;
                (Some(key), Some(next))
            } else {
                (None, None)
            };
            let overlaps = lower.map_or(true, |l| l <= hi) && upper.map_or(true, |u| u > lo);
            if overlaps {
                self.range_node(child, lo, hi, results)?;
            }
            if lower.is_some_and(|l| l > hi) {
                break;
            }
            match next_child {
                Some(next) => child = next,
                None => break,
            }
            lower = upper;
        }
        Ok(())
    }

    fn scan_node(&self, page_id: PageId, results: &mut Vec<(i64, Row)>) -> Result<()> {
        let mut pager = self.pager.write();
        let page_arc = pager.read_page(page_id)?;
        let page_data = {
            let page = page_arc.read();
            page.data().to_vec()
        };
        drop(pager);

        let header = NodeHeader::deserialize(&page_data)?;

        if header.node_type == NodeType::Leaf as u8 {
            // Read all entries from this leaf
            let mut offset = NodeHeader::SIZE;
            for _ in 0..header.num_keys {
                let key = i64::from_le_bytes(
                    page_data[offset..offset + 8]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );

                let size = u32::from_le_bytes(
                    page_data[offset + 8..offset + 12]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
                ) as usize;

                let data = &page_data[offset + 12..offset + 12 + size];
                let row = self.deserialize_row(data)?;
                results.push((key, row));

                offset += 12 + size;
            }
        } else {
            // Internal node - visit all children
            // Format: [child_0][key_0][child_1][key_1]...[key_{n-1}][child_n]
            let mut offset = NodeHeader::SIZE;

            // First child
            let first_child = u64::from_le_bytes(
                page_data[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
            ) as PageId;
            offset += 8;

            self.scan_node(first_child, results)?;

            // Remaining key/child pairs
            for _ in 0..header.num_keys {
                // Skip the key (8 bytes)
                offset += 8;

                let child =
                    u64::from_le_bytes(page_data[offset..offset + 8].try_into().map_err(|_| {
                        VelociError::Corruption("Invalid child pointer".to_string())
                    })?) as PageId;
                offset += 8;

                self.scan_node(child, results)?;
            }
        }

        Ok(())
    }

    /// Reads an internal node's keys and child pointers into vectors for easier
    /// manipulation during merge/redistribute.
    fn parse_internal_node(
        &self,
        data: &[u8],
        header: &NodeHeader,
    ) -> Result<(Vec<i64>, Vec<PageId>)> {
        let mut keys = Vec::with_capacity(header.num_keys as usize);
        let mut children = Vec::with_capacity(header.num_keys as usize + 1);
        let mut offset = NodeHeader::SIZE;

        let first_child = u64::from_le_bytes(
            data[offset..offset + 8]
                .try_into()
                .map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
        ) as PageId;
        children.push(first_child);
        offset += 8;

        for _ in 0..header.num_keys {
            let k = i64::from_le_bytes(
                data[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );
            keys.push(k);
            let c = u64::from_le_bytes(
                data[offset + 8..offset + 16]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
            ) as PageId;
            children.push(c);
            offset += 16;
        }

        Ok((keys, children))
    }

    /// Writes the body of an internal node (children + interleaved keys). The
    /// caller is responsible for writing the `NodeHeader`.
    fn write_internal_body(
        &self,
        page: &mut Page,
        keys: &[i64],
        children: &[PageId],
    ) -> Result<()> {
        if children.len() != keys.len() + 1 {
            return Err(VelociError::Corruption(format!(
                "Internal node invariant violated: {} keys but {} children",
                keys.len(),
                children.len()
            )));
        }
        let needed = NodeHeader::SIZE + 8 + keys.len() * 16;
        if needed > PAGE_SIZE {
            return Err(VelociError::StorageError(
                "Internal node would exceed page size".to_string(),
            ));
        }
        let mut offset = NodeHeader::SIZE;
        page.data_mut()[offset..offset + 8].copy_from_slice(&children[0].to_le_bytes());
        offset += 8;
        for (k, c) in keys.iter().zip(children.iter().skip(1)) {
            page.data_mut()[offset..offset + 8].copy_from_slice(&k.to_le_bytes());
            page.data_mut()[offset + 8..offset + 16].copy_from_slice(&(*c).to_le_bytes());
            offset += 16;
        }
        Ok(())
    }

    fn find_leaf(&self, pager: &mut Pager, root_page: PageId, key: i64) -> Result<PageId> {
        let mut page_id = root_page;

        loop {
            let page_arc = pager.read_page(page_id)?;
            let page = page_arc.read();
            let header = NodeHeader::deserialize(page.data())?;

            if header.node_type == NodeType::Leaf as u8 {
                return Ok(page_id);
            }

            // Internal node - find the child to descend to
            // Format: [child (8 bytes)][key (8 bytes)][child (8 bytes)][key (8 bytes)]...
            let mut offset = NodeHeader::SIZE;
            let mut child_page = u64::from_le_bytes(
                page.data()[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
            ) as PageId;

            offset += 8;

            for _ in 0..header.num_keys {
                let stored_key = i64::from_le_bytes(
                    page.data()[offset..offset + 8]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );

                let next_child =
                    u64::from_le_bytes(page.data()[offset + 8..offset + 16].try_into().map_err(
                        |_| VelociError::Corruption("Invalid child pointer".to_string()),
                    )?) as PageId;

                if key < stored_key {
                    break;
                }

                child_page = next_child;
                offset += 16;
            }

            page_id = child_page;
        }
    }

    fn serialize_row(&self, row: &Row) -> Result<Vec<u8>> {
        let mut buffer = Vec::new();

        // Number of values
        buffer.extend_from_slice(&(row.values.len() as u32).to_le_bytes());

        for value in &row.values {
            match value {
                Value::Null => {
                    buffer.push(0);
                }
                Value::Integer(i) => {
                    buffer.push(1);
                    buffer.extend_from_slice(&i.to_le_bytes());
                }
                Value::Float(f) | Value::Real(f) => {
                    buffer.push(2);
                    buffer.extend_from_slice(&f.to_le_bytes());
                }
                Value::Text(s) => {
                    buffer.push(3);
                    buffer.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    buffer.extend_from_slice(s.as_bytes());
                }
                Value::Blob(b) => {
                    buffer.push(4);
                    buffer.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    buffer.extend_from_slice(b);
                }
                Value::Vector(v) => {
                    buffer.push(5);
                    buffer.extend_from_slice(&(v.len() as u32).to_le_bytes());
                    for x in v {
                        buffer.extend_from_slice(&x.to_le_bytes());
                    }
                }
            }
        }

        Ok(buffer)
    }

    fn deserialize_row(&self, data: &[u8]) -> Result<Row> {
        let mut offset = 0;

        let num_values = u32::from_le_bytes(
            data[offset..offset + 4]
                .try_into()
                .map_err(|_| VelociError::Corruption("Invalid value count".to_string()))?,
        ) as usize;
        offset += 4;

        let mut values = Vec::with_capacity(num_values);

        for _ in 0..num_values {
            let type_tag = data[offset];
            offset += 1;

            match type_tag {
                0 => values.push(Value::Null),
                1 => {
                    let i = i64::from_le_bytes(
                        data[offset..offset + 8]
                            .try_into()
                            .map_err(|_| VelociError::Corruption("Invalid integer".to_string()))?,
                    );
                    offset += 8;
                    values.push(Value::Integer(i));
                }
                2 => {
                    let f = f64::from_le_bytes(
                        data[offset..offset + 8]
                            .try_into()
                            .map_err(|_| VelociError::Corruption("Invalid float".to_string()))?,
                    );
                    offset += 8;
                    values.push(Value::Float(f));
                }
                3 => {
                    let len =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
                            VelociError::Corruption("Invalid text length".to_string())
                        })?) as usize;
                    offset += 4;
                    let s = String::from_utf8(data[offset..offset + len].to_vec())
                        .map_err(|_| VelociError::Corruption("Invalid UTF-8".to_string()))?;
                    offset += len;
                    values.push(Value::Text(s));
                }
                4 => {
                    let len =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
                            VelociError::Corruption("Invalid blob length".to_string())
                        })?) as usize;
                    offset += 4;
                    let b = data[offset..offset + len].to_vec();
                    offset += len;
                    values.push(Value::Blob(b));
                }
                5 => {
                    let dim =
                        u32::from_le_bytes(data[offset..offset + 4].try_into().map_err(|_| {
                            VelociError::Corruption("Invalid vector dimension".to_string())
                        })?) as usize;
                    offset += 4;
                    if offset + dim * 4 > data.len() {
                        return Err(VelociError::Corruption("Vector data truncated".to_string()));
                    }
                    let mut v = Vec::with_capacity(dim);
                    for i in 0..dim {
                        let start = offset + i * 4;
                        v.push(f32::from_le_bytes(
                            data[start..start + 4].try_into().map_err(|_| {
                                VelociError::Corruption("Invalid vector component".to_string())
                            })?,
                        ));
                    }
                    offset += dim * 4;
                    values.push(Value::Vector(v));
                }
                _ => {
                    return Err(VelociError::Corruption(format!(
                        "Invalid type tag: {}",
                        type_tag
                    )))
                }
            }
        }

        Ok(Row { values })
    }

    pub fn root_page(&self) -> PageId {
        *self.root_page.read()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Pager;
    use tempfile::NamedTempFile;

    #[test]
    fn test_btree_create() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let btree = BTree::new(pager).unwrap();
        assert!(btree.scan().unwrap().is_empty());
    }

    #[test]
    fn test_insert_and_search() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let mut btree = BTree::new(pager).unwrap();

        let row = Row::new(vec![Value::Integer(1), Value::Text("Alice".to_string())]);
        btree.insert(1, &row).unwrap();

        let result = btree.search(1).unwrap();
        assert!(result.is_some());
        let found_row = result.unwrap();
        assert_eq!(found_row.values.len(), 2);
    }

    #[test]
    fn test_delete() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let mut btree = BTree::new(pager).unwrap();

        let row = Row::new(vec![Value::Integer(1), Value::Text("Alice".to_string())]);
        btree.insert(1, &row).unwrap();

        let deleted = btree.delete(1).unwrap();
        assert!(deleted);

        let result = btree.search(1).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_large_dataset() {
        // Test that we can insert a large number of records (triggering multiple splits)
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let mut btree = BTree::new(pager).unwrap();

        // Insert 1000 records
        for i in 0..1000 {
            let row = Row::new(vec![
                Value::Integer(i),
                Value::Text(format!("Record {}", i)),
            ]);
            btree.insert(i, &row).unwrap();
        }

        // Verify all records can be retrieved
        for i in 0..1000 {
            let result = btree.search(i).unwrap();
            assert!(result.is_some(), "Failed to find record {}", i);
            let row = result.unwrap();
            assert_eq!(row.values.len(), 2);
        }
    }

    #[test]
    fn test_moderate_dataset() {
        // Test with a moderate dataset that should work within limits
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let mut btree = BTree::new(pager).unwrap();

        // Insert 50 records - should be well within limits
        for i in 0..50 {
            let row = Row::new(vec![
                Value::Integer(i),
                Value::Text(format!("Record {}", i)),
            ]);
            btree.insert(i, &row).unwrap();
        }

        // Verify all records can be retrieved
        for i in 0..50 {
            let result = btree.search(i).unwrap();
            assert!(result.is_some(), "Failed to find record {}", i);
            let row = result.unwrap();
            assert_eq!(row.values.len(), 2);
        }

        // Test non-existent keys
        assert!(btree.search(100).unwrap().is_none());
    }

    /// Stress test that mixes inserts and deletes across enough keys to span
    /// multiple internal node levels, then verifies that scan output exactly
    /// matches the model (a `BTreeMap` we maintain in parallel).
    /// Regression: deletes used to fail ("Cannot merge: target page full",
    /// "Parent-child link corruption") once rows were larger than a few
    /// bytes, and a split after the root collapsed linked the new sibling
    /// into a dead page, losing rows.
    #[test]
    fn test_delete_all_then_reinsert_with_wide_rows() {
        for pad in [0usize, 40, 96, 500, 4000] {
            let temp_file = NamedTempFile::new().unwrap();
            let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
            // One write group: standalone page writes would each fsync.
            pager.write().begin_group().unwrap();
            let mut btree = BTree::new(Arc::clone(&pager)).unwrap();
            let row = |k: i64| Row::new(vec![Value::Integer(k), Value::Text("x".repeat(pad))]);

            let mut keys: Vec<i64> = (0..600).collect();
            for &k in &keys {
                btree.insert(k, &row(k)).unwrap();
            }
            let mut state = 12345u64;
            for i in (1..keys.len()).rev() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                keys.swap(i, (state % (i as u64 + 1)) as usize);
            }
            for (i, &k) in keys.iter().enumerate() {
                assert!(
                    btree.delete(k).unwrap(),
                    "pad {} delete #{} of {}",
                    pad,
                    i,
                    k
                );
            }
            assert_eq!(btree.check_invariants().unwrap(), 0);

            for &k in &keys {
                btree.insert(k, &row(k)).unwrap();
            }
            assert_eq!(btree.check_invariants().unwrap(), 600, "pad {}", pad);
            for k in 0..600 {
                assert_eq!(
                    btree.search(k).unwrap().map(|r| r.values),
                    Some(row(k).values),
                    "pad {} key {}",
                    pad,
                    k
                );
            }
        }
    }

    #[test]
    fn test_row_too_large_is_rejected() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let mut btree = BTree::new(pager).unwrap();
        let row = Row::new(vec![Value::Text("x".repeat(PAGE_SIZE))]);
        assert!(matches!(
            btree.insert(1, &row),
            Err(VelociError::StorageError(_))
        ));
        assert_eq!(btree.check_invariants().unwrap(), 0);
    }

    #[test]
    fn test_insert_delete_mixed_invariants() {
        let temp_file = NamedTempFile::new().unwrap();
        let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
        let mut btree = BTree::new(pager).unwrap();

        let mut model: std::collections::BTreeMap<i64, Row> = std::collections::BTreeMap::new();

        // First grow the tree to ~3 levels of internal nodes.
        for i in 0..2_000i64 {
            let row = Row::new(vec![Value::Integer(i), Value::Text(format!("R{}", i))]);
            btree.insert(i, &row).unwrap();
            model.insert(i, row);
        }

        // Now delete every third key. With BTREE_ORDER=64 and MIN_KEYS=32 this
        // is enough to exercise internal-node merge/redistribute paths.
        for i in (0..2_000i64).step_by(3) {
            assert!(btree.delete(i).unwrap(), "delete {} returned false", i);
            model.remove(&i);
        }

        // Re-insert some of the deleted keys with different payloads.
        for i in (0..2_000i64).step_by(7) {
            model.entry(i).or_insert_with(|| {
                let row = Row::new(vec![Value::Integer(i), Value::Text(format!("X{}", i))]);
                btree.insert(i, &row).unwrap();
                row
            });
        }

        // Point lookups must agree.
        for (k, expected) in model.iter() {
            let got = btree.search(*k).unwrap();
            assert!(got.is_some(), "missing key {}", k);
            let got = got.unwrap();
            assert_eq!(
                got.values.len(),
                expected.values.len(),
                "value count mismatch at {}",
                k
            );
            assert_eq!(
                got.values[0], expected.values[0],
                "pk value mismatch at {}",
                k
            );
            assert_eq!(
                got.values[1], expected.values[1],
                "payload mismatch at {}",
                k
            );
        }

        // Full scan must contain exactly the model keys.
        let scan_keys: std::collections::BTreeSet<i64> =
            btree.scan().unwrap().into_iter().map(|(k, _)| k).collect();
        let model_keys: std::collections::BTreeSet<i64> = model.keys().copied().collect();
        assert_eq!(scan_keys, model_keys, "scan keys do not match model");
    }

    // Property test: any random sequence of inserts and deletes leaves the
    // B-tree consistent with a parallel in-memory model.
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 16, max_global_rejects: 1000, .. ProptestConfig::default() })]

        #[test]
        fn proptest_btree_matches_model(
            ops in proptest::collection::vec(
                (any::<bool>(), 0i64..600i64),
                1..400usize,
            )
        ) {
            let temp_file = NamedTempFile::new().unwrap();
            let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
            let mut btree = BTree::new(pager).unwrap();
            let mut model: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();

            for (is_insert, key) in ops {
                if is_insert {
                    let row = Row::new(vec![Value::Integer(key)]);
                    model.entry(key).or_insert_with(|| {
                        btree.insert(key, &row).unwrap();
                        key
                    });
                } else if model.remove(&key).is_some() {
                    assert!(btree.delete(key).unwrap());
                }
            }

            let scan_keys: std::collections::BTreeSet<i64> =
                btree.scan().unwrap().into_iter().map(|(k, _)| k).collect();
            let model_keys: std::collections::BTreeSet<i64> = model.keys().copied().collect();
            prop_assert_eq!(scan_keys, model_keys);
            prop_assert_eq!(btree.check_invariants().unwrap(), model.len());
        }

        #[test]
        fn proptest_btree_mixed_row_sizes(
            ops in proptest::collection::vec(
                (any::<bool>(), 0i64..400, prop_oneof![0usize..64, 64usize..400, 400usize..1500]),
                1..600usize,
            )
        ) {
            let temp_file = NamedTempFile::new().unwrap();
            let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
            // One write group: standalone page writes would each fsync.
            pager.write().begin_group().unwrap();
            let mut btree = BTree::new(Arc::clone(&pager)).unwrap();
            let mut model: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();

            for (i, (is_insert, key, pad)) in ops.into_iter().enumerate() {
                if is_insert {
                    if let std::collections::btree_map::Entry::Vacant(e) = model.entry(key) {
                        e.insert(pad);
                        let row = Row::new(vec![Value::Integer(key), Value::Text("p".repeat(pad))]);
                        btree.insert(key, &row).unwrap();
                    }
                } else {
                    prop_assert_eq!(btree.delete(key).unwrap(), model.remove(&key).is_some());
                }
                if i % 16 == 0 {
                    prop_assert_eq!(btree.check_invariants().unwrap(), model.len());
                }
            }

            prop_assert_eq!(btree.check_invariants().unwrap(), model.len());
            let got: Vec<(i64, usize)> = btree
                .scan()
                .unwrap()
                .into_iter()
                .map(|(k, row)| match &row.values[1] {
                    Value::Text(t) => (k, t.len()),
                    other => panic!("unexpected {:?}", other),
                })
                .collect();
            let want: Vec<(i64, usize)> = model.into_iter().collect();
            prop_assert_eq!(got, want);
        }

        #[test]
        fn proptest_btree_range_matches_model(
            keys in proptest::collection::vec(any::<i64>().prop_map(|k| k % 5000), 1..500usize),
            deletes in proptest::collection::vec(any::<prop::sample::Index>(), 0..200usize),
            bounds in proptest::collection::vec((-6000i64..6000, 0i64..3000), 1..20usize),
        ) {
            let temp_file = NamedTempFile::new().unwrap();
            let pager = Arc::new(RwLock::new(Pager::new(temp_file.path()).unwrap()));
            // One write group: standalone page writes would each fsync.
            pager.write().begin_group().unwrap();
            let mut btree = BTree::new(Arc::clone(&pager)).unwrap();
            let mut model = std::collections::BTreeSet::new();
            // ~100-byte rows force multi-level trees at a few hundred keys.
            let pad = "x".repeat(96);
            for key in &keys {
                if model.insert(*key) {
                    btree
                        .insert(*key, &Row::new(vec![Value::Integer(*key), Value::Text(pad.clone())]))
                        .unwrap();
                }
            }
            let present: Vec<i64> = model.iter().copied().collect();
            for idx in deletes {
                let key = present[idx.index(present.len())];
                if model.remove(&key) {
                    assert!(btree.delete(key).unwrap());
                }
            }

            for (lo, width) in bounds {
                let hi = lo + width;
                let got: Vec<i64> = btree.range(lo, hi).unwrap().into_iter().map(|(k, row)| {
                    assert_eq!(row.values[0], Value::Integer(k));
                    k
                }).collect();
                let want: Vec<i64> = model.range(lo..=hi).copied().collect();
                prop_assert_eq!(got, want);
            }
            prop_assert!(btree.range(i64::MIN, i64::MAX).unwrap().len() == model.len());
            prop_assert!(btree.range(1, 0).unwrap().is_empty());
            prop_assert_eq!(btree.check_invariants().unwrap(), model.len());
        }
    }
}

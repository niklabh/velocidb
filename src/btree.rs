//! B-Tree index for primary key lookups and range scans.
//!
//! Supports insert, delete, point lookup, and range scan operations.
//! Keys are stored as `i64` with associated `Row` values in leaf pages.

use crate::storage::{Page, Pager, PAGE_SIZE};
use crate::types::{PageId, Result, Row, Value, VelociError};
use parking_lot::RwLock;
use std::sync::Arc;

const BTREE_ORDER: usize = 64; // Max keys per node
const MIN_KEYS: usize = BTREE_ORDER / 2; // Minimum keys per node

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

    pub fn new(node_type: NodeType) -> Self {
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
                "Buffer too small for NodeHeader: {} < {}", buffer.len(), Self::SIZE
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

        Ok(Self { root_page: Arc::new(RwLock::new(root_page)), pager })
    }

    pub fn from_root(root_page: PageId, pager: Arc<RwLock<Pager>>) -> Self {
        Self { root_page: Arc::new(RwLock::new(root_page)), pager }
    }

    pub fn insert(&mut self, key: i64, row: &Row) -> Result<()> {
        // Serialize the row
        let serialized = self.serialize_row(row)?;

        // Find the leaf node
        let root_page = *self.root_page.read();
        let leaf_page_id = {
            let mut pager = self.pager.write();
            self.find_leaf(&mut pager, root_page, key)?
        };

        // Insert into leaf
        let mut pager = self.pager.write();
        if let Some(new_root) = self.insert_into_leaf(&mut pager, leaf_page_id, key, &serialized)? {
            // Update the root page
            *self.root_page.write() = new_root;
        }

        Ok(())
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

                let child = u64::from_le_bytes(
                    page_data[offset..offset + 8]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
                ) as PageId;
                offset += 8;

                self.scan_node(child, results)?;
            }
        }

        Ok(())
    }

    pub fn delete(&mut self, key: i64) -> Result<bool> {
        let mut pager = self.pager.write();
        let root_page = *self.root_page.read();

        let leaf_page_id = self.find_leaf(&mut pager, root_page, key)?;
        let page_arc = pager.read_page(leaf_page_id)?;
        
        // Clone the page data to work with
        let mut page_clone = {
            let page = page_arc.read();
            page.clone()
        };
        
        let mut header = NodeHeader::deserialize(page_clone.data())?;
        
        // Find and remove the key
        let mut offset = NodeHeader::SIZE;
        let mut found = false;
        let mut delete_offset = 0;
        let mut delete_size = 0;
        
        for _ in 0..header.num_keys {
            let stored_key = i64::from_le_bytes(
                page_clone.data()[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );
            
            let size = u32::from_le_bytes(
                page_clone.data()[offset + 8..offset + 12]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
            ) as usize;
            
            if stored_key == key {
                found = true;
                delete_offset = offset;
                delete_size = 12 + size;
                break;
            }
            
            offset += 12 + size;
        }
        
        if !found {
            return Ok(false);
        }
        
        // Shift remaining data
        let data = page_clone.data_mut();
        let end_offset = self.find_data_end(&header, data)?;
        if delete_offset + delete_size < end_offset {
            data.copy_within(delete_offset + delete_size..end_offset, delete_offset);
        }
        
        header.num_keys -= 1;
        header.serialize(data);
        
        // Write back
        pager.write_page(leaf_page_id, &page_clone)?;

        // Handle node underflow: if leaf is not root and has fewer than MIN_KEYS
        if leaf_page_id != root_page && (header.num_keys as usize) < MIN_KEYS {
            drop(page_arc);
            self.handle_leaf_underflow(&mut pager, leaf_page_id, root_page)?;
        }
        
        Ok(true)
    }

    fn handle_leaf_underflow(&self, pager: &mut Pager, page_id: PageId, root_page: PageId) -> Result<()> {
        let page_arc = pager.read_page(page_id)?;
        let header = {
            let page = page_arc.read();
            NodeHeader::deserialize(page.data())?
        };

        if header.parent == 0 {
            return Ok(());
        }

        let parent_id = header.parent as PageId;
        // Collect parent data, release lock, then find sibling
        let (sibling_id, separator_key, sibling_is_left) = {
            let parent_arc = pager.read_page(parent_id)?;
            let parent_page = parent_arc.read();
            let parent_header = NodeHeader::deserialize(parent_page.data())?;
            // Collect child/separator info while holding parent lock
            let result = self.find_sibling_info(&parent_page, &parent_header, page_id)?;
            drop(parent_page);
            drop(parent_arc);
            result
        };

        if sibling_id == 0 {
            return Err(VelociError::Corruption(format!(
                "Parent-child link corruption: page {} has no sibling in parent {}",
                page_id, parent_id
            )));
        }

        // Check if sibling can lend a key (has more than MIN_KEYS)
        let sibling_arc = pager.read_page(sibling_id)?;
        let sibling_count = {
            let sibling_page = sibling_arc.read();
            let sibling_header = NodeHeader::deserialize(sibling_page.data())?;
            sibling_header.num_keys as usize
        };

        if sibling_count > MIN_KEYS {
            self.redistribute_from_sibling(pager, page_id, sibling_id, parent_id, separator_key, sibling_is_left)?;
        } else {
            self.merge_leaves(pager, page_id, sibling_id, parent_id, separator_key, root_page, sibling_is_left)?;
        }

        Ok(())
    }

    fn find_sibling_info(&self, parent_page: &Page, parent_header: &NodeHeader, child_id: PageId) -> Result<(PageId, i64, bool)> {
        let data = parent_page.data();

        let mut offset = NodeHeader::SIZE;
        let first_child = u64::from_le_bytes(
            data[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
        ) as PageId;
        offset += 8;

        if first_child == child_id {
            let separator = i64::from_le_bytes(
                data[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );
            let sibling = u64::from_le_bytes(
                data[offset + 8..offset + 16].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
            ) as PageId;
            // child is first, sibling is to the right
            return Ok((sibling, separator, false));
        }

        let mut prev_child = first_child;
        for _ in 0..parent_header.num_keys {
            let key = i64::from_le_bytes(
                data[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );
            let next_child = u64::from_le_bytes(
                data[offset + 8..offset + 16].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
            ) as PageId;

            if next_child == child_id {
                // child is after key, sibling (prev_child) is to the left
                return Ok((prev_child, key, true));
            }

            prev_child = next_child;
            offset += 16;
        }

        Ok((0, 0, false))
    }

    fn redistribute_from_sibling(&self, pager: &mut Pager, page_id: PageId, sibling_id: PageId, parent_id: PageId, separator_key: i64, sibling_is_left: bool) -> Result<()> {
        let sibling_arc = pager.read_page(sibling_id)?;
        let (stolen_key, stolen_data) = {
            let sibling_page = sibling_arc.read();
            let sibling_header = NodeHeader::deserialize(sibling_page.data())?;
            let data = sibling_page.data();

            let (key_offset, key, size, entry_data) = if sibling_is_left {
                // Steal the last entry from left sibling
                let entry_count = sibling_header.num_keys as usize;
                let (entry_key, entry_size, entry_data, entry_start) =
                    self.read_entry_at_index(data, NodeHeader::SIZE, entry_count - 1)?;
                (entry_start, entry_key, entry_size, entry_data)
            } else {
                // Steal the first entry from right sibling
                let key = i64::from_le_bytes(
                    data[NodeHeader::SIZE..NodeHeader::SIZE + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );
                let size = u32::from_le_bytes(
                    data[NodeHeader::SIZE + 8..NodeHeader::SIZE + 12].try_into().map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
                ) as usize;
                let entry_data = data[NodeHeader::SIZE + 12..NodeHeader::SIZE + 12 + size].to_vec();
                (NodeHeader::SIZE, key, size, entry_data)
            };

            // Remove the entry from sibling by shifting
            let end = self.find_data_end(&sibling_header, data)?;
            let entry_size = 12 + size;
            let mut sibling_clone = sibling_page.clone();
            let sib_data = sibling_clone.data_mut();
            sib_data.copy_within(key_offset + entry_size..end, key_offset);
            let mut new_header = sibling_header.clone();
            new_header.num_keys -= 1;
            new_header.serialize(sib_data);
            pager.write_page(sibling_id, &sibling_clone)?;

            (key, entry_data)
        };

        // Insert stolen entry into target page
        {
            let page_arc = pager.read_page(page_id)?;
            let mut page_clone = page_arc.read().clone();
            let header = NodeHeader::deserialize(page_clone.data())?;

            let end_offset = self.find_data_end(&header, page_clone.data())?;
            let entry_size = 12 + stolen_data.len();
            if end_offset + entry_size > PAGE_SIZE {
                return Err(VelociError::StorageError("Cannot redistribute: target page full".to_string()));
            }

            if sibling_is_left {
                // Prepend: shift existing data right to make room at the beginning
                let existing_start = NodeHeader::SIZE;
                let existing_end = end_offset;
                page_clone.data_mut().copy_within(existing_start..existing_end, existing_start + entry_size);
                let offset = existing_start;
                page_clone.data_mut()[offset..offset + 8].copy_from_slice(&stolen_key.to_le_bytes());
                page_clone.data_mut()[offset + 8..offset + 12].copy_from_slice(&(stolen_data.len() as u32).to_le_bytes());
                page_clone.data_mut()[offset + 12..offset + 12 + stolen_data.len()].copy_from_slice(&stolen_data);
            } else {
                // Append at end (current behavior)
                let offset = end_offset;
                page_clone.data_mut()[offset..offset + 8].copy_from_slice(&stolen_key.to_le_bytes());
                page_clone.data_mut()[offset + 8..offset + 12].copy_from_slice(&(stolen_data.len() as u32).to_le_bytes());
                page_clone.data_mut()[offset + 12..offset + 12 + stolen_data.len()].copy_from_slice(&stolen_data);
            }

            let mut new_header = header;
            new_header.num_keys += 1;
            new_header.serialize(page_clone.data_mut());
            pager.write_page(page_id, &page_clone)?;
        }

        // Update separator in parent
        if sibling_is_left {
            // When borrowing from left, the parent separator becomes the new first key of target
            let page_arc = pager.read_page(page_id)?;
            let page_header = NodeHeader::deserialize(page_arc.read().data())?;
            if page_header.num_keys > 0 {
                let new_first = i64::from_le_bytes(
                    page_arc.read().data()[NodeHeader::SIZE..NodeHeader::SIZE + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );
                self.update_parent_key(pager, parent_id, separator_key, new_first)?;
            }
        } else {
            // When borrowing from right, the parent separator becomes the new first key of sibling
            let sibling_arc = pager.read_page(sibling_id)?;
            let sibling_header = NodeHeader::deserialize(sibling_arc.read().data())?;
            if sibling_header.num_keys > 0 {
                let new_separator = i64::from_le_bytes(
                    sibling_arc.read().data()[NodeHeader::SIZE..NodeHeader::SIZE + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );
                self.update_parent_key(pager, parent_id, separator_key, new_separator)?;
            }
        }

        Ok(())
    }

    fn read_entry_at_index(&self, data: &[u8], start_offset: usize, index: usize) -> Result<(i64, usize, Vec<u8>, usize)> {
        let mut offset = start_offset;
        for i in 0..=index {
            let key = i64::from_le_bytes(
                data[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );
            let size = u32::from_le_bytes(
                data[offset + 8..offset + 12].try_into().map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
            ) as usize;
            if i == index {
                let entry_data = data[offset + 12..offset + 12 + size].to_vec();
                return Ok((key, size, entry_data, offset));
            }
            offset += 12 + size;
        }
        Err(VelociError::Corruption("Entry index out of bounds".to_string()))
    }

    fn update_parent_key(&self, pager: &mut Pager, parent_id: PageId, old_key: i64, new_key: i64) -> Result<()> {
        let parent_arc = pager.read_page(parent_id)?;
        let mut parent_clone = parent_arc.read().clone();
        let parent_header = NodeHeader::deserialize(parent_clone.data())?;

        let mut offset = NodeHeader::SIZE + 8; // skip first child
        for _ in 0..parent_header.num_keys {
            let key_bytes = &mut parent_clone.data_mut()[offset..offset + 8];
            let current = i64::from_le_bytes(key_bytes.try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?);
            if current == old_key {
                key_bytes.copy_from_slice(&new_key.to_le_bytes());
                break;
            }
            offset += 16;
        }

        pager.write_page(parent_id, &parent_clone)?;
        Ok(())
    }

    fn merge_leaves(&self, pager: &mut Pager, page_id: PageId, sibling_id: PageId, parent_id: PageId, _separator_key: i64, root_page: PageId, sibling_is_left: bool) -> Result<()> {
        // Canonical merge: always merge right page into left page
        let (left_id, right_id) = if sibling_is_left {
            (sibling_id, page_id)
        } else {
            (page_id, sibling_id)
        };

        // Move all entries from right page into left page
        let right_arc = pager.read_page(right_id)?;
        let right_entries: Vec<(i64, Vec<u8>)> = {
            let right_page = right_arc.read();
            let right_header = NodeHeader::deserialize(right_page.data())?;
            let mut entries = Vec::new();
            let mut offset = NodeHeader::SIZE;
            for _ in 0..right_header.num_keys {
                let key = i64::from_le_bytes(
                    right_page.data()[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );
                let size = u32::from_le_bytes(
                    right_page.data()[offset + 8..offset + 12].try_into().map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
                ) as usize;
                let data = right_page.data()[offset + 12..offset + 12 + size].to_vec();
                entries.push((key, data));
                offset += 12 + size;
            }
            entries
        };

        // Add entries to left page
        {
            let left_arc = pager.read_page(left_id)?;
            let mut left_clone = left_arc.read().clone();
            let mut header = NodeHeader::deserialize(left_clone.data())?;

            for (key, data) in &right_entries {
                let end_offset = self.find_data_end(&header, left_clone.data())?;
                let entry_size = 12 + data.len();
                if end_offset + entry_size > PAGE_SIZE {
                    return Err(VelociError::StorageError("Cannot merge: target page full".to_string()));
                }
                left_clone.data_mut()[end_offset..end_offset + 8].copy_from_slice(&key.to_le_bytes());
                left_clone.data_mut()[end_offset + 8..end_offset + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
                left_clone.data_mut()[end_offset + 12..end_offset + 12 + data.len()].copy_from_slice(data);
                header.num_keys += 1;
            }
            header.serialize(left_clone.data_mut());
            pager.write_page(left_id, &left_clone)?;
        }

        // Remove right page's pointer and key from parent
        self.remove_child_from_parent(pager, parent_id, right_id, root_page)?;

        Ok(())
    }

    fn remove_child_from_parent(&self, pager: &mut Pager, parent_id: PageId, child_id: PageId, root_page: PageId) -> Result<()> {
        let parent_arc = pager.read_page(parent_id)?;
        let mut parent_clone = parent_arc.read().clone();
        let parent_header = NodeHeader::deserialize(parent_clone.data())?;

        let mut offset = NodeHeader::SIZE;
        let first_child = u64::from_le_bytes(
            parent_clone.data()[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
        ) as PageId;
        offset += 8;

        if first_child == child_id {
            // Remove first child + first key
            let end = NodeHeader::SIZE + 8 + (parent_header.num_keys as usize * 16);
            parent_clone.data_mut().copy_within(NodeHeader::SIZE + 16..end, NodeHeader::SIZE);
            let mut new_header = parent_header.clone();
            new_header.num_keys -= 1;
            new_header.serialize(parent_clone.data_mut());
            pager.write_page(parent_id, &parent_clone)?;
        
            // If parent becomes the root and has no keys, promote the remaining child
            if parent_id == root_page && new_header.num_keys == 0 {
                let new_root = u64::from_le_bytes(
                    parent_clone.data()[NodeHeader::SIZE..NodeHeader::SIZE + 8].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
                ) as PageId;
                *self.root_page.write() = new_root;
            }
            return Ok(());
        }

        for i in 0..parent_header.num_keys {
            let next_child = u64::from_le_bytes(
                parent_clone.data()[offset + 8..offset + 16].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
            ) as PageId;

            if next_child == child_id {
                // Remove key at `i` and child at `i+1` (at offset + 8)
                let end = NodeHeader::SIZE + 8 + (parent_header.num_keys as usize * 16);
                parent_clone.data_mut().copy_within(offset + 16..end, offset);
                let mut new_header = parent_header.clone();
                new_header.num_keys -= 1;
                new_header.serialize(parent_clone.data_mut());
                pager.write_page(parent_id, &parent_clone)?;

                // Handle root with no keys
                if parent_id == root_page && new_header.num_keys == 0 {
                    let new_root = u64::from_le_bytes(
                        parent_clone.data()[NodeHeader::SIZE..NodeHeader::SIZE + 8].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?,
                    ) as PageId;
                    *self.root_page.write() = new_root;
                } else if parent_id != root_page && (new_header.num_keys as usize) < MIN_KEYS {
                    self.handle_internal_underflow(pager, parent_id, root_page)?;
                }
                return Ok(());
            }
            offset += 16;
        }

        Ok(())
    }

    fn handle_internal_underflow(&self, pager: &mut Pager, page_id: PageId, root_page: PageId) -> Result<()> {
        let page_arc = pager.read_page(page_id)?;
        let header = {
            let page = page_arc.read();
            NodeHeader::deserialize(page.data())?
        };
        if header.parent == 0 {
            return Ok(());
        }
        // For simplicity, internal node underflow follows the same pattern as leaf underflow
        // but redistributes child pointers instead of data entries.
        // This is a simplified implementation - a full implementation would mirror
        // handle_leaf_underflow but for internal nodes.
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
                
                let next_child = u64::from_le_bytes(
                    page.data()[offset + 8..offset + 16]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
                ) as PageId;
                
                if key < stored_key {
                    break;
                }
                
                child_page = next_child;
                offset += 16;
            }
            
            page_id = child_page;
        }
    }

    fn insert_into_leaf(&self, pager: &mut Pager, page_id: PageId, key: i64, data: &[u8]) -> Result<Option<PageId>> {
        let page_arc = pager.read_page(page_id)?;
        
        // Clone the page data to work with
        let mut page_clone = {
            let page = page_arc.read();
            page.clone()
        };
        
        let mut header = NodeHeader::deserialize(page_clone.data())?;
        
        // Find insertion point
        let mut insert_offset = NodeHeader::SIZE;
        let mut offset = NodeHeader::SIZE;
        
        for _ in 0..header.num_keys {
            let stored_key = i64::from_le_bytes(
                page_clone.data()[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );
            
            let size = u32::from_le_bytes(
                page_clone.data()[offset + 8..offset + 12]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
            ) as usize;
            
            if key < stored_key {
                insert_offset = offset;
                break;
            }
            
            offset += 12 + size;
            insert_offset = offset;
        }
        
        // Check if we have space
        let required_space = 12 + data.len();
        let end_offset = self.find_data_end(&header, page_clone.data())?;

        if end_offset + required_space > PAGE_SIZE {
            // Node is full, split it
            let (sibling_page_id, split_key) = self.split_leaf_node(pager, page_id)?;
            let new_root = self.insert_into_parent(pager, page_id, sibling_page_id, split_key)?;

            // Insert into the appropriate leaf (could be original or sibling)
            if key < split_key {
                self.insert_into_leaf(pager, page_id, key, data)?;
            } else {
                self.insert_into_leaf(pager, sibling_page_id, key, data)?;
            }

            return Ok(new_root);
        }
        
        // Make room for new entry
        let page_data = page_clone.data_mut();
        if insert_offset < end_offset {
            page_data.copy_within(insert_offset..end_offset, insert_offset + required_space);
        }
        
        // Write new entry
        page_data[insert_offset..insert_offset + 8].copy_from_slice(&key.to_le_bytes());
        page_data[insert_offset + 8..insert_offset + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        page_data[insert_offset + 12..insert_offset + 12 + data.len()].copy_from_slice(data);
        
        header.num_keys += 1;
        header.serialize(page_data);
        
        // Write back
        pager.write_page(page_id, &page_clone)?;

        Ok(None)
    }

    fn find_data_end(&self, header: &NodeHeader, data: &[u8]) -> Result<usize> {
        let mut offset = NodeHeader::SIZE;

        for i in 0..header.num_keys {
            if offset + 12 > PAGE_SIZE {
                return Err(VelociError::Corruption(format!("Invalid offset {} for key {}", offset, i)));
            }

            let size_bytes = data.get(offset + 8..offset + 12)
                .ok_or_else(|| VelociError::Corruption(format!("Cannot read size for key {}", i)))?;

            let size = u32::from_le_bytes(size_bytes.try_into()
                .map_err(|_| VelociError::Corruption(format!("Invalid size bytes for key {}", i)))?) as usize;

            offset += 12 + size;

            if offset > PAGE_SIZE {
                return Err(VelociError::Corruption(format!("Data end offset {} exceeds page size", offset)));
            }
        }

        Ok(offset)
    }

    fn split_leaf_node(&self, pager: &mut Pager, page_id: PageId) -> Result<(PageId, i64)> {
        // Read the current page - snapshot all data at once to avoid corruption
        let page_arc = pager.read_page(page_id)?;
        let page_data: Vec<u8> = {
            let page = page_arc.read();
            page.data().to_vec()
        };
        let header = NodeHeader::deserialize(&page_data)?;

        // Collect all entries first (key, size, data) into a vector
        let mut entries: Vec<(i64, Vec<u8>)> = Vec::new();
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
            let data = page_data[offset + 12..offset + 12 + size].to_vec();
            entries.push((key, data));
            offset += 12 + size;
        }

        // Split point (middle of keys)
        let mut split_index = entries.len() / 2;
        if split_index == 0 && !entries.is_empty() {
            split_index = 1;
        }
        let split_key = entries[split_index].0;

        // Create new sibling page
        let sibling_page_id = pager.allocate_page()?;
        let mut sibling_page = Page::new();

        // Build left page with entries 0..split_index
        let mut left_page = Page::new();
        let mut left_offset = NodeHeader::SIZE;
        let mut left_keys = 0u16;
        for entry in entries.iter().take(split_index) {
            let (key, data) = entry;
            let entry_size = 12 + data.len();
            left_page.data_mut()[left_offset..left_offset + 8].copy_from_slice(&key.to_le_bytes());
            left_page.data_mut()[left_offset + 8..left_offset + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
            left_page.data_mut()[left_offset + 12..left_offset + 12 + data.len()].copy_from_slice(data);
            left_offset += entry_size;
            left_keys += 1;
        }
        let mut left_header = header.clone();
        left_header.num_keys = left_keys;
        left_header.serialize(left_page.data_mut());
        pager.write_page(page_id, &left_page)?;

        // Build right page with entries split_index..
        let mut right_offset = NodeHeader::SIZE;
        let mut right_keys = 0u16;
        for entry in entries.iter().skip(split_index) {
            let (key, data) = entry;
            let entry_size = 12 + data.len();
            sibling_page.data_mut()[right_offset..right_offset + 8].copy_from_slice(&key.to_le_bytes());
            sibling_page.data_mut()[right_offset + 8..right_offset + 12].copy_from_slice(&(data.len() as u32).to_le_bytes());
            sibling_page.data_mut()[right_offset + 12..right_offset + 12 + data.len()].copy_from_slice(data);
            right_offset += entry_size;
            right_keys += 1;
        }
        let mut right_header = NodeHeader::new_leaf();
        right_header.num_keys = right_keys;
        right_header.serialize(sibling_page.data_mut());
        pager.write_page(sibling_page_id, &sibling_page)?;

        Ok((sibling_page_id, split_key))
    }

    fn insert_into_parent(&self, pager: &mut Pager, left_page: PageId, right_page: PageId, split_key: i64) -> Result<Option<PageId>> {
        // Get parent of left page
        let left_page_data = pager.read_page(left_page)?;
        let left_header = NodeHeader::deserialize(left_page_data.read().data())?;

        let new_root = if left_header.parent == 0 {
            // Left page is root, create new root
            Some(self.create_new_root(pager, left_page, right_page, split_key)?)
        } else {
            // Insert into existing parent (may cause recursive splits and new root)
            self.insert_into_internal(pager, left_header.parent as u64, left_page, right_page, split_key)?
        };

        Ok(new_root)
    }

    fn create_new_root(&self, pager: &mut Pager, left_page: PageId, right_page: PageId, split_key: i64) -> Result<PageId> {
        // Allocate new root page
        let root_page_id = pager.allocate_page()?;
        let mut root_page = Page::new();

        // Create internal node header
        let mut header = NodeHeader::new(NodeType::Internal);
        header.num_keys = 1;

        // Write header
        header.serialize(root_page.data_mut());

        // Write the single key and child pointers
        let mut offset = NodeHeader::SIZE;

        // Left child pointer
        root_page.data_mut()[offset..offset + 8].copy_from_slice(&(left_page as u64).to_le_bytes());
        offset += 8;

        // Key
        root_page.data_mut()[offset..offset + 8].copy_from_slice(&split_key.to_le_bytes());
        offset += 8;

        // Right child pointer
        root_page.data_mut()[offset..offset + 8].copy_from_slice(&(right_page as u64).to_le_bytes());

        // Write new root
        pager.write_page(root_page_id, &root_page)?;

        // Update child parent pointers
        let left_page_data = pager.read_page(left_page)?;
        let mut left_header = NodeHeader::deserialize(left_page_data.read().data())?;
        left_header.parent = root_page_id as u32;
        left_header.serialize(left_page_data.write().data_mut());

        let right_page_data = pager.read_page(right_page)?;
        let mut right_header = NodeHeader::deserialize(right_page_data.read().data())?;
        right_header.parent = root_page_id as u32;
        right_header.serialize(right_page_data.write().data_mut());

        Ok(root_page_id)
    }

    fn insert_into_internal(&self, pager: &mut Pager, page_id: PageId, left_page: PageId, right_page: PageId, key: i64) -> Result<Option<PageId>> {
        // Update right_page parent pointer to this page
        // We do this first so that if we split, the child already points to us (the left node),
        // and if it moves to the sibling, the split logic will update it to the sibling.
        {
            let right_page_data = pager.read_page(right_page)?;
            let mut right_header = NodeHeader::deserialize(right_page_data.read().data())?;
            right_header.parent = page_id as u32;
            right_header.serialize(right_page_data.write().data_mut());
        }

        let page_arc = pager.read_page(page_id)?;
        let mut page = page_arc.read().clone();
        let mut header = NodeHeader::deserialize(page.data())?;

        // Find insertion point
        // Internal node format: [child_0][key_0][child_1][key_1]...[key_n-1][child_n]
        let mut insert_idx = 0;
        let mut offset = NodeHeader::SIZE + 8; // Skip first child pointer

        for i in 0..header.num_keys {
            let stored_key = i64::from_le_bytes(
                page.data()[offset..offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );

            if key < stored_key {
                break;
            }
            insert_idx = i + 1;
            offset += 16; // key + child pointer
        }

        // Check if we need to split this internal node
        if header.num_keys >= BTREE_ORDER as u16 {
            return self.split_internal_node(pager, page_id, left_page, right_page, key);
        }

        // Calculate insertion offset: NodeHeader + first_child + (insert_idx * (key + child))
        let insert_offset = NodeHeader::SIZE + 8 + (insert_idx as usize * 16);
        let end_offset = NodeHeader::SIZE + 8 + (header.num_keys as usize * 16);

        // Make room for new entry (key + child pointer = 16 bytes)
        if insert_offset < end_offset {
            page.data_mut().copy_within(insert_offset..end_offset, insert_offset + 16);
        }

        // Insert new key and right child
        page.data_mut()[insert_offset..insert_offset + 8].copy_from_slice(&key.to_le_bytes());
        page.data_mut()[insert_offset + 8..insert_offset + 16].copy_from_slice(&(right_page as u64).to_le_bytes());

        header.num_keys += 1;
        header.serialize(page.data_mut());

        pager.write_page(page_id, &page)?;

        Ok(None)
    }

    fn split_internal_node(&self, pager: &mut Pager, page_id: PageId, _left_page: PageId, right_page: PageId, key: i64) -> Result<Option<PageId>> {
        // Read the current page
        let page_arc = pager.read_page(page_id)?;
        let page = page_arc.read().clone();
        let header = NodeHeader::deserialize(page.data())?;

        // Collect all keys and children
        let mut keys = Vec::with_capacity(BTREE_ORDER + 1);
        let mut children = Vec::with_capacity(BTREE_ORDER + 2);

        let mut offset = NodeHeader::SIZE;
        
        // First child
        let first_child = u64::from_le_bytes(
            page.data()[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?
        ) as PageId;
        children.push(first_child);
        offset += 8;

        for _ in 0..header.num_keys {
            let k = i64::from_le_bytes(
                page.data()[offset..offset + 8].try_into().map_err(|_| VelociError::Corruption("Invalid key".to_string()))?
            );
            keys.push(k);
            
            let c = u64::from_le_bytes(
                page.data()[offset + 8..offset + 16].try_into().map_err(|_| VelociError::Corruption("Invalid child".to_string()))?
            ) as PageId;
            children.push(c);
            
            offset += 16;
        }

        // Find insertion point
        let mut insert_idx = 0;
        while insert_idx < keys.len() && keys[insert_idx] < key {
            insert_idx += 1;
        }

        // Insert new key and child
        keys.insert(insert_idx, key);
        children.insert(insert_idx + 1, right_page);

        // Split
        let split_idx = keys.len() / 2;
        let promoted_key = keys[split_idx];

        // Create sibling page
        let sibling_page_id = pager.allocate_page()?;
        let mut sibling_page = Page::new();
        let mut sibling_header = NodeHeader::new(NodeType::Internal);

        // Right node data
        let right_keys = &keys[split_idx + 1..];
        let right_children = &children[split_idx + 1..];

        sibling_header.num_keys = right_keys.len() as u16;
        sibling_header.serialize(sibling_page.data_mut());

        let mut offset = NodeHeader::SIZE;
        sibling_page.data_mut()[offset..offset + 8].copy_from_slice(&(right_children[0] as u64).to_le_bytes());
        offset += 8;

        for i in 0..right_keys.len() {
            sibling_page.data_mut()[offset..offset + 8].copy_from_slice(&right_keys[i].to_le_bytes());
            sibling_page.data_mut()[offset + 8..offset + 16].copy_from_slice(&(right_children[i + 1] as u64).to_le_bytes());
            offset += 16;
        }

        // Update parent pointers for children moved to sibling
        for &child_id in right_children {
            let child_page_arc = pager.read_page(child_id)?;
            // We need to acquire a write lock on the child page to update its parent pointer
            let mut child_page = child_page_arc.write();
            let mut child_header = NodeHeader::deserialize(child_page.data())?;
            child_header.parent = sibling_page_id as u32;
            child_header.serialize(child_page.data_mut());
        }

        pager.write_page(sibling_page_id, &sibling_page)?;

        // Update current (left) page
        let left_keys = &keys[0..split_idx];
        let left_children = &children[0..split_idx + 1];

        let mut new_left_page = Page::new();
        let mut new_left_header = header.clone();
        new_left_header.num_keys = left_keys.len() as u16;
        new_left_header.serialize(new_left_page.data_mut());

        let mut offset = NodeHeader::SIZE;
        new_left_page.data_mut()[offset..offset + 8].copy_from_slice(&(left_children[0] as u64).to_le_bytes());
        offset += 8;

        for i in 0..left_keys.len() {
            new_left_page.data_mut()[offset..offset + 8].copy_from_slice(&left_keys[i].to_le_bytes());
            new_left_page.data_mut()[offset + 8..offset + 16].copy_from_slice(&(left_children[i + 1] as u64).to_le_bytes());
            offset += 16;
        }

        pager.write_page(page_id, &new_left_page)?;

        // Insert promoted key into parent
        self.insert_into_parent(pager, page_id, sibling_page_id, promoted_key)
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
                    let len = u32::from_le_bytes(
                        data[offset..offset + 4]
                            .try_into()
                            .map_err(|_| VelociError::Corruption("Invalid text length".to_string()))?,
                    ) as usize;
                    offset += 4;
                    let s = String::from_utf8(data[offset..offset + len].to_vec())
                        .map_err(|_| VelociError::Corruption("Invalid UTF-8".to_string()))?;
                    offset += len;
                    values.push(Value::Text(s));
                }
                4 => {
                    let len = u32::from_le_bytes(
                        data[offset..offset + 4]
                            .try_into()
                            .map_err(|_| VelociError::Corruption("Invalid blob length".to_string()))?,
                    ) as usize;
                    offset += 4;
                    let b = data[offset..offset + len].to_vec();
                    offset += len;
                    values.push(Value::Blob(b));
                }
                _ => return Err(VelociError::Corruption(format!("Invalid type tag: {}", type_tag))),
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
        assert!(btree.root_page() >= 0);
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
                Value::Text(format!("Record {}", i))
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
                Value::Text(format!("Record {}", i))
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
}


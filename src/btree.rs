// B-Tree implementation for indexing

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

        // Find the leftmost leaf
        let mut page_id = root_page;
        loop {
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
                break;
            } else {
                // Internal node - go to first child
                let child = u32::from_le_bytes(
                    page_data[NodeHeader::SIZE..NodeHeader::SIZE + 4]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
                ) as PageId;
                page_id = child;
            }
        }
        
        Ok(results)
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
        
        Ok(true)
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
            let mut offset = NodeHeader::SIZE;
            let mut child_page = u32::from_le_bytes(
                page.data()[offset..offset + 4]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
            ) as PageId;
            
            offset += 4;
            
            for _ in 0..header.num_keys {
                let stored_key = i64::from_le_bytes(
                    page.data()[offset..offset + 8]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
                );
                
                let next_child = u32::from_le_bytes(
                    page.data()[offset + 8..offset + 12]
                        .try_into()
                        .map_err(|_| VelociError::Corruption("Invalid child pointer".to_string()))?,
                ) as PageId;
                
                if key < stored_key {
                    break;
                }
                
                child_page = next_child;
                offset += 12;
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

            // If a new root was created, return it immediately
            if let Some(root_page_id) = new_root {
                return Ok(Some(root_page_id));
            }

            // Otherwise, insert into the appropriate leaf (could be original or sibling)
            if key < split_key {
                return self.insert_into_leaf(pager, page_id, key, data);
            } else {
                return self.insert_into_leaf(pager, sibling_page_id, key, data);
            }
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
        // Read the current page
        let page_arc = pager.read_page(page_id)?;
        let page = page_arc.read();
        let header = NodeHeader::deserialize(page.data())?;
        let end_offset = self.find_data_end(&header, page.data())?;

        // Create new sibling page
        let sibling_page_id = pager.allocate_page()?;
        let mut sibling_page = Page::new();

        // Split point (middle of keys)
        let split_index = header.num_keys as usize / 2;
        let mut left_keys = 0u16;
        let mut right_keys = 0u16;
        let mut split_key = 0i64;

        // Copy data to new pages
        let mut left_offset = NodeHeader::SIZE;
        let mut right_offset = NodeHeader::SIZE;
        let mut data_offset = NodeHeader::SIZE;

        for i in 0..header.num_keys {
            // Read key and size
            let key = i64::from_le_bytes(
                page.data()[data_offset..data_offset + 8]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );

            let size = u32::from_le_bytes(
                page.data()[data_offset + 8..data_offset + 12]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid size".to_string()))?,
            ) as usize;

            let entry_size = 12 + size;

            if i < split_index as u16 {
                // Copy to left page - read current left page first
                let current_left_page = pager.read_page(page_id)?;
                let mut left_page_clone = current_left_page.read().clone();

                left_page_clone.data_mut()[left_offset..left_offset + entry_size]
                    .copy_from_slice(&page.data()[data_offset..data_offset + entry_size]);
                left_offset += entry_size;
                left_keys += 1;

                pager.write_page(page_id, &left_page_clone)?;
            } else {
                // Copy to right page
                if i == split_index as u16 {
                    split_key = key;
                }

                sibling_page.data_mut()[right_offset..right_offset + entry_size]
                    .copy_from_slice(&page.data()[data_offset..data_offset + entry_size]);
                right_offset += entry_size;
                right_keys += 1;
            }

            data_offset += entry_size;
        }

        // Update headers
        let mut left_header = header.clone();
        left_header.num_keys = left_keys;
        left_header.serialize(pager.read_page(page_id)?.write().data_mut());

        let mut right_header = NodeHeader::new_leaf();
        right_header.num_keys = right_keys;
        right_header.serialize(sibling_page.data_mut());

        // Write sibling page
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
            // Insert into existing parent
            self.insert_into_internal(pager, left_header.parent as u64, left_page, right_page, split_key)?;
            None
        };

        // Update parent pointers (only if not creating new root)
        if new_root.is_none() {
            let right_page_data = pager.read_page(right_page)?;
            let mut right_header = NodeHeader::deserialize(right_page_data.read().data())?;
            right_header.parent = left_header.parent;
            right_header.serialize(right_page_data.write().data_mut());
        }

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

    fn insert_into_internal(&self, pager: &mut Pager, page_id: PageId, left_page: PageId, right_page: PageId, key: i64) -> Result<()> {
        let page_arc = pager.read_page(page_id)?;
        let mut page = page_arc.read().clone();
        let mut header = NodeHeader::deserialize(page.data())?;

        // Find insertion point
        let mut insert_idx = 0;
        let mut offset = NodeHeader::SIZE;

        for i in 0..header.num_keys {
            let stored_key = i64::from_le_bytes(
                page.data()[offset + 8..offset + 16]
                    .try_into()
                    .map_err(|_| VelociError::Corruption("Invalid key".to_string()))?,
            );

            if key < stored_key {
                break;
            }
            insert_idx = i + 1;
            offset += 16; // key + right child pointer
        }

        // Check if we need to split this internal node
        if header.num_keys >= BTREE_ORDER as u16 {
            return self.split_internal_node(pager, page_id, left_page, right_page, key);
        }

        // Make room for new entry (key + right child pointer)
        let insert_offset = NodeHeader::SIZE + (insert_idx as usize * 16);
        let end_offset = NodeHeader::SIZE + (header.num_keys as usize * 16) + 8; // +8 for the last child pointer

        if insert_offset < end_offset {
            page.data_mut().copy_within(insert_offset..end_offset, insert_offset + 16);
        }

        // Insert new entry
        let insert_pos = insert_offset;
        page.data_mut()[insert_pos..insert_pos + 8].copy_from_slice(&(left_page as u64).to_le_bytes());
        page.data_mut()[insert_pos + 8..insert_pos + 16].copy_from_slice(&key.to_le_bytes());

        // Update the child pointer after the inserted key
        let right_ptr_pos = insert_pos + 16;
        page.data_mut()[right_ptr_pos..right_ptr_pos + 8].copy_from_slice(&(right_page as u64).to_le_bytes());

        header.num_keys += 1;
        header.serialize(page.data_mut());

        pager.write_page(page_id, &page)?;

        Ok(())
    }

    fn split_internal_node(&self, _pager: &mut Pager, _page_id: PageId, _left_page: PageId, _right_page: PageId, _key: i64) -> Result<()> {
        // For now, just create a simple split - this is complex and would need more implementation
        // This is a placeholder that needs full implementation
        Err(VelociError::Corruption("Internal node splitting not fully implemented".to_string()))
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
                Value::Float(f) => {
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
}


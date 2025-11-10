// B-Tree implementation for indexing

use crate::storage::{Page, Pager, PAGE_SIZE};
use crate::types::{PageId, Result, Row, Value, VelociError};
use parking_lot::RwLock;
use std::sync::Arc;

const BTREE_ORDER: usize = 64; // Max keys per node

#[derive(Debug, Clone, Copy, PartialEq)]
enum NodeType {
    Internal = 0,
    Leaf = 1,
}

#[repr(C)]
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

    pub fn deserialize(buffer: &[u8]) -> Self {
        Self {
            node_type: buffer[0],
            num_keys: u16::from_le_bytes([buffer[1], buffer[2]]),
            parent: u32::from_le_bytes([buffer[3], buffer[4], buffer[5], buffer[6]]),
            _padding: 0,
        }
    }
}

pub struct BTree {
    root_page: PageId,
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

        Ok(Self { root_page, pager })
    }

    pub fn from_root(root_page: PageId, pager: Arc<RwLock<Pager>>) -> Self {
        Self { root_page, pager }
    }

    pub fn insert(&mut self, key: i64, row: &Row) -> Result<()> {
        // Serialize the row
        let serialized = self.serialize_row(row)?;
        
        // Find the leaf node
        let leaf_page_id = {
            let mut pager = self.pager.write();
            self.find_leaf(&mut pager, key)?
        };
        
        // Insert into leaf
        let mut pager = self.pager.write();
        self.insert_into_leaf(&mut pager, leaf_page_id, key, &serialized)?;
        
        Ok(())
    }

    pub fn search(&self, key: i64) -> Result<Option<Row>> {
        let mut pager = self.pager.write();
        
        let leaf_page_id = self.find_leaf(&mut pager, key)?;
        let page_arc = pager.read_page(leaf_page_id)?;
        let page = page_arc.read();
        
        let header = NodeHeader::deserialize(page.data());
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
        
        // Find the leftmost leaf
        let mut page_id = self.root_page;
        loop {
            let mut pager = self.pager.write();
            let page_arc = pager.read_page(page_id)?;
            let page_data = {
                let page = page_arc.read();
                page.data().to_vec()
            };
            drop(pager);
            
            let header = NodeHeader::deserialize(&page_data);
            
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
        
        let leaf_page_id = self.find_leaf(&mut pager, key)?;
        let page_arc = pager.read_page(leaf_page_id)?;
        
        // Clone the page data to work with
        let mut page_clone = {
            let page = page_arc.read();
            page.clone()
        };
        
        let mut header = NodeHeader::deserialize(page_clone.data());
        
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
        let end_offset = self.find_data_end(&header, data);
        if delete_offset + delete_size < end_offset {
            data.copy_within(delete_offset + delete_size..end_offset, delete_offset);
        }
        
        header.num_keys -= 1;
        header.serialize(data);
        
        // Write back
        pager.write_page(leaf_page_id, &page_clone)?;
        
        Ok(true)
    }

    fn find_leaf(&self, pager: &mut Pager, key: i64) -> Result<PageId> {
        let mut page_id = self.root_page;
        
        loop {
            let page_arc = pager.read_page(page_id)?;
            let page = page_arc.read();
            let header = NodeHeader::deserialize(page.data());
            
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

    fn insert_into_leaf(&self, pager: &mut Pager, page_id: PageId, key: i64, data: &[u8]) -> Result<()> {
        let page_arc = pager.read_page(page_id)?;
        
        // Clone the page data to work with
        let mut page_clone = {
            let page = page_arc.read();
            page.clone()
        };
        
        let mut header = NodeHeader::deserialize(page_clone.data());
        
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
        let end_offset = self.find_data_end(&header, page_clone.data());
        
        if end_offset + required_space > PAGE_SIZE {
            // TODO: Handle node split
            return Err(VelociError::Corruption("Node full - split not implemented".to_string()));
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
        
        Ok(())
    }

    fn find_data_end(&self, header: &NodeHeader, data: &[u8]) -> usize {
        let mut offset = NodeHeader::SIZE;
        
        for _ in 0..header.num_keys {
            let size = u32::from_le_bytes(
                data[offset + 8..offset + 12]
                    .try_into()
                    .unwrap_or([0, 0, 0, 0]),
            ) as usize;
            offset += 12 + size;
        }
        
        offset
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
        self.root_page
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


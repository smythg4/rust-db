use crate::commontypes::{
    Key, KeyError, Lsn, LsnError, PAGE_ID_SIZE, PageId, SLOT_ENTRY_SIZE, SlotEntry,
};
use crate::schema::{Row, RowValueError};
use crate::traits::Serializable;
use std::io::{Cursor, Read, Write};
use thiserror::Error;

pub const PAGE_SIZE: usize = 4096;
pub const LEAF_TAG: u8 = 1;
pub const INTERNAL_TAG: u8 = 2;

#[derive(Error, Debug)]
pub enum PageError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error(transparent)]
    LsnError(#[from] LsnError),
    #[error(transparent)]
    Value(#[from] RowValueError),
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("Invalid page tag: {0}")]
    InvalidTag(u8),
}
pub type RawPage = [u8; PAGE_SIZE];

#[derive(Debug, PartialEq)]
pub struct Page {
    last_update: Lsn,
    parent: Option<PageId>,
    body: PageBody,
}

#[derive(Debug, PartialEq)]
pub enum PageBody {
    Leaf {
        records: Vec<Row>,
        next: Option<PageId>,
        prev: Option<PageId>,
    },
    Internal {
        keys: Vec<Key>,
        children: Vec<PageId>,
    },
}

impl Page {
    pub fn page_type_tag(&self) -> u8 {
        match self.body {
            PageBody::Internal { .. } => INTERNAL_TAG,
            PageBody::Leaf { .. } => LEAF_TAG,
        }
    }

    pub fn num_items(&self) -> usize {
        match &self.body {
            PageBody::Leaf { records, .. } => records.len(),
            PageBody::Internal { keys, .. } => keys.len(),
        }
    }

    pub fn as_raw_page(&self) -> Result<RawPage, PageError> {
        let mut cursor = Cursor::new([0u8; PAGE_SIZE]);

        // Write the header information: Type Tag, LSN, Parent page, then if
        // a Leaf node, write the Next and Prev pages.
        cursor.write_all(&[self.page_type_tag()])?;
        self.last_update.serialize(&mut cursor)?;
        self.parent.serialize(&mut cursor)?;
        match self.body {
            PageBody::Leaf { next, prev, .. } => {
                next.serialize(&mut cursor)?;
                prev.serialize(&mut cursor)?;
            }
            PageBody::Internal { .. } => {}
        };

        // note the number of items stored on the page
        let num_items = self.num_items() as u16; // records.len() for Leaf, keys.len() for Internal
        cursor.write_all(&num_items.to_be_bytes())?;

        // Header complete, mark the position.
        let header_end = cursor.position() as usize;

        // track the current position of where to write data
        let mut data_offset = PAGE_SIZE;

        match &self.body {
            PageBody::Leaf { records, .. } => {
                // Track the positions
                let mut slot_offset = header_end;

                for record in records {
                    // serialize the row into a 'scratch' buffer to measure the length
                    let mut buffer = Vec::new();
                    record.serialize(&mut buffer)?;
                    let len = buffer.len();

                    // shift the offset back, and write the Row at the proper offset
                    data_offset -= len;
                    cursor.set_position(data_offset as u64);
                    cursor.write_all(&buffer)?;

                    // build a SlotEntry to point to the newly written data and write
                    // it at the proper offset
                    let slot_entry = SlotEntry::new(data_offset as u16, len as u16);
                    cursor.set_position(slot_offset as u64);
                    slot_entry.serialize(&mut cursor)?;

                    // advance the running slot_offset
                    slot_offset += SLOT_ENTRY_SIZE;
                }
            }
            PageBody::Internal { keys, children } => {
                // children are fixed-width and inserted in order right after the header
                let mut children_pos = header_end;
                for child in children {
                    cursor.set_position(children_pos as u64);
                    child.serialize(&mut cursor)?;
                    children_pos += PAGE_ID_SIZE;
                }

                // Keys: variable-width, same slot+data-area pattern as Leaf's rows.
                let mut slot_offset = children_pos;
                for key in keys {
                    // write key into a scratch buffer and measure size
                    let mut buffer = Vec::new();
                    key.serialize(&mut buffer)?;
                    let length = buffer.len();

                    // adjust data_offset and write the data
                    data_offset -= length;
                    cursor.set_position(data_offset as u64);
                    cursor.write_all(&buffer)?;

                    // write the accompanying SlotEntry
                    let slot_entry = SlotEntry::new(data_offset as u16, length as u16);
                    cursor.set_position(slot_offset as u64);
                    slot_entry.serialize(&mut cursor)?;
                    slot_offset += SLOT_ENTRY_SIZE;
                }
            }
        };
        Ok(cursor.into_inner())
    }
}

impl Serializable for Page {
    type Error = PageError;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        let raw = self.as_raw_page()?;
        w.write_all(&raw)?;
        Ok(())
    }
    fn deserialize<R: std::io::prelude::Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut buf = [0u8; PAGE_SIZE];
        r.read_exact(&mut buf)?;
        let mut cursor = Cursor::new(&buf[..]);

        let mut buf_one = [0u8; 1];
        cursor.read_exact(&mut buf_one)?;
        let tag = buf_one[0];
        let is_leaf = match tag {
            INTERNAL_TAG => false,
            LEAF_TAG => true,
            _ => return Err(PageError::InvalidTag(tag)),
        };

        let last_update = Lsn::deserialize(&mut cursor)?;
        let parent = Option::<PageId>::deserialize(&mut cursor)?;
        let (next_page, prev_page) = if is_leaf {
            let np = Option::<PageId>::deserialize(&mut cursor)?;
            let pp = Option::<PageId>::deserialize(&mut cursor)?;
            (np, pp)
        } else {
            (None, None)
        };

        let mut buf_two = [0u8; 2];
        cursor.read_exact(&mut buf_two)?;
        let num_items = u16::from_be_bytes(buf_two) as usize;

        let body = if is_leaf {
            let mut records = Vec::with_capacity(num_items);
            for _ in 0..num_items {
                let slot = SlotEntry::deserialize(&mut cursor)?;
                records.push(Row::deserialize(&mut &buf[slot.range()])?);
            }
            PageBody::Leaf {
                next: next_page,
                prev: prev_page,
                records,
            }
        } else {
            let mut children = Vec::with_capacity(num_items + 1);
            for _ in 0..num_items + 1 {
                children.push(PageId::deserialize(&mut cursor)?);
            }
            let mut keys = Vec::with_capacity(num_items);
            for _ in 0..num_items {
                let slot = SlotEntry::deserialize(&mut cursor)?;
                keys.push(Key::deserialize(&mut &buf[slot.range()])?);
            }
            PageBody::Internal { keys, children }
        };
        Ok(Page {
            parent,
            last_update,
            body,
        })
    }
}

impl PageBody {
    pub fn find_child(&self, search_key: &Key) -> Option<PageId> {
        match self {
            PageBody::Internal { keys, children } => {
                // binary search of the sorted keys
                let idx = keys.partition_point(|k| k <= search_key);
                children.get(idx).copied()
            }
            PageBody::Leaf { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commontypes::TableId;
    use crate::schema::RowValue;

    #[test]
    fn basic_leaf_page_roundtrip() {
        let records: Vec<Row> = (1..=5)
            .map(|j| Row {
                fields: (0..10)
                    .map(|i| match i % 4 {
                        0 => RowValue::Integer(i * j),
                        1 => RowValue::String(format!("{i}{j}")),
                        2 => RowValue::Float(i as f64 / j as f64),
                        3 => RowValue::Null,
                        _ => unreachable!(),
                    })
                    .collect(),
            })
            .collect();
        let lpage = Page {
            last_update: Lsn::new(10).unwrap(),
            parent: Some(PageId::new(TableId::new(1), 1)),
            body: PageBody::Leaf {
                records,
                next: Some(PageId::new(TableId::new(1), 3)),
                prev: None,
            },
        };
        let mut bytes = Vec::with_capacity(PAGE_SIZE);
        lpage.serialize(&mut bytes).unwrap();

        assert_eq!(bytes.len(), PAGE_SIZE);
        let deser = Page::deserialize(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(deser, lpage);
    }

    #[test]
    fn basic_internal_page_roundtrip() {
        let keys: Vec<Key> = (0..10).map(|i| Key::String(i.to_string())).collect();
        let children: Vec<PageId> = (0..11).map(|i| PageId::new(TableId::new(1), i)).collect();
        let ipage = Page {
            last_update: Lsn::new(10).unwrap(),
            parent: Some(PageId::new(TableId::new(1), 1)),
            body: PageBody::Internal { keys, children },
        };
        let mut bytes = Vec::with_capacity(PAGE_SIZE);
        ipage.serialize(&mut bytes).unwrap();

        assert_eq!(bytes.len(), PAGE_SIZE);
        let deser = Page::deserialize(&mut Cursor::new(bytes)).unwrap();

        assert_eq!(deser, ipage);
    }
}

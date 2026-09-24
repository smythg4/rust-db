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
pub const MAX_LEAF_HEADER_SIZE: usize = 38;
pub const MAX_INTERNAL_HEADER_SIZE: usize = 20;

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

#[derive(Debug, PartialEq, Clone)]
pub struct Page {
    last_update: Lsn,
    parent: Option<PageId>,
    body: PageBody,
}

#[derive(Debug, PartialEq, Clone)]
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

    pub fn free_space(&self) -> Option<usize> {
        let base_header_len = 1 // tag
            + 8 // Lsn
            + 1 + PAGE_ID_SIZE // parent_page
            + 2; // num_pages
        let used_size = match &self.body {
            PageBody::Internal { keys, children } => {
                let header_len = base_header_len;
                let slots_len = keys.len() * SLOT_ENTRY_SIZE;
                let children_len = children.len() * PAGE_ID_SIZE;
                let keys_len = keys.iter().map(|k| k.encoded_size()).sum::<usize>();
                header_len + slots_len + children_len + keys_len
            }
            PageBody::Leaf { records, .. } => {
                let header_len = base_header_len
                    + 1 + PAGE_ID_SIZE // next page
                    + 1 + PAGE_ID_SIZE; // prev page
                let slots_len = records.len() * SLOT_ENTRY_SIZE;
                let records_len = records.iter().map(|r| r.encoded_size()).sum::<usize>();
                header_len + slots_len + records_len
            }
        };
        PAGE_SIZE.checked_sub(used_size)
    }

    /// Writes header to underlying RawPage
    /// TODO: Figure out trait bounds to make this generic (Write + Seek)?
    pub fn write_header(&self, writer: &mut Cursor<RawPage>) -> Result<(), PageError> {
        // Write the header information: Type Tag, LSN, Parent page, then if
        // a Leaf node, write the Next and Prev pages.
        writer.write_all(&[self.page_type_tag()])?;
        self.last_update.serialize(writer)?;
        self.parent.serialize(writer)?;
        match self.body {
            PageBody::Leaf { next, prev, .. } => {
                next.serialize(writer)?;
                prev.serialize(writer)?;
            }
            PageBody::Internal { .. } => {}
        };

        // note the number of items stored on the page
        let num_items = self.num_items() as u16; // records.len() for Leaf, keys.len() for Internal
        writer.write_all(&num_items.to_be_bytes())?;
        Ok(())
    }

    /// Writes the body of a page to the underlying RawPage
    /// Returns slot_offset and data_offset (used for tests)
    /// TODO: Figure out trait bounds to make this generic (Write + Seek)?
    pub fn write_body(
        &self,
        writer: &mut Cursor<RawPage>,
        header_end: usize,
    ) -> Result<(usize, usize), PageError> {
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
                    writer.set_position(data_offset as u64);
                    writer.write_all(&buffer)?;

                    // build a SlotEntry to point to the newly written data and write
                    // it at the proper offset
                    let slot_entry = SlotEntry::new(data_offset as u16, len as u16);
                    writer.set_position(slot_offset as u64);
                    slot_entry.serialize(writer)?;

                    // advance the running slot_offset
                    slot_offset += SLOT_ENTRY_SIZE;
                }
                Ok((slot_offset, data_offset))
            }
            PageBody::Internal { keys, children } => {
                // children are fixed-width and inserted in order right after the header
                let mut children_pos = header_end;
                for child in children {
                    writer.set_position(children_pos as u64);
                    child.serialize(writer)?;
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
                    writer.set_position(data_offset as u64);
                    writer.write_all(&buffer)?;

                    // write the accompanying SlotEntry
                    let slot_entry = SlotEntry::new(data_offset as u16, length as u16);
                    writer.set_position(slot_offset as u64);
                    slot_entry.serialize(writer)?;
                    slot_offset += SLOT_ENTRY_SIZE;
                }
                Ok((slot_offset, data_offset))
            }
        }
    }

    pub fn as_raw_page(&self) -> Result<RawPage, PageError> {
        let mut cursor = Cursor::new([0u8; PAGE_SIZE]);

        self.write_header(&mut cursor)?;

        // Header complete, mark the position.
        let header_end = cursor.position() as usize;

        // write the body (result isn't important here)
        let _ = self.write_body(&mut cursor, header_end)?;

        Ok(cursor.into_inner())
    }

    pub fn can_insert(&self, row: &Row) -> bool {
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        row.encoded_size() + SLOT_ENTRY_SIZE < free_space
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

    fn encoded_size(&self) -> usize {
        PAGE_SIZE
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
    use quickcheck::{Arbitrary, Gen, TestResult};
    use quickcheck_macros::quickcheck;

    use super::*;
    use crate::commontypes::TableId;
    use crate::schema::RowValue;

    impl Arbitrary for PageBody {
        fn arbitrary(g: &mut Gen) -> Self {
            let kind = g.choose(&[INTERNAL_TAG, LEAF_TAG]).unwrap();
            match *kind {
                INTERNAL_TAG => {
                    let keys: Vec<Key> =
                        (0..10).map(|_| Key::String(String::arbitrary(g))).collect();
                    let children: Vec<PageId> = (0..11)
                        .map(|_| PageId::new(TableId::new(u32::arbitrary(g)), u32::arbitrary(g)))
                        .collect();
                    PageBody::Internal { keys, children }
                }
                LEAF_TAG => {
                    let records: Vec<Row> = (0..10).map(|_| Row::arbitrary(g)).collect();
                    let next = Some(PageId::new(
                        TableId::new(u32::arbitrary(g)),
                        u32::arbitrary(g),
                    ));
                    let prev = Some(PageId::new(
                        TableId::new(u32::arbitrary(g)),
                        u32::arbitrary(g),
                    ));

                    PageBody::Leaf {
                        records,
                        next,
                        prev,
                    }
                }
                _ => unreachable!(),
            }
        }
    }

    impl Arbitrary for Page {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut lsn = u64::arbitrary(g);
            while lsn == 0 {
                lsn = u64::arbitrary(g);
            }
            let last_update = Lsn::new(lsn).unwrap();
            let parent = Some(PageId::new(
                TableId::new(u32::arbitrary(g)),
                u32::arbitrary(g),
            ));
            let body = PageBody::arbitrary(g);
            Page {
                last_update,
                parent,
                body,
            }
        }
    }

    #[quickcheck]
    fn header_constant_is_right(page: Page) -> TestResult {
        let mut cursor = Cursor::new([0u8; PAGE_SIZE]);
        page.write_header(&mut cursor).unwrap();
        match page.body {
            PageBody::Internal { .. } => {
                assert_eq!(cursor.position(), MAX_INTERNAL_HEADER_SIZE as u64)
            }
            PageBody::Leaf { .. } => assert_eq!(cursor.position(), MAX_LEAF_HEADER_SIZE as u64),
        };
        TestResult::passed()
    }

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

    #[quickcheck]
    fn free_space_works(page: Page) -> TestResult {
        let free_space = page.free_space().unwrap();

        let mut raw_page = Cursor::new([0u8; PAGE_SIZE]);
        page.write_header(&mut raw_page).unwrap();
        let header_end = raw_page.position() as usize;
        let (slot_offset, data_offset) = page.write_body(&mut raw_page, header_end).unwrap();

        let actual_free_space = data_offset - slot_offset;

        assert_eq!(free_space, actual_free_space);
        TestResult::passed()
    }

    #[quickcheck]
    fn page_quickcheck_roundtrip(page: Page) -> TestResult {
        let mut bytes = Cursor::new(Vec::new());
        page.serialize(&mut bytes).unwrap();

        bytes.set_position(0);
        let deser_page = Page::deserialize(&mut bytes).unwrap();

        assert_eq!(page, deser_page);
        TestResult::passed()
    }
}

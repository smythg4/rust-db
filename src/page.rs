use crate::commontypes::{
    Key, KeyError, LsnError, PAGE_ID_SIZE, PageId, PageLsn, SLOT_ENTRY_SIZE, SlotEntry,
};
use crate::schema::{Row, RowValueError, ValidatedRow};
use crate::traits::Serializable;
use std::io::{Cursor, Read, Write};
use thiserror::Error;

pub const PAGE_SIZE: usize = 4096;
pub const LEAF_TAG: u8 = 1;
pub const INTERNAL_TAG: u8 = 2;
pub const MAX_LEAF_HEADER_SIZE: usize = 46;
pub const MAX_INTERNAL_HEADER_SIZE: usize = 28;

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
    #[error("Page got overfull before serialization")]
    PageOverFlow,
    #[error("Page too small to split: {0}")]
    TooSmallToSplit(PageId),
    #[error("Page too full to fit row -- need to split")]
    PageFull,
    #[error("Attempt to insert a duplicate key")]
    DuplicateKey,
    #[error("Attempt to insert into a non-leaf page")]
    NotLeaf,
}
pub type RawPage = [u8; PAGE_SIZE];

#[derive(Debug, PartialEq, Clone)]
pub struct Page {
    page_id: PageId,
    last_update: PageLsn,
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
        let header_len = match self.body {
            PageBody::Internal { .. } => MAX_INTERNAL_HEADER_SIZE,
            PageBody::Leaf { .. } => MAX_LEAF_HEADER_SIZE,
        };
        let used_size = match &self.body {
            PageBody::Internal { keys, children } => {
                let slots_len = keys.len() * SLOT_ENTRY_SIZE;
                let children_len = children.len() * PAGE_ID_SIZE;
                let keys_len = keys.iter().map(|k| k.encoded_size()).sum::<usize>();
                header_len + slots_len + children_len + keys_len
            }
            PageBody::Leaf { records, .. } => {
                let slots_len = records.len() * SLOT_ENTRY_SIZE;
                let records_len = records.iter().map(|r| r.encoded_size()).sum::<usize>();
                header_len + slots_len + records_len
            }
        };
        PAGE_SIZE.checked_sub(used_size)
    }

    /// Writes header to underlying RawPage
    /// TODO: Figure out trait bounds to make this generic (Write + Seek)?
    pub(crate) fn write_header(&self, writer: &mut Cursor<RawPage>) -> Result<(), PageError> {
        // Write the header information: Type Tag, LSN, Parent page, then if
        // a Leaf node, write the Next and Prev pages.
        writer.write_all(&[self.page_type_tag()])?;
        self.page_id.serialize(writer)?;
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
    pub(crate) fn write_body(
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
        if self.free_space().is_none() {
            return Err(PageError::PageOverFlow);
        }
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
        row.encoded_size() + SLOT_ENTRY_SIZE <= free_space
    }

    pub fn insert(&mut self, validated_row: ValidatedRow) -> Result<(), PageError> {
        let PageBody::Leaf { .. } = &mut self.body else {
            return Err(PageError::NotLeaf);
        };
        let new_key = validated_row.primary_key();
        let row: Row = validated_row.into();
        let can_insert = self.can_insert(&row);
        let PageBody::Leaf { records, .. } = &mut self.body else {
            unreachable!();
        };

        let insert_pos = match records.binary_search_by(|r| r.cmp_key(&new_key)) {
            Ok(_) => return Err(PageError::DuplicateKey),
            Err(i) => i,
        };

        if !can_insert {
            return Err(PageError::PageFull);
        }
        records.insert(insert_pos, row);
        Ok(())
    }

    // The old right neighbor's `prev` pointer will need to be updated to point to the new page returned
    pub fn split_page(&mut self, new_page_id: PageId) -> Result<(Key, Page), PageError> {
        let curr_page_id = self.page_id;
        match &mut self.body {
            PageBody::Internal { keys, children } => {
                if keys.len() < 3 {
                    return Err(PageError::TooSmallToSplit(self.page_id));
                }
                debug_assert!(
                    keys.windows(2).all(|w| w[0] < w[1]),
                    "keys aren't strictly increasing"
                );
                let split_point = keys.len() / 2;
                let mut new_keys = keys.split_off(split_point);
                let new_children = children.split_off(split_point + 1);
                let split_key = new_keys.remove(0);
                debug_assert_eq!(keys.len() + 1, children.len());
                debug_assert_eq!(new_keys.len() + 1, new_children.len());
                Ok((
                    split_key,
                    Page {
                        page_id: new_page_id,
                        last_update: PageLsn(None),
                        parent: self.parent,
                        body: PageBody::Internal {
                            keys: new_keys,
                            children: new_children,
                        },
                    },
                ))
            }
            PageBody::Leaf { records, next, .. } => {
                if records.len() < 2 {
                    return Err(PageError::TooSmallToSplit(self.page_id));
                }
                debug_assert!(
                    records
                        .iter()
                        .filter_map(|r| r.fields.first())
                        .map(|k| Key::try_from(k).unwrap())
                        .is_sorted()
                );
                let split_point = records.len() / 2;
                let new_records = records.split_off(split_point);
                let split_key: Key = new_records
                    .first()
                    .expect("missing promoting key")
                    .fields
                    .first()
                    .expect("missing primary key")
                    .try_into()
                    .expect("invalid key");

                let new_page = Page {
                    page_id: new_page_id,
                    last_update: PageLsn(None),
                    parent: self.parent,
                    body: PageBody::Leaf {
                        records: new_records,
                        prev: Some(curr_page_id),
                        next: *next,
                    },
                };

                // connect current node to new right neighbor
                *next = Some(new_page_id);
                Ok((split_key, new_page))
            }
        }
    }
}

impl Serializable for Page {
    type Error = PageError;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        let raw = self.as_raw_page()?;
        w.write_all(&raw)?;
        Ok(())
    }
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
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

        let page_id = PageId::deserialize(&mut cursor)?;
        let last_update = PageLsn::deserialize(&mut cursor)?;
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
                let slot_range = SlotEntry::deserialize(&mut cursor)?.range();
                records.push(Row::deserialize(
                    &mut buf.get(slot_range).expect("bad range for leaf page!"),
                )?);
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
                let slot_range = SlotEntry::deserialize(&mut cursor)?.range();
                keys.push(Key::deserialize(
                    &mut buf.get(slot_range).expect("bad range for internal page!"),
                )?);
            }
            PageBody::Internal { keys, children }
        };
        Ok(Page {
            page_id,
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
    use std::cmp::Ordering;

    use quickcheck::{Arbitrary, Gen, TestResult};
    use quickcheck_macros::quickcheck;

    use super::*;
    use crate::commontypes::{Lsn, TableId};
    use crate::schema::tests::SchemaRowPair;
    use crate::schema::tests::valid_row_from_schema;
    use crate::schema::{RowValue, Schema};

    impl Arbitrary for PageBody {
        fn arbitrary(g: &mut Gen) -> Self {
            let kind = g.choose(&[INTERNAL_TAG, LEAF_TAG]).unwrap();
            match *kind {
                INTERNAL_TAG => {
                    let mut keys: Vec<Key> =
                        (0..10).map(|_| Key::String(String::arbitrary(g))).collect();
                    keys.sort();
                    keys.dedup();
                    let children: Vec<PageId> = (0..keys.len() + 1)
                        .map(|_| PageId::new(TableId::new(u32::arbitrary(g)), u32::arbitrary(g)))
                        .collect();
                    PageBody::Internal { keys, children }
                }
                LEAF_TAG => {
                    let mut records: Vec<Row> = (0..10).map(|_| Row::arbitrary(g)).collect();
                    records.sort_by_key(|r| Key::try_from(&r.fields[0]).unwrap());
                    records.dedup_by_key(|r| Key::try_from(&r.fields[0]).unwrap());
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
            let page_id = PageId::new(TableId::new(u32::arbitrary(g)), u32::arbitrary(g));
            let last_update = PageLsn(Option::<Lsn>::arbitrary(g));
            let parent = Some(PageId::new(
                TableId::new(u32::arbitrary(g)),
                u32::arbitrary(g),
            ));
            let body = PageBody::arbitrary(g);
            let mut page = Page {
                page_id,
                last_update,
                parent,
                body,
            };
            while page.free_space().is_none() {
                page.body = PageBody::arbitrary(g);
            }
            page
        }
    }

    #[derive(Debug, Clone)]
    struct SchemaWithRows(Schema, Vec<Row>);

    impl Arbitrary for SchemaWithRows {
        fn arbitrary(g: &mut Gen) -> Self {
            let schema = Schema::arbitrary(g);
            let n = usize::arbitrary(g) % 25;
            let rows: Vec<Row> = (0..n).map(|_| valid_row_from_schema(&schema, g)).collect();
            SchemaWithRows(schema, rows)
        }
    }

    #[quickcheck]
    fn insertion_order_on_leaves(
        SchemaWithRows(schema, rows): SchemaWithRows,
        mut page: Page,
    ) -> TestResult {
        if let PageBody::Leaf { records, .. } = &mut page.body {
            records.clear();
        } else {
            return TestResult::discard();
        };
        use std::collections::BTreeMap;
        let mut expected = BTreeMap::new();

        for row in rows {
            let validated_row = schema.validate_row(row).unwrap();
            let key = validated_row.primary_key();
            let row: Row = validated_row.clone().into();

            let is_duplicate = expected.contains_key(&key);
            let is_full = !page.can_insert(&row);

            let result = page.insert(validated_row);
            if is_duplicate {
                assert!(matches!(result, Err(PageError::DuplicateKey)));
            } else if is_full {
                assert!(matches!(result, Err(PageError::PageFull)));
            } else {
                assert!(result.is_ok());
                expected.insert(key, row.clone());
            }
        }
        let PageBody::Leaf {
            records: actual_records,
            ..
        } = page.body
        else {
            unreachable!()
        };
        let expected_records: Vec<Row> = expected.into_values().collect();
        assert_eq!(expected_records, actual_records);
        TestResult::passed()
    }

    #[quickcheck]
    fn split_page_roundtrip(mut page: Page) -> TestResult {
        let new_page_id = PageId::new(TableId::new(u32::MAX), u32::MAX);
        if let Ok((_, new_page)) = page.split_page(new_page_id) {
            let mut buf = Cursor::new(Vec::new());
            page.serialize(&mut buf).unwrap();
            buf.set_position(0);
            let deser = Page::deserialize(&mut buf).unwrap();
            assert_eq!(page, deser, "Original page doesn't roundtrip");

            buf.set_position(0);
            new_page.serialize(&mut buf).unwrap();
            buf.set_position(0);
            let deser = Page::deserialize(&mut buf).unwrap();
            assert_eq!(new_page, deser, "New page doesn't roundtrip");
            TestResult::passed()
        } else {
            TestResult::discard()
        }
    }

    #[quickcheck]
    fn split_page_key_in_right_spot(mut page: Page) -> TestResult {
        let new_page_id = PageId::new(TableId::new(u32::MAX), u32::MAX);
        if let Ok((split_key, new_page)) = page.split_page(new_page_id) {
            match page.body {
                PageBody::Internal { keys, .. } => {
                    assert!(!keys.is_empty());
                    assert!(keys.iter().all(|k| k < &split_key));
                    let PageBody::Internal { keys: new_keys, .. } = new_page.body else {
                        unreachable!()
                    };
                    assert!(!new_keys.is_empty());
                    assert!(new_keys.iter().all(|k| k > &split_key));
                }
                PageBody::Leaf { records, .. } => {
                    assert!(!records.is_empty());
                    assert!(
                        records
                            .iter()
                            .all(|r| r.cmp_key(&split_key) == Ordering::Less)
                    );
                    let PageBody::Leaf {
                        records: new_records,
                        ..
                    } = new_page.body
                    else {
                        unreachable!()
                    };
                    assert!(!new_records.is_empty());
                    assert!(
                        new_records
                            .iter()
                            .all(|r| r.cmp_key(&split_key) != Ordering::Less)
                    );
                }
            };
            TestResult::passed()
        } else {
            TestResult::discard()
        }
    }

    #[quickcheck]
    fn sibling_and_parent_pointers_correct_after_split(mut page: Page) -> TestResult {
        if matches!(page.body, PageBody::Internal { .. }) {
            return TestResult::discard();
        }
        let original_id = page.page_id;
        let new_page_id = PageId::new(TableId::new(u32::MAX), u32::MAX);
        let PageBody::Leaf {
            next: old_next,
            prev: old_prev,
            ..
        } = page.body.clone()
        else {
            unreachable!()
        };
        if let Ok((_, new_page)) = page.split_page(new_page_id) {
            match page.body {
                PageBody::Internal { .. } => unreachable!(),
                PageBody::Leaf { next, prev, .. } => {
                    assert_eq!(prev, old_prev, "original page prev pointer wasn't retained");
                    assert_eq!(
                        next,
                        Some(new_page_id),
                        "original page doesn't point to new page"
                    );
                    let PageBody::Leaf {
                        prev: new_prev,
                        next: new_next,
                        ..
                    } = new_page.body
                    else {
                        unreachable!()
                    };
                    assert_eq!(
                        new_prev,
                        Some(original_id),
                        "new page prev pointer doesn't point to original page"
                    );
                    assert_eq!(
                        new_next, old_next,
                        "new page next pointer doesn't point to original page's original next"
                    );
                }
            };
            assert_eq!(
                page.parent, new_page.parent,
                "new and original page should have same parent"
            );
            TestResult::passed()
        } else {
            TestResult::discard()
        }
    }

    #[quickcheck]
    fn no_rows_lost_in_split(mut page: Page) -> TestResult {
        let new_page_id = PageId::new(TableId::new(u32::MAX), u32::MAX);
        let mut original_keys: Vec<Key> = Vec::new();
        let mut original_children: Vec<PageId> = Vec::new();
        let mut original_records: Vec<Row> = Vec::new();

        match &page.body {
            PageBody::Internal { keys, children } => {
                original_keys = keys.clone();
                original_children = children.clone();
            }
            PageBody::Leaf { records, .. } => {
                original_records = records.clone();
            }
        };

        if let Ok((split_key, new_page)) = page.split_page(new_page_id) {
            match new_page.body {
                PageBody::Internal {
                    keys: new_keys,
                    children: new_children,
                } => {
                    let PageBody::Internal {
                        keys: old_keys,
                        children: old_children,
                    } = page.body.clone()
                    else {
                        unreachable!()
                    };
                    let combined_keys: Vec<Key> = old_keys
                        .clone()
                        .into_iter()
                        .chain(new_keys.into_iter())
                        .collect();
                    let combined_children: Vec<PageId> = old_children
                        .clone()
                        .into_iter()
                        .chain(new_children.into_iter())
                        .collect();

                    // remove the split key from the internal node original list of Keys
                    let remove_pos = original_keys.binary_search(&split_key).unwrap();
                    original_keys.remove(remove_pos);

                    assert_eq!(original_keys, combined_keys);
                    assert_eq!(original_children, combined_children);
                }
                PageBody::Leaf {
                    records: new_records,
                    ..
                } => {
                    let PageBody::Leaf {
                        records: old_records,
                        ..
                    } = page.body.clone()
                    else {
                        unreachable!()
                    };
                    let combined_records: Vec<Row> = old_records
                        .clone()
                        .into_iter()
                        .chain(new_records.into_iter())
                        .collect();
                    assert_eq!(original_records, combined_records);
                }
            }

            TestResult::passed()
        } else {
            TestResult::discard()
        }
    }

    #[quickcheck]
    fn page_insert_returns_page_full_when_full(
        mut page: Page,
        SchemaRowPair(schema, row): SchemaRowPair,
    ) -> TestResult {
        if !matches!(page.body, PageBody::Leaf { .. }) {
            return TestResult::discard();
        }
        if page.can_insert(&row) {
            return TestResult::discard();
        }

        let validated_row = schema.validate_row(row).unwrap();

        let result = page.insert(validated_row);

        assert!(matches!(result, Err(PageError::PageFull)), "{result:?}");
        TestResult::passed()
    }

    #[quickcheck]
    fn duplicate_keys_trigger_error_on_insert(
        mut page: Page,
        SchemaRowPair(schema, row): SchemaRowPair,
    ) -> TestResult {
        if !matches!(page.body, PageBody::Leaf { .. }) {
            return TestResult::discard();
        }
        if !page.can_insert(&row) {
            return TestResult::discard();
        }
        let validated_row = schema.validate_row(row).unwrap();
        let _ = page.insert(validated_row.clone()); // this might error if the key already exists, but we're guaranteed to have it in there after calling it
        let result = page.insert(validated_row); // this is the check that matters

        assert!(matches!(result, Err(PageError::DuplicateKey)), "{result:?}");
        TestResult::passed()
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
            page_id: PageId::new(TableId::new(10), 10),
            last_update: PageLsn(Some(Lsn::new(10).unwrap())),
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
            page_id: PageId::new(TableId::new(10), 10),
            last_update: PageLsn(Some(Lsn::new(10).unwrap())),
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

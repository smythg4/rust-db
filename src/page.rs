use crate::commontypes::{
    Key, KeyError, LsnError, PAGE_ID_SIZE, PageId, PageLsn, SLOT_ENTRY_SIZE, SlotEntry,
};
use crate::schema::{Row, RowValue, RowValueError, ValidatedRow};
use crate::traits::Serializable;
use crc32_light::Crc32Stream;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use thiserror::Error;

pub const PAGE_SIZE: usize = 4096;
pub const LEAF_TAG: u8 = 1;
pub const INTERNAL_TAG: u8 = 2;
pub const MAX_LEAF_HEADER_SIZE: usize = 41;
pub const MAX_INTERNAL_HEADER_SIZE: usize = 23;
pub const CHECKSUM_OFFSET: usize = 17;

/// The maximum size for an entry into a leaf, decided to ensure that upon a Page split
/// an entry will fit after
pub const MAX_LEAF_ENTRY_SIZE: usize = (PAGE_SIZE - MAX_LEAF_HEADER_SIZE) / 4;

/// The maximum size for an key going into an internal page, decided to ensure that upon a Page split
/// an entry will fit after
pub const MAX_INTERNAL_ENTRY_SIZE: usize =
    (PAGE_SIZE - MAX_INTERNAL_HEADER_SIZE - PAGE_ID_SIZE) / 4;

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
    #[error("Page got overfull before serialization")]
    PageOverFlow,
    #[error("Page too small to split: {0}")]
    TooSmallToSplit(PageId),
    #[error("Page too full to fit row -- need to split")]
    PageFull,
    #[error("Attempt to insert a duplicate key")]
    DuplicateKey,
    #[error("Attempt to insert a row into a non-leaf page")]
    NotLeaf,
    #[error("Attempt to insert a separator into a non-internal page")]
    NotInternal,
    #[error("invariant violated")]
    InvariantViolated,
    #[error("Corrupt data found on page: {page_id:?}. {kind:?}")]
    Corrupt {
        page_id: Option<PageId>,
        kind: CorruptionKind,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum CorruptionKind {
    InvalidTag(u8),
    InvalidRange(Range<usize>),
    MissingKey,
    InvalidKey(RowValue),
    UnsortedKeys,
    UnsortedRecords,
    PageWouldOverFlow,
    InvalidLsn,
    UnknownCorruption,
    CheckSumMismatch,
}
pub type RawPage = [u8; PAGE_SIZE];

#[derive(Debug, PartialEq, Clone)]
pub struct Page {
    page_id: PageId,
    last_update: PageLsn,
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
    pub fn empty_leaf(page_id: PageId) -> Self {
        let body = PageBody::Leaf {
            records: Vec::new(),
            next: None,
            prev: None,
        };
        Self::empty_page(page_id, body)
    }

    fn empty_page(page_id: PageId, body: PageBody) -> Self {
        Page {
            page_id,
            last_update: PageLsn(None),
            body,
        }
    }

    #[allow(dead_code)]
    fn new_root(page_id: PageId, left: PageId, separator: Key, right: PageId) -> Self {
        let body = PageBody::Internal {
            keys: vec![separator],
            children: vec![left, right],
        };
        Page {
            page_id,
            last_update: PageLsn(None),
            body,
        }
    }

    fn page_type_tag(&self) -> u8 {
        match self.body {
            PageBody::Internal { .. } => INTERNAL_TAG,
            PageBody::Leaf { .. } => LEAF_TAG,
        }
    }

    /// Returns the number of records present in a `Leaf` page or the number of keys present in a `Internal` page.
    pub fn num_items(&self) -> usize {
        match &self.body {
            PageBody::Leaf { records, .. } => records.len(),
            PageBody::Internal { keys, .. } => keys.len(),
        }
    }

    /// Returns the number of bytes available for data in the page. Because the headers can be of variable size depending
    /// on the `Option` variant, we always reserve the maximum space required (e.g. assuming we have two siblings in the `Leaf`
    /// case). Returns `None` if the `Page` won't fit into a [u8; PAGE_SIZE] space, which means somewhere data overflowed.
    pub fn free_space(&self) -> Option<usize> {
        let header_len = match self.body {
            PageBody::Internal { .. } => MAX_INTERNAL_HEADER_SIZE,
            PageBody::Leaf { .. } => MAX_LEAF_HEADER_SIZE,
        };
        let used_size = header_len
            + match &self.body {
                // extra PAGE_ID_SIZE is to account for the right child in the internal page
                PageBody::Internal { keys, .. } => {
                    keys.iter().map(Self::internal_entry_size).sum::<usize>() + PAGE_ID_SIZE
                }
                PageBody::Leaf { records, .. } => {
                    records.iter().map(Self::leaf_entry_size).sum::<usize>()
                }
            };
        PAGE_SIZE.checked_sub(used_size)
    }

    /// Writes `Page` header data to writer.
    pub(crate) fn write_header<W: Write>(&self, writer: &mut W) -> Result<(), PageError> {
        // Write the header information: Type Tag, LSN, a CRC32 checksum, then if
        // a Leaf node, write the Next and Prev pages.
        writer.write_all(&[self.page_type_tag()])?;
        self.page_id.serialize(writer)?;
        self.last_update.serialize(writer)?;
        // place holder for checksum
        writer.write_all(&0u32.to_be_bytes())?;
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

    /// Writes the body of a page to the writer
    /// Returns `slot_offset` and `data_offset` (used for tests to confirm that `free_space` is working properly) for the `Page`
    /// `data_offset` - `slot_offset` is the range of unused space on the `Page`
    pub(crate) fn write_body<W: Write + Seek>(
        &self,
        writer: &mut W,
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
                    writer.seek(SeekFrom::Start(data_offset as u64))?;
                    writer.write_all(&buffer)?;

                    // build a SlotEntry to point to the newly written data and write
                    // it at the proper offset
                    let slot_entry = SlotEntry::new(data_offset as u16, len as u16);
                    writer.seek(SeekFrom::Start(slot_offset as u64))?;
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
                    writer.seek(SeekFrom::Start(children_pos as u64))?;
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
                    writer.seek(SeekFrom::Start(data_offset as u64))?;
                    writer.write_all(&buffer)?;

                    // write the accompanying SlotEntry
                    let slot_entry = SlotEntry::new(data_offset as u16, length as u16);
                    writer.seek(SeekFrom::Start(slot_offset as u64))?;
                    slot_entry.serialize(writer)?;
                    slot_offset += SLOT_ENTRY_SIZE;
                }
                Ok((slot_offset, data_offset))
            }
        }
    }

    /// Returns the `Page` represented as raw bytes (`RawPage = [0u8; PAGE_SIZE]`).
    /// Will error if write to internal `Cursor` fails or if the `Page` would overflow
    /// a `RawPage`.
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

        let mut buf = cursor.into_inner();
        let crc = Self::page_checksum(&buf);
        buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());

        Ok(buf)
    }

    fn page_checksum(raw_page: &RawPage) -> u32 {
        let mut crc_stream = Crc32Stream::new();
        crc_stream.update(&raw_page[..CHECKSUM_OFFSET]);
        crc_stream.update(&raw_page[CHECKSUM_OFFSET + 4..]);
        crc_stream.finalize()
    }

    /// Returns `true` if `row` can fit in the `Page` without overflowing
    /// a `RawPage` when converted to raw bytes.
    pub fn can_insert(&self, row: &Row) -> bool {
        // immediately return `false` if the page is already overfull
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        // new row will be encoded and a corresponding slot index is allocated, both
        // parts need to fit
        Self::leaf_entry_size(row) <= free_space
    }

    /// Inserts a separator `Key` and associated child `PageId` into an internal `Page`.
    /// Primarily called by parent datastructure (e.g. `BTree`) after splitting a `Page`
    /// lower in the tree.
    #[allow(dead_code)]
    fn internal_insert(&mut self, _separator: Key, _right_child: PageId) -> Result<(), PageError> {
        todo!()
    }

    /// Inserts a `ValidatedRow` into the `Page`. `ValidatedRow`s are those that are checked
    /// by a `Schema` type to ensure the `Row` conforms to the rules in the `Schema`.
    /// Will return `Error` if `Page` isn't of the `Leaf` variety, the `Key` is a duplicate,
    /// or the `Page` can't fit it.
    pub fn leaf_insert(&mut self, validated_row: ValidatedRow) -> Result<(), PageError> {
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

    /// Returns the fully loaded cost for inserting a `Key` into an Internal page to include the slot entry
    /// and child page pointer
    pub(crate) fn internal_entry_size(k: &Key) -> usize {
        k.encoded_size() + SLOT_ENTRY_SIZE + PAGE_ID_SIZE
    }

    /// Returns the fully loaded cost for inserting a `Row` into an Leaf page to include the slot entry
    pub(crate) fn leaf_entry_size(r: &Row) -> usize {
        r.encoded_size() + SLOT_ENTRY_SIZE
    }

    /// Accepts a `new_page_id` to assign to the new `Page` and splits the current `Page` into two
    /// parts. Used by a controlling `B+Tree` structure when `Page`s would overflow from an `insert`.
    /// Returns the new `Page` (new right neighbor) and promoted `Key`.
    /// Neighbor pointers for this `Page` and the new `Page` are updated in this call.
    /// Will return `Error` if the `Page` is too small to split (fewer than 3 keys for `Internal`, fewer
    /// than 2 records for a `Leaf`).
    /// Note: The caller is responsible for inserting the returned `Key` into the parent `Page` and
    /// updating the old right neighbor's `prev` pointer to the new `Page` returned
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

                // Split point is based on key size instead of purely indexing halfway through the Vec.
                let split_size = keys.iter().map(Self::internal_entry_size).sum::<usize>() / 2;
                let split_point = keys
                    .iter()
                    .scan(0, |acc, k| {
                        *acc += Self::internal_entry_size(k);
                        Some(*acc)
                    })
                    .position(|total| total >= split_size)
                    .unwrap();

                // split_point is where the total first reaches halfway and we want
                // to split right after that point (+1). We clamp the result in the event
                // of a very large last key we need at least 2 elements in the right set
                // (since we're going to remove one to promote)
                let split_off_point = (split_point + 1).clamp(1, keys.len() - 2);

                let mut new_keys = keys.split_off(split_off_point);
                let new_children = children.split_off(split_off_point + 1);

                let split_key = new_keys.remove(0);
                debug_assert_eq!(keys.len() + 1, children.len());
                debug_assert_eq!(new_keys.len() + 1, new_children.len());
                Ok((
                    split_key,
                    Page {
                        page_id: new_page_id,
                        last_update: PageLsn(None),
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
                        .map(|r| Key::try_from(&r.fields[0]).unwrap())
                        .is_sorted_by(|a, b| a < b),
                    "record aren't sorted by strictly increasing"
                );

                // Split point is based on row size instead of purely indexing halfway through the Vec.
                let split_size = records.iter().map(Self::leaf_entry_size).sum::<usize>() / 2;
                let split_point = records
                    .iter()
                    .scan(0, |acc, r| {
                        *acc += Self::leaf_entry_size(r);
                        Some(*acc)
                    })
                    .position(|total| total >= split_size)
                    .unwrap();

                // split_point is where the total first reaches halfway and we want
                // to split right after that point (+1). We clamp the result in the event
                // of a very large last row we need at least 1 elements in the right set
                let split_off_point = (split_point + 1).clamp(1, records.len() - 1);

                let new_records = records.split_off(split_off_point);
                let split_key: Key = new_records
                    .first()
                    .ok_or(PageError::InvariantViolated)?
                    .fields
                    .first()
                    .ok_or(PageError::InvariantViolated)?
                    .try_into()
                    .map_err(|_| PageError::InvariantViolated)?;

                let new_page = Page {
                    page_id: new_page_id,
                    last_update: PageLsn(None),
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

        // read the checksum
        let crc_buf = &buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4];
        let checksum = u32::from_be_bytes(crc_buf.try_into().expect("always 4 byte slice"));

        let computed_checksum = Self::page_checksum(&buf);
        if computed_checksum != checksum {
            return Err(PageError::Corrupt {
                page_id: None,
                kind: CorruptionKind::CheckSumMismatch,
            });
        }

        let mut cursor = Cursor::new(&buf[..]);

        let mut buf_one = [0u8; 1];
        cursor.read_exact(&mut buf_one)?;
        let tag = buf_one[0];
        let is_leaf = match tag {
            INTERNAL_TAG => false,
            LEAF_TAG => true,
            _ => {
                return Err(PageError::Corrupt {
                    page_id: None,
                    kind: CorruptionKind::InvalidTag(tag),
                });
            }
        };

        let page_id = PageId::deserialize(&mut cursor)?;

        let last_update = PageLsn::deserialize(&mut cursor)?;

        // skip the crc32 portion
        cursor.set_position(CHECKSUM_OFFSET as u64 + 4);

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
                if slot_range.is_empty() {
                    return Err(PageError::Corrupt {
                        page_id: Some(page_id),
                        kind: CorruptionKind::InvalidRange(slot_range),
                    });
                }
                records.push(Row::deserialize(&mut buf.get(slot_range.clone()).ok_or(
                    PageError::Corrupt {
                        page_id: Some(page_id),
                        kind: CorruptionKind::InvalidRange(slot_range),
                    },
                )?)?);
            }

            // check that all the keys are valid for the records
            let keys: Vec<Key> = records
                .iter()
                .map(|r| {
                    let first = r.fields.first().ok_or(PageError::Corrupt {
                        page_id: Some(page_id),
                        kind: CorruptionKind::MissingKey,
                    })?;
                    Key::try_from(first).map_err(|_| PageError::Corrupt {
                        page_id: Some(page_id),
                        kind: CorruptionKind::InvalidKey(first.clone()),
                    })
                })
                .collect::<Result<Vec<Key>, PageError>>()?;

            // ensure the keys are sorted
            if !keys.iter().is_sorted_by(|a, b| a < b) {
                return Err(PageError::Corrupt {
                    page_id: Some(page_id),
                    kind: CorruptionKind::UnsortedKeys,
                });
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
                keys.push(Key::deserialize(&mut buf.get(slot_range.clone()).ok_or(
                    PageError::Corrupt {
                        page_id: Some(page_id),
                        kind: CorruptionKind::InvalidRange(slot_range),
                    },
                )?)?);
            }
            // ensure there's always one more child than keys - this is impossible to fail
            if keys.len() + 1 != children.len() {
                std::hint::cold_path();
                return Err(PageError::InvariantViolated);
            }
            // ensure the keys are sorted
            if !keys.windows(2).all(|w| w[0] < w[1]) {
                return Err(PageError::Corrupt {
                    page_id: Some(page_id),
                    kind: CorruptionKind::UnsortedKeys,
                });
            }
            PageBody::Internal { keys, children }
        };

        let page = Page {
            page_id,
            last_update,
            body,
        };

        // ensure that the page won't overflow PAGE_SIZE bytes
        if page.free_space().is_none() {
            return Err(PageError::Corrupt {
                page_id: Some(page_id),
                kind: CorruptionKind::PageWouldOverFlow,
            });
        }
        Ok(page)
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
    use crate::schema::{Column, RowValue, Schema};

    impl Arbitrary for PageBody {
        fn arbitrary(g: &mut Gen) -> Self {
            let coin_flip = bool::arbitrary(g);
            match coin_flip {
                true => {
                    let coin_flip = bool::arbitrary(g);
                    let mut keys: Vec<Key> = if coin_flip {
                        (0..10).map(|_| Key::String(String::arbitrary(g))).collect()
                    } else {
                        (0..10).map(|_| Key::Integer(i64::arbitrary(g))).collect()
                    };
                    keys.sort();
                    keys.dedup();
                    let children: Vec<PageId> = (0..keys.len() + 1)
                        .map(|_| PageId::new(TableId::new(u32::arbitrary(g)), u32::arbitrary(g)))
                        .collect();
                    PageBody::Internal { keys, children }
                }
                false => {
                    let mut records: Vec<Row> = (0..10).map(|_| Row::arbitrary(g)).collect();
                    records.sort_by_key(|r| Key::try_from(&r.fields[0]).unwrap());
                    records.dedup_by_key(|r| Key::try_from(&r.fields[0]).unwrap());

                    let next = Option::<PageId>::arbitrary(g);
                    let prev = Option::<PageId>::arbitrary(g);

                    PageBody::Leaf {
                        records,
                        next,
                        prev,
                    }
                }
            }
        }
    }

    impl Arbitrary for Page {
        fn arbitrary(g: &mut Gen) -> Self {
            let page_id = PageId::arbitrary(g);
            let last_update = PageLsn(Option::<Lsn>::arbitrary(g));

            let body = PageBody::arbitrary(g);
            let mut page = Page {
                page_id,
                last_update,
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

    /// Fixed-width key prefix so string keys sort by their index regardless of padding.
    fn padded_key(index: usize, total_len: usize) -> Key {
        let prefix = format!("{index:06}");
        Key::String(format!(
            "{prefix}{}",
            "x".repeat(total_len.saturating_sub(prefix.len()))
        ))
    }

    /// Two-column schema (integer key + string payload) and the largest payload that still validates.
    fn leaf_schema() -> (Schema, usize) {
        let schema = Schema::try_from(vec![Column::integer(), Column::string()]).unwrap();
        let row_with = |key: i64, len: usize| Row {
            fields: vec![RowValue::Integer(key), RowValue::String("p".repeat(len))],
        };
        let max_payload = (0..MAX_LEAF_ENTRY_SIZE)
            .rev()
            .find(|&len| schema.validate_row(row_with(0, len)).is_ok())
            .expect("some payload must validate");
        (schema, max_payload)
    }

    #[quickcheck]
    fn max_size_row_fits_after_leaf_split(payload_sizes: Vec<u16>, target: u8) -> TestResult {
        let page_id = PageId::new(TableId::new(u32::MAX), u32::MAX);

        if payload_sizes.is_empty() {
            return TestResult::discard();
        }
        let (schema, max_payload) = leaf_schema();

        // helper closure to build a row from the schema
        let make_row = |key: i64, len: usize| {
            schema
                .validate_row(Row {
                    fields: vec![RowValue::Integer(key), RowValue::String("p".repeat(len))],
                })
                .unwrap()
        };

        // generate an empty leaf page
        let mut page = Page::empty_leaf(page_id);
        let mut count = 0i64;

        // fill it up with various sized rows and keys 0, 10, 20, ...
        for len in payload_sizes.iter().cycle() {
            let len = *len as usize % (max_payload + 1);
            match page.leaf_insert(make_row(count * 10, len)) {
                Ok(()) => count += 1,
                Err(PageError::PageFull) => break,
                Err(e) => panic!("Unexpected error: {e:?}"),
            }
        }

        // split the page! (the new one will have a duplicate page id, but that's fine for the test)
        let (separator, mut right) = page.split_page(page_id).unwrap();

        // generate a key that lands between entries
        let key = (target as i64 % (count + 1)) * 10 - 5;
        // generate a max size payload
        let big_row = make_row(key, max_payload);

        let result = if Key::Integer(key) >= separator {
            right.leaf_insert(big_row)
        } else {
            page.leaf_insert(big_row)
        };

        assert!(
            result.is_ok(),
            "max size row didn't fit after split: {result:?}"
        );

        TestResult::passed()
    }

    #[quickcheck]
    fn max_size_key_fits_after_internal_split(key_sizes: Vec<u16>, target: u16) -> TestResult {
        let page_id = PageId::new(TableId::new(u32::MAX), u32::MAX);
        if key_sizes.is_empty() {
            return TestResult::discard();
        }

        // largest string key whose internal entry still fits the limit
        let max_key_len = (0..MAX_INTERNAL_ENTRY_SIZE)
            .rev()
            .find(|&len| Page::internal_entry_size(&padded_key(0, len)) <= MAX_INTERNAL_ENTRY_SIZE)
            .unwrap();

        // fill and internal page with keys 0, 2, 4, ... until one won't fit
        let mut keys: Vec<Key> = Vec::new();
        let mut children = vec![page_id];
        for (i, size) in key_sizes.iter().cycle().enumerate() {
            let key = padded_key(i * 2, 6 + *size as usize % (max_key_len - 5));
            // this silliness is because I don't have an internal page insert yet
            let page = Page::empty_page(
                page_id,
                PageBody::Internal {
                    keys: keys.clone(),
                    children: children.clone(),
                },
            );
            if page.free_space().unwrap() < Page::internal_entry_size(&key) {
                break;
            }
            keys.push(key);
            children.push(page_id);
        }

        let mut page = Page::empty_page(page_id, PageBody::Internal { keys, children });
        let key_count = page.num_items();

        let (separator, right) = match page.split_page(page_id) {
            Ok(split) => split,
            Err(PageError::TooSmallToSplit(_)) => return TestResult::discard(),
            Err(e) => panic!("Unexpected split error: {e:?}"),
        };

        // a max size key whose prefix lands between existing keys
        let new_key = padded_key((target as usize % (key_count + 1)) * 2 + 1, max_key_len);
        let half = if new_key >= separator { &right } else { &page };
        let free = half.free_space().unwrap();

        // TODO: change this to an actual insert once I have an insert method for internal pages
        assert!(
            free >= Page::internal_entry_size(&new_key),
            "max size target needs {} bytes but the target half only has {free}",
            Page::internal_entry_size(&new_key)
        );
        TestResult::passed()
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

            let page_clone = page.clone();

            let result = page.leaf_insert(validated_row);
            if is_duplicate {
                assert!(matches!(result, Err(PageError::DuplicateKey)));
                assert_eq!(page_clone, page);
            } else if is_full {
                assert!(matches!(result, Err(PageError::PageFull)));
                assert_eq!(page_clone, page);
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
        match page.split_page(new_page_id) {
            Ok((_, new_page)) => {
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
            }
            Err(PageError::TooSmallToSplit(_)) => TestResult::discard(),
            Err(_) => TestResult::failed(),
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
    fn sibling_pointers_correct_after_split(mut page: Page) -> TestResult {
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

    fn increment_key_on_row(row: &Row) -> Row {
        let mut new_row = row.clone();
        let new_key = match &row.fields[0] {
            RowValue::Integer(n) => RowValue::Integer(n.wrapping_add(1)),
            RowValue::String(s) => RowValue::String(format!("{s}1")),
            _ => unreachable!(),
        };
        new_row.fields[0] = new_key;
        new_row
    }

    #[quickcheck]
    fn page_insert_returns_page_full_when_full(
        mut page: Page,
        SchemaRowPair(schema, mut row): SchemaRowPair,
    ) -> TestResult {
        page.body = PageBody::Leaf {
            records: Vec::new(),
            next: None,
            prev: None,
        };

        // fill the page up entries
        while page.can_insert(&row) {
            let vr = schema.validate_row(row.clone()).unwrap();
            page.leaf_insert(vr).unwrap();
            row = increment_key_on_row(&row);
        }
        let snapshot = page.clone();

        // one more insert should trigger page full
        let vr = schema.validate_row(row.clone()).unwrap();
        let result = page.leaf_insert(vr);
        assert!(matches!(result, Err(PageError::PageFull)));

        // make sure the failed insert didn't change the underlying page
        assert_eq!(page.as_raw_page().unwrap(), snapshot.as_raw_page().unwrap());

        TestResult::passed()
    }

    #[quickcheck]
    fn random_inputs_never_panic_on_deserialize(raw_page: [u8; PAGE_SIZE]) -> TestResult {
        let _ = Page::deserialize(&mut Cursor::new(raw_page));
        // this will likely always fail, but it's possible that quickcheck generated a valid random input
        // we only care that the call doesn't panic
        TestResult::passed()
    }

    #[quickcheck]
    fn any_burst_of_up_to_4_bytes_is_detected(
        page: Page,
        pos: u16,
        len: u8,
        masks: [u8; 4],
    ) -> TestResult {
        let mut bytes = page.as_raw_page().unwrap();

        // CRC32 is guaranteed to detect any change confined to 4 consecutive bytes.
        let len = 1 + len as usize % 4;
        let start = pos as usize % (PAGE_SIZE - len + 1);

        // XOR with a nonzero mask always changes the byte (writing a value might not).
        for (offset, mask) in masks.iter().take(len).enumerate() {
            bytes[start + offset] ^= (*mask).max(1);
        }

        let result = Page::deserialize(&mut &bytes[..]);
        assert!(
            matches!(
                result,
                Err(PageError::Corrupt {
                    kind: CorruptionKind::CheckSumMismatch,
                    ..
                })
            ),
            "{len} changed byte(s) at offset {start} not detected: {result:?}"
        );
        TestResult::passed()
    }

    #[quickcheck]
    fn mutated_pages_never_panic(page: Page, mutations: Vec<(u16, u8)>) -> TestResult {
        // initial input Page is valid, we deconstruct it into raw bytes and make random
        // modifications.
        let mut bytes = page.as_raw_page().unwrap();
        for &(pos, val) in &mutations {
            bytes[pos as usize % PAGE_SIZE] = val;
        }

        let result = Page::deserialize(&mut &bytes[..]);

        if mutations.is_empty() {
            assert_eq!(result.unwrap(), page);
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn accepted_pages_are_valid(page: Page, mutations: Vec<(u16, u8)>) -> TestResult {
        let mut bytes = page.as_raw_page().unwrap();
        for &(pos, val) in &mutations {
            bytes[pos as usize % PAGE_SIZE] = val;
        }
        match Page::deserialize(&mut &bytes[..]) {
            Ok(decoded) => {
                let again = decoded
                    .as_raw_page()
                    .expect("accepted page doesn't re-serialize");
                assert_eq!(
                    Page::deserialize(&mut &again[..])
                        .unwrap()
                        .as_raw_page()
                        .unwrap(),
                    decoded.as_raw_page().unwrap()
                );
                TestResult::passed()
            }
            Err(_) => TestResult::discard(),
        }
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
        let _ = page.leaf_insert(validated_row.clone()); // this might error if the key already exists, but we're guaranteed to have it in there after calling it
        let result = page.leaf_insert(validated_row); // this is the check that matters

        assert!(matches!(result, Err(PageError::DuplicateKey)), "{result:?}");
        TestResult::passed()
    }

    #[quickcheck]
    fn header_constant_is_right(page: Page) -> TestResult {
        let mut cursor = Cursor::new([0u8; PAGE_SIZE]);
        page.write_header(&mut cursor).unwrap();
        let mut none_count = 0;
        match page.body {
            PageBody::Internal { .. } => {
                assert_eq!(
                    cursor.position() + none_count * PAGE_ID_SIZE as u64,
                    MAX_INTERNAL_HEADER_SIZE as u64
                )
            }
            PageBody::Leaf { prev, next, .. } => {
                none_count += prev.is_none() as u64 + next.is_none() as u64;
                assert_eq!(
                    cursor.position() + none_count * PAGE_ID_SIZE as u64,
                    MAX_LEAF_HEADER_SIZE as u64
                )
            }
        };
        TestResult::passed()
    }

    #[test]
    fn basic_leaf_page_roundtrip() {
        let mut records: Vec<Row> = (1..=5)
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
        records.sort_by_key(|r| Key::try_from(&r.fields[0]).unwrap());
        records.dedup_by_key(|r| Key::try_from(&r.fields[0]).unwrap());

        let lpage = Page {
            page_id: PageId::new(TableId::new(1), 10),
            last_update: PageLsn(Some(Lsn::new(10).unwrap())),
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
        let Some(free_space) = page.free_space() else {
            return TestResult::discard();
        };

        let mut raw_page = Cursor::new([0u8; PAGE_SIZE]);
        page.write_header(&mut raw_page).unwrap();
        let header_end = raw_page.position() as usize;
        let (slot_offset, data_offset) = page.write_body(&mut raw_page, header_end).unwrap();

        let actual_free_space = data_offset - slot_offset;

        let max_header = match page.body {
            PageBody::Internal { .. } => MAX_INTERNAL_HEADER_SIZE,
            PageBody::Leaf { .. } => MAX_LEAF_HEADER_SIZE,
        };
        assert_eq!(free_space + (max_header - header_end), actual_free_space);
        TestResult::passed()
    }

    #[quickcheck]
    fn page_quickcheck_roundtrip(page: Page) -> TestResult {
        let mut bytes = Cursor::new(Vec::new());
        page.serialize(&mut bytes).unwrap();

        bytes.set_position(0);
        let deser_page = Page::deserialize(&mut bytes).unwrap();

        assert_eq!(
            page.as_raw_page().unwrap(),
            deser_page.as_raw_page().unwrap()
        );
        TestResult::passed()
    }
}

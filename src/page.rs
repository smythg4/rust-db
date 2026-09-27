use crate::commontypes::{
    Key, KeyError, LsnError, PAGE_ID_SIZE, PageId, PageLsn, SLOT_ENTRY_SIZE, SlotEntry,
};
use crate::page::BorrowFailReason::PointerMismatch;
use crate::schema::{Row, RowValue, RowValueError, ValidatedRow};
use crate::traits::Serializable;
use crc32_light::Crc32Stream;
use std::cmp::Ordering;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::iter::Iterator;
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

/// Smallest possible encoded key: an empty string (1 tag byte + 1 length byte).
const MIN_KEY_ENCODED_SIZE: usize = 2;
/// Smallest possible encoded row: the field count plus the smallest key.
const MIN_ROW_ENCODED_SIZE: usize = 1 + MIN_KEY_ENCODED_SIZE;
/// Leaf header with both sibling pointers `None`.
const MIN_LEAF_HEADER_SIZE: usize = MAX_LEAF_HEADER_SIZE - 2 * PAGE_ID_SIZE;

/// Upper bound on records in a leaf: every entry at its smallest possible size.
pub const MAX_LEAF_ITEMS: usize =
    (PAGE_SIZE - MIN_LEAF_HEADER_SIZE) / (SLOT_ENTRY_SIZE + MIN_ROW_ENCODED_SIZE);

/// Upper bound on keys in an internal page: every entry at its smallest possible size,
/// plus the one extra child that has no key.
pub const MAX_INTERNAL_ITEMS: usize = (PAGE_SIZE - MAX_INTERNAL_HEADER_SIZE - PAGE_ID_SIZE)
    / (SLOT_ENTRY_SIZE + PAGE_ID_SIZE + MIN_KEY_ENCODED_SIZE);

const _: () = assert!(MIN_LEAF_HEADER_SIZE + MAX_LEAF_ITEMS * SLOT_ENTRY_SIZE <= PAGE_SIZE);
const _: () = assert!(
    MAX_INTERNAL_HEADER_SIZE
        + (MAX_INTERNAL_ITEMS + 1) * PAGE_ID_SIZE
        + MAX_INTERNAL_ITEMS * SLOT_ENTRY_SIZE
        <= PAGE_SIZE
);

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
    #[error("Merge Invalid: {0}")]
    InvalidMerge(MergeFailReason),
    #[error("Borrow Invalid: {0}")]
    InvalidBorrow(BorrowFailReason),
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum BorrowFailReason {
    #[error("Attempt to borrow from empty page: {0}")]
    EmptyBorrow(PageId),
    #[error("Inserting Key outside page bounds: {0:?}")]
    KeysOutOfOrder(Key),
    #[error("Right neighbor is {0:?} but provided {1:?}")]
    PointerMismatch(Option<PageId>, Option<PageId>),
}
#[derive(Error, Debug, Clone, PartialEq)]
pub enum MergeFailReason {
    #[error("Unable to merge Leaf pages with Internal pages")]
    MismatchMerge,
    #[error("Right neighbor is {0:?} but provided {1:?}")]
    PointerMismatch(Option<PageId>, Option<PageId>),
    #[error("Keys aren't sorted or duplicate key found")]
    Keys,
}
#[derive(Debug, Clone, PartialEq)]
pub enum CorruptionKind {
    InvalidTag(u8),
    InvalidRange { slot: usize, range: Range<usize> },
    CorruptRow { slot: usize },
    CorruptKey { slot: usize },
    ChildCountMismatch { keys: usize, children: usize },
    MissingKey,
    InvalidKey(RowValue),
    UnsortedKeys { at: usize },
    ExceedsCapacity,
    CheckSumMismatch,
    InvalidPointerTag,
    OverlappingSlots { first: usize, second: usize },
    TrailingBytes { slot: usize },
    TooManyItems(usize),
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

    pub(crate) fn empty_page(page_id: PageId, body: PageBody) -> Self {
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
        let page = Page {
            page_id,
            last_update: PageLsn(None),
            body,
        };
        page.debug_check_invariants("new_root");
        page
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

    /// Returns `true` if the `Page` is underfull and should be merged with another
    pub fn is_underfull(&self) -> bool {
        match self.free_space() {
            Some(fs) => fs > PAGE_SIZE / 2,
            None => false,
        }
    }

    /// Inserts a separator `Key` and associated child `PageId` into an internal `Page`.
    /// Primarily called by parent datastructure (e.g. `BTree`) after splitting a `Page`
    /// lower in the tree.
    pub fn internal_insert(
        &mut self,
        separator: Key,
        right_child: PageId,
    ) -> Result<(), PageError> {
        let can_insert = matches!(self.free_space(), Some(free_space) if free_space >= Self::internal_entry_size(&separator));

        let PageBody::Internal { keys, children } = &mut self.body else {
            return Err(PageError::NotInternal);
        };

        let insert_pos = match keys.binary_search(&separator) {
            Ok(_) => return Err(PageError::DuplicateKey),
            Err(i) => i,
        };

        if !can_insert {
            return Err(PageError::PageFull);
        }

        keys.insert(insert_pos, separator);
        children.insert(insert_pos + 1, right_child);
        self.debug_check_invariants("internal_insert");
        Ok(())
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
        self.debug_check_invariants("leaf_insert");
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

                self.debug_check_invariants("split_internal");
                let new_page = Page {
                    page_id: new_page_id,
                    last_update: PageLsn(None),
                    body: PageBody::Internal {
                        keys: new_keys,
                        children: new_children,
                    },
                };
                new_page.debug_check_invariants("split_internal");
                Ok((split_key, new_page))
            }
            PageBody::Leaf { records, next, .. } => {
                if records.len() < 2 {
                    return Err(PageError::TooSmallToSplit(self.page_id));
                }

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

                self.debug_check_invariants("split_leaf");
                new_page.debug_check_invariants("split_leaf");
                Ok((split_key, new_page))
            }
        }
    }

    /// If the merge is legal, this will drain the right `Page` of any entries and merge them into
    /// this one. Right `Page` won't be affected in the event of failure. Returns the `PageId` of the
    /// merged `Page` that can be recycled to a free list.
    pub fn leaf_merge_from_right(&mut self, right: &mut Page) -> Result<PageId, PageError> {
        if !self
            .free_space()
            .is_some_and(|free| free >= right.entries_size())
        {
            return Err(PageError::PageFull);
        }
        let right_id = right.page_id;
        match (&mut self.body, &mut right.body) {
            (
                PageBody::Leaf { records, next, .. },
                PageBody::Leaf {
                    records: right_records,
                    next: right_next,
                    ..
                },
            ) => {
                match *next {
                    Some(id) if id == right_id => {
                        if let Some(left_max) = records.last()
                            && right_records.first().is_some_and(|right_min| {
                                let left_key = Key::try_from(&left_max.fields[0]).unwrap();
                                let right_key = Key::try_from(&right_min.fields[0]).unwrap();
                                right_key <= left_key
                            })
                        {
                            return Err(PageError::InvalidMerge(MergeFailReason::Keys));
                        }
                        // leaf merge: append right_records, take over right_next
                        let right_records = right_records.drain(..);
                        records.extend(right_records);
                        *next = *right_next;
                        self.debug_check_invariants("leaf_merge_from_right");
                        // no need to check the right page since it's drained and now ready for re-issue
                        Ok(right_id)
                    }
                    _ => Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch(
                        *next,
                        Some(right_id),
                    ))),
                }
            }
            _ => Err(PageError::InvalidMerge(MergeFailReason::MismatchMerge)),
        }
    }

    /// If the merge is legal, this will drain the right `Page` of any entries and merge them into
    /// this one. Right `Page` won't be affected in the event of failure. `separator` will be installed
    /// at the right point in the newly merged `Page`. Returns the `PageId` of the
    /// merged `Page` that can be recycled to a free list.
    pub fn internal_merge_from_right(
        &mut self,
        right: &mut Page,
        separator: Key,
    ) -> Result<PageId, PageError> {
        if !self.free_space().is_some_and(|free| {
            free >= right.entries_size() + Self::internal_entry_size(&separator)
        }) {
            return Err(PageError::PageFull);
        }
        let right_id = right.page_id;
        match (&mut self.body, &mut right.body) {
            (
                PageBody::Internal { keys, children },
                PageBody::Internal {
                    keys: right_keys,
                    children: right_children,
                },
            ) => {
                // reject if there's a key on the right and it's less than the separator
                // assumes keys list is sorted
                if right_keys.first().is_some_and(|k| k <= &separator) {
                    return Err(PageError::InvalidMerge(MergeFailReason::Keys));
                }
                // reject if there's a key on the left and it's greater than the separator
                // assumes keys list is sorted
                if keys.last().is_some_and(|k| k >= &separator) {
                    return Err(PageError::InvalidMerge(MergeFailReason::Keys));
                }
                let right_keys = right_keys.drain(..);
                let right_children = right_children.drain(..);

                // push the key in between the two lists
                keys.push(separator);
                // add the right side keys
                keys.extend(right_keys);
                // add the right side children
                children.extend(right_children);
                self.debug_check_invariants("internal_merge_from_right");
                // don't check invariants on the right page since it's drained.
                Ok(right_id)
            }
            _ => Err(PageError::InvalidMerge(MergeFailReason::MismatchMerge)),
        }
    }

    /// Accepts a row from the right leaf neighbor. Returns the promoted Key to replace in the parent on success
    pub fn leaf_borrow_from_right(&mut self, right_page: &mut Page) -> Result<Key, PageError> {
        if matches!(self.body, PageBody::Internal { .. })
            || matches!(right_page.body, PageBody::Internal { .. })
        {
            return Err(PageError::NotLeaf);
        }

        // make sure the right page is actually THIS page's right page
        let right_page_id = right_page.page_id;
        match self.next()? {
            Some(np) if np == right_page_id => {}
            other => {
                return Err(PageError::InvalidBorrow(PointerMismatch(
                    other,
                    Some(right_page_id),
                )));
            }
        };

        // make sure there's a record to borrow from the right page.
        let Some(borrow_rec) = right_page.records().unwrap().next() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                right_page.page_id,
            )));
        };

        let borrowed_key =
            Key::try_from(&borrow_rec.fields[0]).expect("stored row has a valid key");

        // make sure this key is greater than all this page's current keys
        if self
            .records()
            .unwrap()
            .last()
            .is_some_and(|r| r.cmp_key(&borrowed_key) != Ordering::Less)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                borrowed_key.clone(),
            )));
        }

        // need some spare rows to leave the right neighbor alive and parent pointers valid
        if right_page.records().is_some_and(|recs| recs.count() < 2) {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                right_page.page_id,
            )));
        }

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::leaf_entry_size(borrow_rec))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the record!
        let PageBody::Leaf { records, .. } = &mut right_page.body else {
            unreachable!()
        };
        let stolen_record = records.remove(0);

        // we checked that there were at least two records in the right_page
        let promoted_key = Key::try_from(&records.first().unwrap().fields[0]).unwrap();

        let PageBody::Leaf { records, .. } = &mut self.body else {
            unreachable!()
        };
        records.push(stolen_record);

        self.debug_check_invariants("leaf_borrow_from_right");
        right_page.debug_check_invariants("leaf_borrow_from_right");

        Ok(promoted_key)
    }

    /// Accepts a row from the left leaf neighbor. Returns the promoted Key to replace in the parent on success
    pub fn leaf_borrow_from_left(&mut self, left_page: &mut Page) -> Result<Key, PageError> {
        if matches!(self.body, PageBody::Internal { .. })
            || matches!(left_page.body, PageBody::Internal { .. })
        {
            return Err(PageError::NotLeaf);
        }

        // make sure the left page is actually THIS page's left page
        let left_page_id = left_page.page_id;
        match self.prev()? {
            Some(pp) if pp == left_page_id => {}
            other => {
                return Err(PageError::InvalidBorrow(PointerMismatch(
                    Some(left_page_id),
                    other,
                )));
            }
        };

        // make sure there's a record to borrow from the left page.
        let Some(borrow_rec) = left_page.records().unwrap().last() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                left_page.page_id,
            )));
        };

        let borrowed_key =
            Key::try_from(&borrow_rec.fields[0]).expect("stored row has a valid key");

        // make sure this key is less than all this page's current keys
        if self
            .records()
            .unwrap()
            .next()
            .is_some_and(|r| r.cmp_key(&borrowed_key) != Ordering::Greater)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                borrowed_key.clone(),
            )));
        }

        // need some spare rows to leave the left neighbor alive and parent pointers valid
        if left_page.records().is_some_and(|recs| recs.count() < 2) {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                left_page.page_id,
            )));
        }

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::leaf_entry_size(borrow_rec))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the record!
        let PageBody::Leaf { records, .. } = &mut left_page.body else {
            unreachable!()
        };
        let stolen_record = records.pop().expect("left page has entries");

        // in this case we use the borrowed_key as the promoted separator key
        let promoted_key = borrowed_key.clone();

        let PageBody::Leaf { records, .. } = &mut self.body else {
            unreachable!()
        };
        records.insert(0, stolen_record);

        self.debug_check_invariants("leaf_borrow_from_left");
        left_page.debug_check_invariants("leaf_borrow_from_left");

        Ok(promoted_key)
    }

    /// Accepts a right_page and parent separator key. Will steal a child from the right page and insert that with
    /// the parent_sep into the `Page` (parent_sep, stolen_child). Returns new separator (first `Key` from right_page)
    /// on success. If the separator invariants aren't upheld, `PageError::InvalidBorrow(KeysOutOfOrder(parent_sep))` will
    /// return the value back to the caller, but will drop it in all other failure modes.
    pub fn internal_borrow_from_right(
        &mut self,
        right_page: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        if matches!(self.body, PageBody::Leaf { .. })
            || matches!(right_page.body, PageBody::Leaf { .. })
        {
            return Err(PageError::NotInternal);
        }

        // Note: No way to ensure these are actually siblings apart from the separator check below.

        // make sure there's a key to borrow from the right page.
        let Some(borrow_key) = right_page.keys().unwrap().next() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                right_page.page_id,
            )));
        };

        // make sure the separator is valid (all keys in this page are less than, borrow key is greater)
        if self
            .keys()
            .unwrap()
            .last()
            .is_some_and(|r| r >= &parent_sep)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        if borrow_key <= &parent_sep {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        // no follow up right_page size check required since worst case right_page will end up with 0 keys and 1 child

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::internal_entry_size(&parent_sep))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the key/child pair!
        let PageBody::Internal { keys, children, .. } = &mut right_page.body else {
            unreachable!()
        };
        let stolen_key = keys.remove(0);
        let stolen_child = children.remove(0);

        // promoted key is the key we stole
        let promoted_key = stolen_key;

        let PageBody::Internal { keys, children, .. } = &mut self.body else {
            unreachable!()
        };
        // add the parent_sep to the keys list
        keys.push(parent_sep);
        // add the stolen child to the children list
        children.push(stolen_child);

        self.debug_check_invariants("internal_borrow_from_right");
        right_page.debug_check_invariants("internal_borrow_from_right");

        Ok(promoted_key)
    }

    /// Accepts a left and parent separator key. Will steal a child from the left page and insert that with
    /// the parent_sep into the `Page` (parent_sep, stolen_child). Returns new separator (last `Key` from left_page)
    /// on success. If the separator invariants aren't upheld, `PageError::InvalidBorrow(KeysOutOfOrder(parent_sep))` will
    /// return the value back to the caller, but will drop it in all other failure modes.
    pub fn internal_borrow_from_left(
        &mut self,
        left_page: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        if matches!(self.body, PageBody::Leaf { .. })
            || matches!(left_page.body, PageBody::Leaf { .. })
        {
            return Err(PageError::NotInternal);
        }

        // Note: No way to ensure these are actually siblings apart from the separator check below.

        // make sure there's a key to borrow from the left page.
        let Some(borrow_key) = left_page.keys().unwrap().last() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                left_page.page_id,
            )));
        };

        // make sure the separator is valid (all keys in this page are greater than, borrow key is less than)
        if self
            .keys()
            .unwrap()
            .next()
            .is_some_and(|r| r <= &parent_sep)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        if borrow_key >= &parent_sep {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        // no follow up left_page size check required since worst case left_page will end up with 0 keys and 1 child

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::internal_entry_size(&parent_sep))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the key/child pair!
        let PageBody::Internal { keys, children, .. } = &mut left_page.body else {
            unreachable!()
        };
        let stolen_key = keys.pop().expect("left_page has a key");
        let stolen_child = children.pop().expect("left_page has a child");

        // promoted key is the key we stole
        let promoted_key = stolen_key;

        let PageBody::Internal { keys, children, .. } = &mut self.body else {
            unreachable!()
        };
        // add the parent_sep to the keys list
        keys.insert(0, parent_sep);
        // add the stolen child to the children list
        children.insert(0, stolen_child);

        self.debug_check_invariants("internal_borrow_from_left");
        left_page.debug_check_invariants("internal_borrow_from_left");

        Ok(promoted_key)
    }

    fn entries_size(&self) -> usize {
        match &self.body {
            PageBody::Internal { keys, .. } => {
                keys.iter().map(Self::internal_entry_size).sum::<usize>()
            }
            PageBody::Leaf { records, .. } => {
                records.iter().map(Self::leaf_entry_size).sum::<usize>()
            }
        }
    }

    // Note: changed these helps to return iterators.
    // Reasoning: these can be a part of the `Page` trait and when I shift to raw byte pages
    // these methods can return a `RecordIter` for example that lazily walk the slots in the page.
    pub fn records(&self) -> Option<impl Iterator<Item = &Row>> {
        match &self.body {
            PageBody::Leaf { records, .. } => Some(records.iter()),
            _ => None,
        }
    }

    pub fn keys(&self) -> Option<impl Iterator<Item = &Key>> {
        match &self.body {
            PageBody::Internal { keys, .. } => Some(keys.iter()),
            _ => None,
        }
    }

    pub fn children(&self) -> Option<impl Iterator<Item = &PageId>> {
        match &self.body {
            PageBody::Internal { children, .. } => Some(children.iter()),
            _ => None,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn leaf_remove(&mut self, key: &Key) -> Result<Option<Row>, PageError> {
        let PageBody::Leaf { records, .. } = &mut self.body else {
            return Err(PageError::NotLeaf);
        };
        let removed = match records.binary_search_by(|r| r.cmp_key(key)) {
            Ok(i) => Some(records.remove(i)),
            Err(_) => None,
        };
        self.debug_check_invariants("leaf_remove");
        Ok(removed)
    }

    #[allow(dead_code)]
    pub(crate) fn internal_remove(
        &mut self,
        key: &Key,
    ) -> Result<Option<(Key, PageId)>, PageError> {
        // check before remove operation to make sure there's a child at `i + 1`
        self.debug_check_invariants("internal_remove");

        let PageBody::Internal { keys, children } = &mut self.body else {
            return Err(PageError::NotInternal);
        };
        let removed = match keys.binary_search(key) {
            Ok(i) => Some((keys.remove(i), children.remove(i + 1))),
            Err(_) => None,
        };
        self.debug_check_invariants("internal_remove");
        Ok(removed)
    }

    #[allow(dead_code)]
    pub(crate) fn next(&self) -> Result<Option<PageId>, PageError> {
        match self.body {
            PageBody::Leaf { next, .. } => Ok(next),
            _ => Err(PageError::NotLeaf),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_next(&mut self, new: Option<PageId>) -> Result<(), PageError> {
        match &mut self.body {
            PageBody::Leaf { next, .. } => {
                *next = new;
                Ok(())
            }
            _ => Err(PageError::NotLeaf),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn prev(&self) -> Result<Option<PageId>, PageError> {
        match self.body {
            PageBody::Leaf { prev, .. } => Ok(prev),
            _ => Err(PageError::NotInternal),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_prev(&mut self, new: Option<PageId>) -> Result<(), PageError> {
        match &mut self.body {
            PageBody::Leaf { prev, .. } => {
                *prev = new;
                Ok(())
            }
            _ => Err(PageError::NotInternal),
        }
    }

    /// Reads `count` slot entries from `cursor` (positioned at the start of the slot array)
    /// and checks the byte ranges they point to:
    ///
    /// - every range is non-empty and lies inside the data area: after the end of the slot
    ///   array and within the page
    /// - no two ranges overlap
    ///
    /// Returns the ranges in slot order.
    fn read_slot_ranges(
        cursor: &mut Cursor<&[u8]>,
        count: usize,
    ) -> Result<Vec<Range<usize>>, CorruptionKind> {
        let mut ranges = Vec::with_capacity(count);
        for _ in 0..count {
            let entry =
                SlotEntry::deserialize(cursor).expect("slot array fits: num_items is bounded");
            ranges.push(entry.range());
        }

        // the data area starts where the slot array ends
        let data_start = cursor.position() as usize;
        if let Some((i, bad)) = ranges
            .iter()
            .enumerate()
            .find(|(_, r)| r.is_empty() || r.start < data_start || r.end > PAGE_SIZE)
        {
            return Err(CorruptionKind::InvalidRange {
                slot: i,
                range: bad.clone(),
            });
        }

        // sort by start offset. each range must end before the next one begins
        let mut by_start: Vec<(usize, &Range<usize>)> = ranges.iter().enumerate().collect();
        by_start.sort_by_key(|(_, r)| r.start);
        if let Some(w) = by_start.windows(2).find(|w| w[1].1.start < w[0].1.end) {
            return Err(CorruptionKind::OverlappingSlots {
                first: w[0].0,
                second: w[1].0,
            });
        }

        Ok(ranges)
    }
    /// Decodes one slot's bytes, requiring the value to use exactly all of them.
    fn decode_slot<T: Serializable>(
        bytes: &[u8],
        slot: usize,
        decode_error: impl Fn(usize) -> CorruptionKind,
    ) -> Result<T, CorruptionKind> {
        let mut remaining = bytes;
        let value = T::deserialize(&mut remaining).map_err(|_| decode_error(slot))?;
        if !remaining.is_empty() {
            return Err(CorruptionKind::TrailingBytes { slot });
        }
        Ok(value)
    }

    /// Checks every structural rule a `Page` must satisfy, whether it came from disk or from
    /// an operation in this module:
    ///
    /// - leaf: every row's first field is a valid key, and keys are strictly increasing
    /// - internal: `children.len() == keys.len() + 1`, and keys are strictly increasing
    /// - the page fits in `PAGE_SIZE` bytes
    ///
    /// Returns the first violation found. `deserialize` maps it to `PageError::Corrupt`;
    /// page operations call `debug_check_invariants` to catch bugs in this module.
    pub(crate) fn check_invariants(&self) -> Result<(), CorruptionKind> {
        match &self.body {
            PageBody::Leaf { records, .. } => {
                let mut prev: Option<Key> = None;
                for (i, record) in records.iter().enumerate() {
                    let first = record.fields.first().ok_or(CorruptionKind::MissingKey)?;
                    let key = Key::try_from(first)
                        .map_err(|_| CorruptionKind::InvalidKey(first.clone()))?;
                    if prev.as_ref().is_some_and(|p| p >= &key) {
                        return Err(CorruptionKind::UnsortedKeys { at: i });
                    }
                    prev = Some(key);
                }
            }
            PageBody::Internal { keys, children } => {
                if children.len() != keys.len() + 1 {
                    return Err(CorruptionKind::ChildCountMismatch {
                        keys: keys.len(),
                        children: children.len(),
                    });
                }
                if let Some(i) = keys.windows(2).position(|w| w[0] >= w[1]) {
                    return Err(CorruptionKind::UnsortedKeys { at: i + 1 });
                }
            }
        }
        if self.free_space().is_none() {
            return Err(CorruptionKind::ExceedsCapacity);
        }
        Ok(())
    }

    /// Panics (debug builds only) if `check_invariants` fails. Call at the end of every
    /// operation that changes a page; `op` names the operation in the panic message.
    #[inline]
    pub(crate) fn debug_check_invariants(&self, op: &str) {
        #[cfg(debug_assertions)]
        if let Err(kind) = self.check_invariants() {
            panic!("page {} violates {kind:?} after {op}", self.page_id);
        }
        #[cfg(not(debug_assertions))]
        let _ = op;
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

        // helper closure for error mapping
        let corrupt = |kind| PageError::Corrupt {
            page_id: Some(page_id),
            kind,
        };

        // skip the crc32 portion
        cursor.set_position(CHECKSUM_OFFSET as u64 + 4);

        let (next_page, prev_page) = if is_leaf {
            let np = Option::<PageId>::deserialize(&mut cursor)
                .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
            let pp = Option::<PageId>::deserialize(&mut cursor)
                .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
            (np, pp)
        } else {
            (None, None)
        };

        let mut buf_two = [0u8; 2];
        cursor.read_exact(&mut buf_two)?;
        let num_items = u16::from_be_bytes(buf_two) as usize;

        let max_items = if is_leaf {
            MAX_LEAF_ITEMS
        } else {
            MAX_INTERNAL_ITEMS
        };
        if num_items > max_items {
            return Err(corrupt(CorruptionKind::TooManyItems(num_items)));
        }

        let body = if is_leaf {
            let ranges = Self::read_slot_ranges(&mut cursor, num_items).map_err(corrupt)?;
            let records = ranges
                .into_iter()
                .enumerate()
                .map(|(slot, range)| {
                    Self::decode_slot(&buf[range], slot, |slot| CorruptionKind::CorruptRow {
                        slot,
                    })
                    .map_err(corrupt)
                })
                .collect::<Result<Vec<Row>, PageError>>()?;

            PageBody::Leaf {
                next: next_page,
                prev: prev_page,
                records,
            }
        } else {
            let mut children = Vec::with_capacity(num_items + 1);
            for _ in 0..num_items + 1 {
                children.push(
                    PageId::deserialize(&mut cursor)
                        .expect("slot array fits: num_items is bounded"),
                );
            }
            let ranges = Self::read_slot_ranges(&mut cursor, num_items).map_err(corrupt)?;
            let keys = ranges
                .into_iter()
                .enumerate()
                .map(|(slot, range)| {
                    Self::decode_slot(&buf[range], slot, |slot| CorruptionKind::CorruptKey {
                        slot,
                    })
                    .map_err(corrupt)
                })
                .collect::<Result<Vec<Key>, PageError>>()?;
            PageBody::Internal { keys, children }
        };

        let page = Page {
            page_id,
            last_update,
            body,
        };

        // ensure all invariants are upheld
        page.check_invariants().map_err(corrupt)?;

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
    use crate::schema::RowValue;
    use crate::test_support::*;

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
    struct LeafPage(Page);

    #[derive(Debug, Clone)]
    struct InternalPage(Page);

    impl Arbitrary for LeafPage {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut page = Page::arbitrary(g);
            while page.records().is_none() {
                page = Page::arbitrary(g);
            }
            LeafPage(page)
        }
    }

    impl Arbitrary for InternalPage {
        fn arbitrary(g: &mut Gen) -> Self {
            let mut page = Page::arbitrary(g);
            while page.children().is_none() {
                page = Page::arbitrary(g);
            }
            InternalPage(page)
        }
    }

    #[quickcheck]
    fn split_then_merge_roundtrip(mut page: Page) -> TestResult {
        let snapshot = page.clone();
        let new_id = page.page_id.wrapping_add(1);

        let Some((separator, mut new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let freed_id = match &page.body {
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut new_page, separator),
            PageBody::Leaf { .. } => page.leaf_merge_from_right(&mut new_page),
        }
        .unwrap();

        // make sure the PageId returned is the one we put in as the right page
        assert_eq!(freed_id, new_id);

        // make sure the page is identical to where we started after the roundtrip
        assert_unchanged(&page, &snapshot);

        // make sure the "freed" page was cleared of any entries
        assert_eq!(new_page.entries_size(), 0);

        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_with_invalid_pointers(LeafPage(mut page): LeafPage) -> TestResult {
        // if it's under full, we know it can fit into itself. We only want leaf pages for this test
        if !page.is_underfull() {
            return TestResult::discard();
        }

        // generate an id that's different from the given page's
        let page_id = page.page_id.wrapping_add(1);

        let Some((_, mut new_page)) = try_split(&mut page, page_id) else {
            return TestResult::discard();
        };

        let snapshot2 = new_page.clone();

        // now we have a new page that we're positive we can merge! Let's mess with it.
        page.set_next(None).unwrap();
        let snapshot1 = page.clone();
        let result = page.leaf_merge_from_right(&mut new_page);

        assert_matches!(
            result,
            Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch(
                None,
                _
            )))
        );
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&new_page, &snapshot2);

        // trying again but with a `Some` value
        page.set_next(Some(page.page_id)).unwrap();
        let snapshot1 = page.clone();
        let result = page.leaf_merge_from_right(&mut new_page);

        assert_matches!(
            result,
            Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch(
                Some(_),
                Some(_)
            )))
        );
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&new_page, &snapshot2);

        // trying again with the right pointer value
        page.set_next(Some(page_id)).unwrap();

        let result = page
            .leaf_merge_from_right(&mut new_page)
            .expect("it should work this time");

        assert_eq!(result, page_id, "freed PageId doesn't match");

        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_with_duplicate_keys(mut page: Page) -> TestResult {
        // if it's under full, we know it can fit into itself
        if !page.is_underfull() {
            return TestResult::discard();
        }

        let mut page2 = page.clone();
        // make up a key to serve as the separator
        let dummy_key = Key::Integer(0);

        let mut snapshot1 = page.clone();
        let snapshot2 = page2.clone();

        let result = match &mut page.body {
            PageBody::Leaf { next, .. } => {
                // tidy up the next pointer so we know that's not causing the error
                *next = Some(page2.page_id);
                // update the snapshot
                snapshot1 = page.clone();

                page.leaf_merge_from_right(&mut page2)
            }
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut page2, dummy_key),
        };

        assert_matches!(result, Err(PageError::InvalidMerge(MergeFailReason::Keys)));
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&page2, &snapshot2);
        TestResult::passed()
    }

    #[quickcheck]
    fn internal_merge_counts_the_separator_size(key_sizes: Vec<u16>) -> TestResult {
        if key_sizes.is_empty() {
            return TestResult::discard();
        }

        let left = fill_internal(child(1), key_sizes);
        let free = left.free_space().unwrap();

        // build the right page as empty as possible
        let right = internal_with_one_child(999);

        // separator_with generates a valid separator based on the left pages last entry. Discard results where
        // left page is empty
        let keys = left.keys().unwrap();

        let Some(Key::String(last)) = keys.last() else {
            return TestResult::discard();
        };
        let separator_with = |extra: usize| Key::String(format!("{last}{}", "z".repeat(extra)));

        // find the smallest key that won't fit
        let too_big = (1..)
            .find(|&n| Page::internal_entry_size(&separator_with(n)) > free)
            .unwrap();

        // make sure that the too big key results in a full page
        let (mut l, mut r) = (left.clone(), right.clone());
        let result = l.internal_merge_from_right(&mut r, separator_with(too_big));
        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&l, &left);
        assert_unchanged(&r, &right);

        // try a key that's one byte smaller and confirm that it fits
        if too_big > 1 {
            let (mut l, mut r) = (left.clone(), right.clone());
            let result = l.internal_merge_from_right(&mut r, separator_with(too_big - 1));
            assert!(
                result.is_ok(),
                "separator that fits was rejected: {result:?}"
            );
            assert!(l.as_raw_page().is_ok(), "merged page doesn't serialize");
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_when_not_small_enough(mut page: Page) -> TestResult {
        // make sure we a page whose data size is less than the free space available
        if page.entries_size() < page.free_space().unwrap_or(usize::MAX) {
            return TestResult::discard();
        }
        // clone that page, so we know that these can't safely merge
        let mut page2 = page.clone();

        // make up a key to serve as the separator
        let dummy_key = Key::Integer(0);

        let mut snapshot1 = page.clone();
        let snapshot2 = page2.clone();

        // this example will have duplicate keys and failed separator checks, but
        // the overfull check should occur first

        let result = match &mut page.body {
            PageBody::Leaf { next, .. } => {
                // make sure this page points to the correct way
                *next = Some(page2.page_id);
                // update the snapshot
                snapshot1 = page.clone();
                page.leaf_merge_from_right(&mut page2)
            }
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut page2, dummy_key),
        };

        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&page2, &snapshot2);
        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_with_dissimilar_pages(mut page1: Page, mut page2: Page) -> TestResult {
        // make sure both pages are underfull
        if !page1.is_underfull() || !page2.is_underfull() {
            return TestResult::discard();
        }
        let dummy_key = Key::String("dummy".into());
        let snapshot1 = page1.clone();
        let snapshot2 = page2.clone();
        // make sure the pages are of different types
        match (&mut page1.body, &mut page2.body) {
            (PageBody::Leaf { .. }, PageBody::Internal { .. }) => {
                let result = page1.leaf_merge_from_right(&mut page2);

                assert_matches!(
                    result,
                    Err(PageError::InvalidMerge(MergeFailReason::MismatchMerge))
                );
                assert_unchanged(&page1, &snapshot1);
                assert_unchanged(&page2, &snapshot2);
                TestResult::passed()
            }
            (PageBody::Internal { .. }, PageBody::Leaf { .. }) => {
                let result = page1.internal_merge_from_right(&mut page2, dummy_key);
                assert_matches!(
                    result,
                    Err(PageError::InvalidMerge(MergeFailReason::MismatchMerge))
                );
                assert_unchanged(&page1, &snapshot1);
                assert_unchanged(&page2, &snapshot2);
                TestResult::passed()
            }
            _ => TestResult::discard(),
        }
    }

    #[quickcheck]
    fn max_size_row_fits_after_leaf_split(payload_sizes: Vec<u16>, target: u8) -> TestResult {
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

        let mut page = fill_leaf(child(1), payload_sizes);
        let count = page.records().unwrap().count() as i64;
        let new_id = page.page_id.wrapping_add(1);

        // split the page! (the new one will have a duplicate page id, but that's fine for the test)
        let (separator, mut right) = page.split_page(new_id).unwrap();

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
        if key_sizes.is_empty() {
            return TestResult::discard();
        }

        // largest string key whose internal entry still fits the limit
        let max_key_len = (0..MAX_INTERNAL_ENTRY_SIZE)
            .rev()
            .find(|&len| Page::internal_entry_size(&padded_key(0, len)) <= MAX_INTERNAL_ENTRY_SIZE)
            .unwrap();

        let mut page = fill_internal(child(1), key_sizes);
        let key_count = page.num_items();
        let new_id = page.page_id.wrapping_add(1);

        let Some((separator, mut right)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let big_key = padded_key((target as usize % (key_count + 1)) * 2 + 1, max_key_len);
        let new_child = child(1_000_000);
        let half = if big_key >= separator {
            &mut right
        } else {
            &mut page
        };

        let result = half.internal_insert(big_key.clone(), new_child);
        assert!(
            result.is_ok(),
            "max-size key didn't fit after split: {result:?}"
        );

        // the new child sits immediately right of the new key
        assert_eq!(half.body.find_child(&big_key), Some(new_child));

        TestResult::passed()
    }

    #[quickcheck]
    fn insertion_order_on_internals(mut keys: Vec<Key>) -> TestResult {
        use std::collections::BTreeMap;

        if keys.is_empty() {
            return TestResult::discard();
        }

        let first = keys.remove(0);

        let leftmost = child(2);
        let mut expected = BTreeMap::new();
        expected.insert(first.clone(), child(3));
        let mut page = Page::new_root(child(1), leftmost, first, child(3));

        for (i, key) in keys.into_iter().enumerate() {
            let right_child = child(4 + i as u32);
            let is_duplicate = expected.contains_key(&key);
            let is_full = Page::internal_entry_size(&key) > page.free_space().unwrap();
            let before = page.clone();

            let result = page.internal_insert(key.clone(), right_child);

            if is_duplicate {
                assert_matches!(result, Err(PageError::DuplicateKey));
                assert_unchanged(&before, &page);
            } else if is_full {
                assert_matches!(result, Err(PageError::PageFull));
                assert_unchanged(&before, &page);
            } else {
                assert!(result.is_ok(), "{result:?}");
                expected.insert(key.clone(), right_child);
                // routing the new key must land on the child inserted with it
                assert_eq!(page.body.find_child(&key), Some(right_child));
            }
        }

        let expected_keys: Vec<Key> = expected.keys().cloned().collect();
        assert_eq!(
            page.keys().unwrap().cloned().collect::<Vec<_>>(),
            expected_keys
        );
        // children: the untouched leftmost child, then each key's right child in key order
        let expected_children: Vec<PageId> = std::iter::once(leftmost)
            .chain(expected.values().copied())
            .collect();
        assert_eq!(
            page.children().unwrap().cloned().collect::<Vec<_>>(),
            expected_children
        );

        TestResult::passed()
    }

    #[quickcheck]
    fn insertion_order_on_leaves(SchemaWithRows(schema, rows): SchemaWithRows) -> TestResult {
        let mut page = Page::empty_leaf(child(1));
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
                assert_matches!(result, Err(PageError::DuplicateKey));
                assert_unchanged(&page_clone, &page);
            } else if is_full {
                assert_matches!(result, Err(PageError::PageFull));
                assert_unchanged(&page_clone, &page);
            } else {
                assert!(result.is_ok());
                expected.insert(key, row.clone());
            }
        }
        let actual_records: Vec<Row> = page.records().unwrap().cloned().collect();
        let expected_records: Vec<Row> = expected.into_values().collect();
        assert_eq!(expected_records, actual_records);
        TestResult::passed()
    }

    #[quickcheck]
    fn split_page_roundtrip(mut page: Page) -> TestResult {
        let new_id = page.page_id.wrapping_add(1);
        let Some((_, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        assert_roundtrip(page);
        assert_roundtrip(new_page);
        TestResult::passed()
    }

    #[quickcheck]
    fn split_page_key_in_right_spot(mut page: Page) -> TestResult {
        let new_id = page.page_id.wrapping_add(1);
        let Some((split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };
        match page.body {
            PageBody::Internal { keys, .. } => {
                assert!(!keys.is_empty());
                assert!(keys.iter().all(|k| k < &split_key));
                let new_keys: Vec<Key> = new_page.keys().unwrap().cloned().collect();
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
                let new_records: Vec<Row> = new_page.records().unwrap().cloned().collect();
                assert!(!new_records.is_empty());
                assert!(
                    new_records
                        .iter()
                        .all(|r| r.cmp_key(&split_key) != Ordering::Less)
                );
            }
        };
        TestResult::passed()
    }

    #[quickcheck]
    fn sibling_pointers_correct_after_split(LeafPage(mut page): LeafPage) -> TestResult {
        let original_id = page.page_id;
        let old_next = page.next().unwrap();
        let old_prev = page.prev().unwrap();
        let new_id = page.page_id.wrapping_add(1);
        let Some((_split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };
        match page.body {
            PageBody::Internal { .. } => unreachable!(),
            PageBody::Leaf { next, prev, .. } => {
                assert_eq!(prev, old_prev, "original page prev pointer wasn't retained");
                assert_eq!(
                    next,
                    Some(new_id),
                    "original page doesn't point to new page"
                );
                let new_prev = new_page.prev().unwrap();
                let new_next = new_page.next().unwrap();
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
    }

    #[quickcheck]
    fn no_rows_lost_in_split(mut page: Page) -> TestResult {
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
        let new_id = page.page_id.wrapping_add(1);

        let Some((split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };
        match new_page.body {
            PageBody::Internal {
                keys: new_keys,
                children: new_children,
            } => {
                let old_keys: Vec<Key> = page.keys().unwrap().cloned().collect();
                let old_children: Vec<PageId> = page.children().unwrap().cloned().collect();
                let combined_keys: Vec<Key> =
                    old_keys.into_iter().chain(new_keys.into_iter()).collect();
                let combined_children: Vec<PageId> = old_children
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
        assert_matches!(result, Err(PageError::PageFull));

        // make sure the failed insert didn't change the underlying page
        assert_unchanged(&page, &snapshot);
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
        // reset the checksum so that the checksum trigger doesn't catch the error
        let crc = Page::page_checksum(&bytes);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());

        let result = Page::deserialize(&mut &bytes[..]);

        if let Err(e) = &result {
            assert_matches!(e, PageError::Corrupt { .. });
        }

        if mutations.is_empty() {
            assert_unchanged(&result.unwrap(), &page);
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
                assert_unchanged(&Page::deserialize(&mut &again[..]).unwrap(), &decoded);
                TestResult::passed()
            }
            Err(_) => TestResult::discard(),
        }
    }

    #[quickcheck]
    fn duplicate_keys_trigger_error_on_insert(
        LeafPage(mut page): LeafPage,
        SchemaRowPair(schema, row): SchemaRowPair,
    ) -> TestResult {
        if !page.can_insert(&row) {
            return TestResult::discard();
        }
        let validated_row = schema.validate_row(row).unwrap();
        let _ = page.leaf_insert(validated_row.clone()); // this might error if the key already exists, but we're guaranteed to have it in there after calling it
        let result = page.leaf_insert(validated_row); // this is the check that matters

        assert_matches!(result, Err(PageError::DuplicateKey));
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

        assert_unchanged(&deser, &lpage);
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
        assert_roundtrip(ipage);
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
        assert_roundtrip(page);
        TestResult::passed()
    }

    /// Linear scan to find appropriate child entry. First index where separator <= key
    fn expected_child(page: &Page, key: &Key) -> PageId {
        let keys: Vec<_> = page.keys().unwrap().cloned().collect();
        let idx = keys.iter().filter(|k| *k <= key).count();
        page.children().unwrap().nth(idx).copied().unwrap()
    }

    #[quickcheck]
    fn find_child_matches_linear_scan(InternalPage(page): InternalPage, key: Key) -> TestResult {
        assert_eq!(
            page.body.find_child(&key),
            Some(expected_child(&page, &key))
        );
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_on_leaf_is_none(LeafPage(page): LeafPage, key: Key) -> TestResult {
        assert_eq!(page.body.find_child(&key), None);
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_boundaries(InternalPage(page): InternalPage) -> TestResult {
        let keys: Vec<Key> = page.keys().unwrap().cloned().collect();
        let children: Vec<PageId> = page.children().unwrap().cloned().collect();

        // check that we have at least one key
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return TestResult::discard();
        };

        // below every separator -> leftmost child
        // (Integer(i64::MIN) is the smallest possible Key; skip if it's already a separator)
        let below = Key::Integer(i64::MIN);
        if &below < first {
            assert_eq!(page.body.find_child(&below), Some(children[0]), "below all");
        }

        // exactly equal to a separator -> the child to its right
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(
                page.body.find_child(k),
                Some(children[i + 1]),
                "equal to keys[{i}]"
            );
        }

        // one below an integer separator -> the child to its left
        for (i, k) in keys.iter().enumerate() {
            if let Key::Integer(n) = k
                && let Some(m) = n.checked_sub(1)
            {
                assert_eq!(
                    page.body.find_child(&Key::Integer(m)),
                    Some(children[i]),
                    "just below keys[{i}]"
                );
            }
        }

        // above every separator -> rightmost child
        // (any String sorts after every Integer; a longer string sorts after its prefix)
        let above = higher_key(last);
        assert_eq!(
            page.body.find_child(&above),
            children.last().copied(),
            "above all"
        );

        TestResult::passed()
    }

    #[quickcheck]
    fn remove_then_insert_leaf_stays_same(LeafPage(mut page): LeafPage, target: u8) -> TestResult {
        if page.records().unwrap().next().is_none() {
            return TestResult::discard();
        }
        let snapshot = page.clone();
        let target = target as usize % page.records().unwrap().count();
        let target_row = page.records().unwrap().nth(target).cloned().unwrap();
        let target_key = Key::try_from(&target_row.fields[0]).unwrap();

        // remove the target key
        let returned_row = page
            .leaf_remove(&target_key)
            .unwrap()
            .expect("key is on the page");

        // make sure what came out is what we expected
        assert_eq!(target_row, returned_row);

        // insert back what was returned
        page.leaf_insert(ValidatedRow::from_row(returned_row))
            .unwrap();

        // make sure nothing changed
        assert_unchanged(&snapshot, &page);

        // remove it again
        let returned_row = page
            .leaf_remove(&target_key)
            .unwrap()
            .expect("key is on the page");

        // make sure what came out is what we expected
        assert_eq!(target_row, returned_row);

        // insert back what was returned - again
        page.leaf_insert(ValidatedRow::from_row(returned_row))
            .unwrap();

        // make sure nothing changed
        assert_unchanged(&snapshot, &page);
        TestResult::passed()
    }

    #[quickcheck]
    fn remove_then_insert_internal_stays_same(
        InternalPage(mut page): InternalPage,
        target: u8,
    ) -> TestResult {
        if page.keys().unwrap().next().is_none() || page.children().unwrap().next().is_none() {
            return TestResult::discard();
        }
        let snapshot = page.clone();

        let target = target as usize % page.keys().unwrap().count();
        let target_key = page.keys().unwrap().nth(target).cloned().unwrap();
        let target_child = page.children().unwrap().nth(target + 1).cloned().unwrap();

        // try to remove the key
        let (returned_key, returned_child) = page
            .internal_remove(&target_key)
            .unwrap()
            .expect("key is on the page");
        // make sure the returned values match
        assert_eq!(target_key, returned_key);
        assert_eq!(target_child, returned_child);

        // insert what was returned
        page.internal_insert(returned_key, returned_child).unwrap();

        // make sure the page is back to its original shape
        assert_unchanged(&snapshot, &page);

        // remove it again
        let (returned_key, returned_child) = page
            .internal_remove(&target_key)
            .unwrap()
            .expect("key is on the page");
        // make sure the returned values match
        assert_eq!(target_key, returned_key);
        assert_eq!(target_child, returned_child);

        // insert what was returned - again
        page.internal_insert(returned_key, returned_child).unwrap();

        // make sure the page is back to its original shape
        assert_unchanged(&snapshot, &page);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_remove_returns_none_when_its_not_there(
        LeafPage(mut page): LeafPage,
        target: u8,
    ) -> TestResult {
        if page.records().unwrap().next().is_none() {
            return TestResult::discard();
        }

        // find a key in the page
        let target = target as usize % page.records().unwrap().count();
        let target_row = page.records().unwrap().nth(target).cloned().unwrap();
        let target_key = ValidatedRow::from_row(target_row).primary_key();

        // remove it
        page.leaf_remove(&target_key)
            .unwrap()
            .expect("key is on the page");

        // take a snapshot
        let snapshot = page.clone();

        // try again - should return `Ok(None)`
        let result = page
            .leaf_remove(&target_key)
            .expect("leaf_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        // find a key outside the range
        let last_key =
            ValidatedRow::from_row(page.records().unwrap().last().cloned().unwrap()).primary_key();

        let out_of_bounds_key = higher_key(&last_key);

        let result = page
            .leaf_remove(&out_of_bounds_key)
            .expect("leaf_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        TestResult::passed()
    }

    #[quickcheck]
    fn internal_remove_returns_none_when_its_not_there(
        InternalPage(mut page): InternalPage,
        target: u8,
    ) -> TestResult {
        if page.keys().unwrap().next().is_none() {
            return TestResult::discard();
        }

        // find a key in the page
        let target = target as usize % page.keys().unwrap().count();
        let target_key = page.keys().unwrap().nth(target).cloned().unwrap();

        // remove it
        page.internal_remove(&target_key)
            .unwrap()
            .expect("key is on the page");

        // take a snapshot
        let snapshot = page.clone();

        // try again - should return `Ok(None)`
        let result = page
            .internal_remove(&target_key)
            .expect("internal_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        // find a key outside the range
        let last_key = page.keys().unwrap().last().cloned().unwrap();

        let out_of_bounds_key = higher_key(&last_key);

        let result = page
            .internal_remove(&out_of_bounds_key)
            .expect("internal_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        TestResult::passed()
    }

    #[quickcheck]
    fn removes_fail_with_wrong_page_type(
        LeafPage(mut leaf): LeafPage,
        InternalPage(mut internal): InternalPage,
    ) -> TestResult {
        let internal_key = internal.keys().unwrap().next().cloned().unwrap();
        let leaf_key =
            ValidatedRow::from_row(leaf.records().unwrap().next().cloned().unwrap()).primary_key();

        let res1 = internal.leaf_remove(&internal_key);
        let res2 = leaf.internal_remove(&leaf_key);

        assert_matches!(res1, Err(PageError::NotLeaf));
        assert_matches!(res2, Err(PageError::NotInternal));

        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_after_internal_remove(
        InternalPage(mut page): InternalPage,
        target: u8,
        probes: Vec<Key>,
    ) -> TestResult {
        let old_keys: Vec<Key> = page.keys().unwrap().cloned().collect();
        let old_children: Vec<PageId> = page.children().unwrap().cloned().collect();
        if old_keys.is_empty() {
            return TestResult::discard();
        }

        // remove separator i (and it's right child i+1)
        let i = target as usize % old_keys.len();
        page.internal_remove(&old_keys[i])
            .unwrap()
            .expect("separator is one the page");

        let expected_after_remove = |probe: &Key| {
            let old_index = old_keys.iter().filter(|k| *k <= probe).count();
            if old_index == i + 1 {
                old_children[i]
            } else {
                old_children[old_index]
            }
        };

        let boundary_probes = old_keys
            .iter()
            .cloned()
            .chain([higher_key(old_keys.last().unwrap())]);
        for probe in probes.into_iter().chain(boundary_probes) {
            assert_eq!(
                page.body.find_child(&probe),
                Some(expected_after_remove(&probe)),
                "routing {probe:?} after removing keys[{i}]"
            );
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn borrow_and_borrow_back_leaf_remains_same(LeafPage(mut left): LeafPage) -> TestResult {
        let left_page_id = left.page_id;
        let Some((separator, mut right)) = try_split(&mut left, left_page_id.wrapping_add(1))
        else {
            return TestResult::discard();
        };
        let (left_before, right_before) = (left.clone(), right.clone());

        // borrow the right page's first row into the left page
        let key1 = match left.leaf_borrow_from_right(&mut right) {
            Ok(k) => k,
            Err(
                PageError::PageFull | PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)),
            ) => return TestResult::discard(),
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };
        // the new separator is the right page's new first key
        let right_first = right.records().unwrap().next().unwrap();
        assert_eq!(right_first.cmp_key(&key1), Ordering::Equal);

        // borrow it straight back
        let key2 = right
            .leaf_borrow_from_left(&mut left)
            .expect("borrow back must succeed");
        assert_eq!(
            key2, separator,
            "borrow-back must restore the original separator"
        );

        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn borrow_and_borrow_back_internal_remains_same(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let left_page_id = left.page_id;
        let Some((separator, mut right)) = try_split(&mut left, left_page_id.wrapping_add(1))
        else {
            return TestResult::discard();
        };
        let (left_before, right_before) = (left.clone(), right.clone());

        // the separator comes down, the right page's first child moves over, its first key goes up
        let key1 = match left.internal_borrow_from_right(&mut right, separator.clone()) {
            Ok(k) => k,
            Err(
                PageError::PageFull | PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)),
            ) => return TestResult::discard(),
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        // the new separator is the old right page's first key
        let right_first = right_before.keys().unwrap().next().unwrap();
        assert_eq!(right_first, &key1);

        // borrow it straight back
        let key2 = right
            .internal_borrow_from_left(&mut left, key1)
            .expect("borrow back must succeed");
        assert_eq!(
            key2, separator,
            "borrow-back must restore the original separator"
        );

        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_fail_with_wrong_page_type(
        LeafPage(mut leaf): LeafPage,
        InternalPage(mut internal): InternalPage,
    ) -> TestResult {
        // This error is top dog so all other problems with this scenario don't matter
        let res1 = internal.leaf_borrow_from_left(&mut leaf);
        let res2 = internal.leaf_borrow_from_right(&mut leaf);
        let res3 = leaf.leaf_borrow_from_left(&mut internal);
        let res4 = leaf.leaf_borrow_from_right(&mut internal);

        assert_matches!(res1, Err(PageError::NotLeaf));
        assert_matches!(res2, Err(PageError::NotLeaf));
        assert_matches!(res3, Err(PageError::NotLeaf));
        assert_matches!(res4, Err(PageError::NotLeaf));

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_fails_when_dest_full(payload_sizes: Vec<u16>) -> TestResult {
        if payload_sizes.is_empty() {
            return TestResult::discard();
        }
        // Two valid, adjacent leaves: fill one page, then split it.
        // fill_leaf uses keys 0, 10, 20, ... so every key on the left page is >= 0.
        let mut left = fill_leaf(child(1), payload_sizes);
        let Some((_separator, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };
        // the donor must have a row to spare, or the borrow is rejected for that reason instead
        if right.records().unwrap().count() < 2 {
            return TestResult::discard();
        }
        let right_first = right.records().unwrap().next().cloned().unwrap();

        // Refill the left page with the smallest possible rows until the right page's first
        // row no longer fits. Negative keys are below every existing key, so they're unique
        // and always sort before the separator: the only thing wrong is the size.
        let (schema, _) = leaf_schema();
        let mut key = -1i64;
        while left.can_insert(&right_first) {
            let filler = Row {
                fields: vec![RowValue::Integer(key), RowValue::String(String::new())],
            };
            left.leaf_insert(schema.validate_row(filler).unwrap())
                .unwrap();
            key -= 1;
        }

        let (left_before, right_before) = (left.clone(), right.clone());
        let result = left.leaf_borrow_from_right(&mut right);

        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrow_returns_error_with_empty_donor(LeafPage(mut left): LeafPage) -> TestResult {
        let mut right = Page::empty_leaf(left.page_id.wrapping_add(1));

        left.set_next(Some(right.page_id)).unwrap();

        let before_left = left.clone();
        let before_right = right.clone();
        let result = left.leaf_borrow_from_right(&mut right);

        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        // now switch sides
        left.set_prev(Some(right.page_id)).unwrap();
        let before_left = left.clone();

        let result = left.leaf_borrow_from_left(&mut right);
        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        TestResult::passed()
    }

    /// A two-row leaf (both pointers `None`) and the byte offset of its slot array.
    fn two_row_leaf() -> (RawPage, usize) {
        let (schema, _) = leaf_schema();
        let mut page = Page::empty_leaf(child(1));
        for key in [1, 2] {
            let row = Row {
                fields: vec![RowValue::Integer(key), RowValue::String("abc".into())],
            };
            page.leaf_insert(schema.validate_row(row).unwrap()).unwrap();
        }
        // header with both sibling pointers None: MAX_LEAF_HEADER_SIZE minus 8 bytes each
        (
            page.as_raw_page().unwrap(),
            MAX_LEAF_HEADER_SIZE - 2 * PAGE_ID_SIZE,
        )
    }

    fn get_slot(bytes: &RawPage, slot_array: usize, slot: usize) -> (u16, u16) {
        let at = slot_array + slot * SLOT_ENTRY_SIZE;
        (
            u16::from_be_bytes([bytes[at], bytes[at + 1]]),
            u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]),
        )
    }

    fn set_slot(bytes: &mut RawPage, slot_array: usize, slot: usize, offset: u16, length: u16) {
        let at = slot_array + slot * SLOT_ENTRY_SIZE;
        bytes[at..at + 2].copy_from_slice(&offset.to_be_bytes());
        bytes[at + 2..at + 4].copy_from_slice(&length.to_be_bytes());
    }

    fn fix_checksum(bytes: &mut RawPage) {
        let crc = Page::page_checksum(bytes);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
    }

    #[test]
    fn overlapping_slots_are_corrupt() {
        let (mut bytes, slots) = two_row_leaf();
        // slot 1's row sits just before slot 0's; stretch it one byte into slot 0's row
        let (offset, length) = get_slot(&bytes, slots, 1);
        set_slot(&mut bytes, slots, 1, offset, length + 1);
        fix_checksum(&mut bytes);
        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt {
                kind: CorruptionKind::OverlappingSlots { .. },
                ..
            })
        );
    }

    #[test]
    fn slot_pointing_into_slot_array_is_corrupt() {
        let (mut bytes, slots) = two_row_leaf();
        set_slot(&mut bytes, slots, 0, slots as u16, 4);
        fix_checksum(&mut bytes);
        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt {
                kind: CorruptionKind::InvalidRange { slot: 0, .. },
                ..
            })
        );
    }

    #[test]
    fn slot_with_trailing_bytes_is_corrupt() {
        let (mut bytes, slots) = two_row_leaf();
        // move slot 1's row one byte earlier (into free space) and grow its slot by one,
        // leaving one extra byte after the row that nothing else uses
        let (offset, length) = get_slot(&bytes, slots, 1);
        let (o, l) = (offset as usize, length as usize);
        bytes.copy_within(o..o + l, o - 1);
        bytes[o - 1 + l] = 0xAB;
        set_slot(&mut bytes, slots, 1, (o - 1) as u16, length + 1);
        fix_checksum(&mut bytes);
        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt {
                kind: CorruptionKind::TrailingBytes { slot: 1 },
                ..
            })
        );
    }

    #[quickcheck]
    fn too_many_slots_triggers_error(mut num_items: u16) -> TestResult {
        let (mut bytes, slot_array) = two_row_leaf();
        num_items = num_items.saturating_add(1 + MAX_LEAF_ITEMS as u16);

        // num_items is always the two bytes preceding the slot_array
        let offset = slot_array - 2;
        // write our big num_items value into the right spot
        bytes[offset..offset + 2].copy_from_slice(&num_items.to_be_bytes());
        fix_checksum(&mut bytes);

        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt { kind: CorruptionKind::TooManyItems(n), .. }) if n == num_items as usize
        );
        TestResult::passed()
    }

    #[test]
    fn slot_count_at_boundary_doesnt_trigger_toomanyitems() {
        let (mut bytes, slot_array) = two_row_leaf();
        let offset = slot_array - 2;
        // check right at the boundary
        let num_items = MAX_LEAF_ITEMS as u16;
        bytes[offset..offset + 2].copy_from_slice(&num_items.to_be_bytes());
        fix_checksum(&mut bytes);

        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt { kind, .. }) if !matches!(kind, CorruptionKind::TooManyItems(_))
        );
    }
}

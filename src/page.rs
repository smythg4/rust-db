use crate::commontypes::{
    Key, KeyError, Lsn, LsnError, PAGE_ID_SIZE, PageId, PageLsn, SLOT_ENTRY_SIZE, SlotEntry,
};

use crate::schema::{Row, RowValue, RowValueError, SchemaError, ValidatedRow};
use crate::traits::Serializable;
use crc32_light::Crc32Stream;
use std::cmp::Ordering;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use thiserror::Error;

/// The page size for the entire application
pub const PAGE_SIZE: usize = 4096;

/// Tag codes for page serialization
pub const LEAF_TAG: u8 = 1;
pub const INTERNAL_TAG: u8 = 2;
pub const META_TAG: u8 = 3;
pub const FREE_TAG: u8 = 4;

/// Largest possible encoding of headers for pages
pub const MAX_LEAF_HEADER_SIZE: usize = 41;
pub const MAX_INTERNAL_HEADER_SIZE: usize = 23;

/// The offset where the checksum value lives (it's a u32, so 4 bytes long)
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

/// Useable space available in a `Page` for data storage
const LEAF_USABLE: usize = PAGE_SIZE - MAX_LEAF_HEADER_SIZE;
const INTERNAL_USABLE: usize = PAGE_SIZE - MAX_INTERNAL_HEADER_SIZE - PAGE_ID_SIZE;

/// A leaf is underfull below this many bytes of entries: two such leaves always fit in one.
pub const LEAF_UNDERFULL_BYTES: usize = LEAF_USABLE / 2;
/// An internal page is underfull below this many bytes of entries: two such pages
/// plus the largest possible separator always fit in one.
pub const INTERNAL_UNDERFULL_BYTES: usize = (INTERNAL_USABLE - MAX_INTERNAL_ENTRY_SIZE) / 2;

/// Assertions make sure that `.is_underfull` will always allow merging of another underfull `Page`
const _: () = assert!(2 * LEAF_UNDERFULL_BYTES <= LEAF_USABLE);
const _: () = assert!(2 * INTERNAL_UNDERFULL_BYTES + MAX_INTERNAL_ENTRY_SIZE <= INTERNAL_USABLE);

/// Upper bound on records in a leaf: every entry at its smallest possible size.
pub const MAX_LEAF_ITEMS: usize =
    (PAGE_SIZE - MIN_LEAF_HEADER_SIZE) / (SLOT_ENTRY_SIZE + MIN_ROW_ENCODED_SIZE);

/// Upper bound on keys in an internal page: every entry at its smallest possible size,
/// plus the one extra child that has no key.
pub const MAX_INTERNAL_ITEMS: usize = (PAGE_SIZE - MAX_INTERNAL_HEADER_SIZE - PAGE_ID_SIZE)
    / (SLOT_ENTRY_SIZE + PAGE_ID_SIZE + MIN_KEY_ENCODED_SIZE);

/// Assertions make sure that constants governing component limits can never exceed the declared PAGE_SIZE
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
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error("Page too small to split: {0}")]
    TooSmallToSplit(PageId),
    #[error("Page too full to fit row -- need to split")]
    PageFull,
    #[error("Attempt to insert a duplicate key")]
    DuplicateKey,
    #[error("Attempt to complete operation on page type that doesn't support it")]
    WrongPageType,
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
    #[error("Stale Lsn. Old {old}, New {new}")]
    StaleLsnUpdate { old: Lsn, new: Lsn },
    #[error("Attempt to replace a `Key` that doesn't exist: {search_key:?} with {new_key:?}")]
    MissingKey { search_key: Key, new_key: Key },
    #[error("Attempt to insert a `Key` that's out of order with others: {0:?}")]
    KeyNotInOrder(Key),
    #[error("Attempt to pop a page off the free list that wasn't the head")]
    NotFreeListHead { head: Option<PageId>, got: PageId },
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum BorrowFailReason {
    #[error("Attempt to borrow from empty page: {0}")]
    EmptyBorrow(PageId),
    #[error("Inserting Key outside page bounds: {0:?}")]
    KeysOutOfOrder(Key),
    #[error("Requested Neighbor is {expected:?} but got {got:?}")]
    PointerMismatch {
        expected: Option<PageId>,
        got: Option<PageId>,
    },
}

#[derive(Error, Debug, Clone, PartialEq)]
pub enum MergeFailReason {
    #[error("Right neighbor is {expected:?} but got {got:?}")]
    PointerMismatch {
        expected: Option<PageId>,
        got: Option<PageId>,
    },
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
    RowTooLarge { slot: usize },
    KeyTooLarge { slot: usize },
    PageNumOutOfRange(usize), // that page number that exceeds the meta data's stored page_count
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
    Meta {
        root_id: PageId,
        page_count: u32,
        free_list_head: Option<PageId>,
    },
    Free {
        next: Option<PageId>, // pointer to the next free page
    },
}

impl Page {
    pub fn page_id(&self) -> PageId {
        self.page_id
    }

    pub fn lsn(&self) -> Option<Lsn> {
        self.last_update.0
    }

    pub fn set_lsn(&mut self, new_lsn: Lsn) -> Result<(), PageError> {
        if self.last_update.0.is_some_and(|l| l > new_lsn) {
            return Err(PageError::StaleLsnUpdate {
                old: self.last_update.0.unwrap(),
                new: new_lsn,
            });
        }
        self.last_update.0 = Some(new_lsn);
        Ok(())
    }

    /// Used to `pop` the next free page off the list
    pub fn free_list_pop(&mut self, page: &Page) -> Result<PageId, PageError> {
        if !page.is_free() || !self.is_meta() {
            return Err(PageError::WrongPageType);
        }
        let PageBody::Meta { free_list_head, .. } = &mut self.body else {
            unreachable!()
        };
        if *free_list_head != Some(page.page_id) {
            return Err(PageError::NotFreeListHead {
                head: *free_list_head,
                got: page.page_id,
            });
        }

        let PageBody::Free { next } = &page.body else {
            unreachable!()
        };

        *free_list_head = *next;

        self.debug_check_invariants("free_list_pop");
        Ok(page.page_id)
    }

    /// Used to update the free_list on a `Meta` `Page`. `freed` must be part of the same `Table`,
    /// must have a page_num that's less than the meta page's `num_pages`, and can't be page number
    /// 0 since that's reserved for the `Meta` `Page`.
    pub fn free_list_push(&mut self, freed: &mut Page) -> Result<(), PageError> {
        if !self.is_meta() || !freed.is_free() {
            return Err(PageError::WrongPageType);
        }
        let PageBody::Meta {
            free_list_head,
            root_id,
            page_count,
        } = &mut self.body
        else {
            unreachable!()
        };
        let PageBody::Free { next } = &mut freed.body else {
            unreachable!()
        };

        if *free_list_head == *next {
            // head was the same, no-op
            return Ok(());
        }

        match next {
            None => return Ok(()), // another no-op
            Some(free_id) => {
                if free_id.get_table_id() != root_id.get_table_id() {
                    // page in wrong table
                    // TODO: This isn't the right error type, make a new one
                    return Err(PageError::DuplicateKey);
                }
                if free_id.get_page_num() >= *page_count {
                    // Page out of range
                    // TODO: This isn't the right error type, make a new one
                    return Err(PageError::DuplicateKey);
                }
                if free_id.get_page_num() == 0 {
                    // reserved meta page
                    // TODO: This isn't the right error type, make a new one
                    return Err(PageError::DuplicateKey);
                }
            }
        }

        *next = *free_list_head;

        *free_list_head = Some(freed.page_id);

        self.debug_check_invariants("free_list_push");
        Ok(())
    }

    /// Converts `Page` of any type into a free `Page`
    pub fn make_free(&mut self) {
        self.body = PageBody::Free { next: None };
    }

    pub fn empty_leaf(page_id: PageId) -> Self {
        let body = PageBody::Leaf {
            records: Vec::new(),
            next: None,
            prev: None,
        };
        Self::empty_page(page_id, body)
    }

    pub fn is_leaf(&self) -> bool {
        matches!(self.body, PageBody::Leaf { .. })
    }

    pub fn is_internal(&self) -> bool {
        matches!(self.body, PageBody::Internal { .. })
    }

    pub fn is_meta(&self) -> bool {
        matches!(self.body, PageBody::Meta { .. })
    }

    pub fn is_free(&self) -> bool {
        matches!(self.body, PageBody::Free { .. })
    }

    pub(crate) fn empty_page(page_id: PageId, body: PageBody) -> Self {
        Page {
            page_id,
            last_update: PageLsn(None),
            body,
        }
    }

    #[allow(dead_code)] // remove this once we have external callers
    pub(crate) fn new_root(page_id: PageId, left: PageId, separator: Key, right: PageId) -> Self {
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
            PageBody::Meta { .. } => META_TAG,
            PageBody::Free { .. } => FREE_TAG,
        }
    }

    /// Returns the number of records present in a `Leaf` page or the number of keys present in a `Internal` page.
    pub fn num_items(&self) -> usize {
        match &self.body {
            PageBody::Leaf { records, .. } => records.len(),
            PageBody::Internal { keys, .. } => keys.len(),
            PageBody::Free { .. } | PageBody::Meta { .. } => 0,
        }
    }

    /// Returns the number of bytes available for data in the page. Because the headers can be of variable size depending
    /// on the `Option` variant, we always reserve the maximum space required (e.g. assuming we have two siblings in the `Leaf`
    /// case). Returns `None` if the `Page` won't fit into a [u8; PAGE_SIZE] space, which means somewhere data overflowed.
    pub fn free_space(&self) -> Option<usize> {
        let header_len = match &self.body {
            PageBody::Leaf { .. } => MAX_LEAF_HEADER_SIZE,
            _ => MAX_INTERNAL_HEADER_SIZE,
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
                PageBody::Free { .. } => 1 + PAGE_ID_SIZE,
                PageBody::Meta { .. } => 1 + PAGE_ID_SIZE,
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
            PageBody::Internal { .. } | PageBody::Free { .. } | PageBody::Meta { .. } => {}
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
            PageBody::Free { next } => {
                // Free pages are just the next pointer, simple enough!
                next.serialize(writer)?;
                Ok((0, 0))
            }
            PageBody::Meta {
                root_id,
                page_count,
                free_list_head,
            } => {
                // write the data straight out in order
                root_id.serialize(writer)?;
                writer.write_all(&page_count.to_be_bytes())?;
                free_list_head.serialize(writer)?;
                Ok((0, 0))
            }
        }
    }

    /// Returns the `Page` represented as raw bytes (`RawPage = [0u8; PAGE_SIZE]`).
    /// Will error if write to internal `Cursor` fails or if the `Page` would overflow
    /// a `RawPage`.
    pub fn as_raw_page(&self) -> Result<RawPage, PageError> {
        self.check_invariants().map_err(|kind| PageError::Corrupt {
            page_id: Some(self.page_id),
            kind,
        })?;
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
        if !self.is_leaf() {
            return false;
        }
        // immediately return `false` if the page is already overfull
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        // new row will be encoded and a corresponding slot index is allocated, both parts need to fit
        Self::leaf_entry_size(row) <= free_space
    }

    /// Returns `true` if `key` can fit in the `Page` without overflowing
    /// a `RawPage` when converted to raw bytes.
    pub fn can_insert_separator(&self, key: &Key) -> bool {
        if !self.is_internal() {
            return false;
        }
        // immediately return `false` if the page is already overfull
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        // new key will be encoded, child index and a corresponding slot index is allocated, all parts need to fit
        Self::internal_entry_size(key) <= free_space
    }

    /// Returns `true` if the `Page` is underfull and can be merged with any other underfull `Page`
    pub fn is_underfull(&self) -> bool {
        match &self.body {
            PageBody::Leaf { .. } => self.entries_size() < LEAF_UNDERFULL_BYTES,
            PageBody::Internal { .. } => self.entries_size() < INTERNAL_UNDERFULL_BYTES,
            _ => false,
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
        if Self::internal_entry_size(&separator) > MAX_INTERNAL_ENTRY_SIZE {
            return Err(PageError::Schema(SchemaError::KeyTooLong(
                Self::internal_entry_size(&separator),
            )));
        }
        let can_insert = matches!(self.free_space(), Some(free_space) if free_space >= Self::internal_entry_size(&separator));

        let PageBody::Internal { keys, children } = &mut self.body else {
            return Err(PageError::WrongPageType);
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
            return Err(PageError::WrongPageType);
        };
        let new_key = validated_row.primary_key();
        let can_insert = self.can_insert(validated_row.as_ref());
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
        records.insert(insert_pos, validated_row.into());
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
            _ => Err(PageError::WrongPageType),
        }
    }

    /// If the merge is legal, this will drain the right `Page` of any entries and merge them into
    /// this one. Right `Page` won't be affected in the event of failure. Returns the `PageId` of the
    /// merged `Page` that can be recycled to a free list. `right` is converted into a free page
    /// upon success
    pub fn leaf_merge_from_right(&mut self, right: &mut Page) -> Result<PageId, PageError> {
        if !self.is_leaf() || !right.is_leaf() {
            return Err(PageError::WrongPageType);
        }
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
                        right.make_free();
                        Ok(right_id)
                    }
                    _ => Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch {
                        expected: *next,
                        got: Some(right_id),
                    })),
                }
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    /// If the merge is legal, this will drain the right `Page` of any entries and merge them into
    /// this one. Right `Page` won't be affected in the event of failure. `separator` will be installed
    /// at the right point in the newly merged `Page`. Returns the `PageId` of the
    /// merged `Page` that can be recycled to a free list. `right` is converted into a free page
    /// upon success
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
                right.make_free();
                Ok(right_id)
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    /// Accepts a row from the right leaf neighbor. Returns the promoted Key to replace in the parent on success
    pub fn leaf_borrow_from_right(&mut self, right_page: &mut Page) -> Result<Key, PageError> {
        if !self.is_leaf() || !right_page.is_leaf() {
            return Err(PageError::WrongPageType);
        }

        // make sure the right page is actually THIS page's right page
        let right_page_id = right_page.page_id;
        match self.next()? {
            Some(np) if np == right_page_id => {}
            other => {
                return Err(PageError::InvalidBorrow(
                    BorrowFailReason::PointerMismatch {
                        expected: other,
                        got: Some(right_page_id),
                    },
                ));
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
        if !self.is_leaf() || !left_page.is_leaf() {
            return Err(PageError::WrongPageType);
        }

        // make sure the left page is actually THIS page's left page
        let left_page_id = left_page.page_id;
        match self.prev()? {
            Some(pp) if pp == left_page_id => {}
            other => {
                return Err(PageError::InvalidBorrow(
                    BorrowFailReason::PointerMismatch {
                        expected: other,
                        got: Some(left_page_id),
                    },
                ));
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
        if !self.is_internal() || !right_page.is_internal() {
            return Err(PageError::WrongPageType);
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
        if !self.is_internal() || !left_page.is_internal() {
            return Err(PageError::WrongPageType);
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
            _ => 0,
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
            return Err(PageError::WrongPageType);
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
            return Err(PageError::WrongPageType);
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
            _ => Err(PageError::WrongPageType),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_next(&mut self, new: Option<PageId>) -> Result<(), PageError> {
        match &mut self.body {
            PageBody::Leaf { next, .. } => {
                *next = new;
                Ok(())
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn prev(&self) -> Result<Option<PageId>, PageError> {
        match self.body {
            PageBody::Leaf { prev, .. } => Ok(prev),
            _ => Err(PageError::WrongPageType),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_prev(&mut self, new: Option<PageId>) -> Result<(), PageError> {
        match &mut self.body {
            PageBody::Leaf { prev, .. } => {
                *prev = new;
                Ok(())
            }
            _ => Err(PageError::WrongPageType),
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
        if self.free_space().is_none() {
            return Err(CorruptionKind::ExceedsCapacity);
        }
        match &self.body {
            PageBody::Leaf { records, .. } => {
                let mut prev: Option<Key> = None;
                for (i, record) in records.iter().enumerate() {
                    if Self::leaf_entry_size(record) > MAX_LEAF_ENTRY_SIZE {
                        return Err(CorruptionKind::RowTooLarge { slot: i });
                    }
                    let first = record.fields.first().ok_or(CorruptionKind::MissingKey)?;
                    let key = Key::try_from(first)
                        .map_err(|_| CorruptionKind::InvalidKey(first.clone()))?;
                    if Self::internal_entry_size(&key) > MAX_INTERNAL_ENTRY_SIZE {
                        return Err(CorruptionKind::KeyTooLarge { slot: i });
                    }
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
                if let Some(slot) = keys
                    .iter()
                    .position(|k| Self::internal_entry_size(k) > MAX_INTERNAL_ENTRY_SIZE)
                {
                    return Err(CorruptionKind::KeyTooLarge { slot });
                }
                if let Some(i) = keys.windows(2).position(|w| w[0] >= w[1]) {
                    return Err(CorruptionKind::UnsortedKeys { at: i + 1 });
                }
            }
            PageBody::Meta {
                page_count,
                free_list_head,
                ..
            } => {
                if let Some(flh) = free_list_head
                    && flh.get_page_num() >= *page_count
                {
                    return Err(CorruptionKind::PageNumOutOfRange(
                        flh.get_page_num() as usize
                    ));
                }
            }
            PageBody::Free { .. } => {} // nothing to check within the page. BPM needs to make sure it's in the right table
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

    pub fn find_child(&self, search_key: &Key) -> Option<PageId> {
        self.find_child_index(search_key).map(|(_, id)| id)
    }

    pub(crate) fn find_child_index(&self, search_key: &Key) -> Option<(ChildIndex, PageId)> {
        let PageBody::Internal { keys, children } = &self.body else {
            return None;
        };
        let i = match keys.binary_search(search_key) {
            Ok(i) => i + 1, // equal to a separator: route right
            Err(i) => i,    // between separators
        };
        Some((ChildIndex(i), children[i]))
    }

    #[allow(dead_code)] // I'll need it for BTrees
    pub(crate) fn child_at(&self, idx: ChildIndex) -> Option<PageId> {
        let PageBody::Internal { children, .. } = &self.body else {
            return None;
        };
        children.get(idx.0).copied()
    }

    #[allow(dead_code)] // I'll need it for BTrees
    pub(crate) fn key_at(&self, idx: KeyIndex) -> Option<&Key> {
        let PageBody::Internal { keys, .. } = &self.body else {
            return None;
        };
        keys.get(idx.0)
    }

    /// Finds a record in a leaf given a key to search. Returns `None` if the
    /// key isn't in the `Page`
    pub fn leaf_get(&self, key: &Key) -> Result<Option<&Row>, PageError> {
        let PageBody::Leaf { records, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(records
            .binary_search_by(|r| r.cmp_key(key))
            .ok()
            .map(|i| &records[i]))
    }

    /// Returns an iterator of records in a leaf that are >= `start_key`
    pub fn leaf_records_from<'a>(
        &'a self,
        start_key: &Key,
    ) -> Result<impl Iterator<Item = &'a Row> + use<'a>, PageError> {
        let PageBody::Leaf { records, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        let start = records.partition_point(|r| r.cmp_key(start_key) == Ordering::Less);
        Ok(records[start..].iter())
    }

    /// Accepts a `Key` by value to replace a `&Key` in an internal `Page`
    /// Used to replace a separator after a borrow, keeping its children in place
    pub fn internal_replace_key(&mut self, old: &Key, new: Key) -> Result<(), PageError> {
        let fits = self.free_space().is_some_and(|free| {
            free + Self::internal_entry_size(old) >= Self::internal_entry_size(&new)
        });
        let PageBody::Internal { keys, .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        let idx = keys.binary_search(old).map_err(|_| PageError::MissingKey {
            search_key: old.clone(),
            new_key: new.clone(),
        })?;

        let above_left = idx == 0 || keys[idx - 1] < new;
        let below_right = idx + 1 == keys.len() || new < keys[idx + 1];
        if !(above_left && below_right) {
            return Err(PageError::KeyNotInOrder(new));
        }
        if !fits {
            return Err(PageError::PageFull);
        }

        keys[idx] = new;
        self.debug_check_invariants("internal_replace_key");
        Ok(())
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

        let page_id = PageId::deserialize(&mut cursor)?;
        let last_update = PageLsn::deserialize(&mut cursor)?;

        // helper closure for error mapping
        let corrupt = |kind| PageError::Corrupt {
            page_id: Some(page_id),
            kind,
        };

        // skip the crc32 portion
        cursor.set_position(CHECKSUM_OFFSET as u64 + 4);

        let (next_page, prev_page) = if tag == LEAF_TAG {
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

        let max_items = match tag {
            META_TAG => 0,
            FREE_TAG => 0,
            INTERNAL_TAG => MAX_INTERNAL_ITEMS,
            LEAF_TAG => MAX_LEAF_ITEMS,
            _ => {
                return Err(PageError::Corrupt {
                    page_id: None,
                    kind: CorruptionKind::InvalidTag(tag),
                });
            }
        };
        if num_items > max_items {
            return Err(corrupt(CorruptionKind::TooManyItems(num_items)));
        }

        let body = match tag {
            LEAF_TAG => {
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
            }
            INTERNAL_TAG => {
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
            }
            FREE_TAG => {
                let next = Option::<PageId>::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
                PageBody::Free { next }
            }
            META_TAG => {
                let root_id = PageId::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
                let mut buf_four = [0u8; 4];
                cursor.read_exact(&mut buf_four)?;
                let page_count = u32::from_be_bytes(buf_four);
                let free_list_head = Option::<PageId>::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
                PageBody::Meta {
                    root_id,
                    page_count,
                    free_list_head,
                }
            }
            _ => unreachable!(),
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

/// Position of a child pointer in an internal page (0..=keys.len()).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChildIndex(usize);

impl ChildIndex {
    #[allow(dead_code)] // I'll need it for BTrees
    /// The separator between this child and the next one.
    pub(crate) fn right_separator(self) -> KeyIndex {
        KeyIndex(self.0)
    }
    #[allow(dead_code)] // I'll need it for BTrees
    /// The separator between the previous child and this one (`None` for the first child).
    pub(crate) fn left_separator(self) -> Option<KeyIndex> {
        self.0.checked_sub(1).map(KeyIndex)
    }
    #[allow(dead_code)] // I'll need it for BTrees
    pub(crate) fn right_sibling(self) -> ChildIndex {
        ChildIndex(self.0 + 1)
    }
    #[allow(dead_code)] // I'll need it for BTrees
    pub(crate) fn left_sibling(self) -> Option<ChildIndex> {
        self.0.checked_sub(1).map(ChildIndex)
    }
}

/// Position of a separator key in an internal page (0..keys.len()).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct KeyIndex(usize);

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use quickcheck::{Arbitrary, Gen, TestResult};
    use quickcheck_macros::quickcheck;

    use super::*;
    use crate::commontypes::{Lsn, TableId};
    use crate::schema::RowValue;
    use crate::test_support::*;

    impl Arbitrary for Page {
        fn arbitrary(g: &mut Gen) -> Self {
            if bool::arbitrary(g) {
                arbitrary_leaf(g)
            } else {
                arbitrary_internal(g)
            }
        }
    }

    /// A leaf built through the real API: random rows inserted until a generated target
    /// count is reached or the page fills up. Every generated leaf is one `leaf_insert` can
    /// actually produce: sorted, unique keys, within size limits, from empty to full.
    fn arbitrary_leaf(g: &mut Gen) -> Page {
        let mut page = Page::empty_leaf(PageId::arbitrary(g));
        page.last_update = PageLsn(Option::<Lsn>::arbitrary(g));
        page.set_next(Option::<PageId>::arbitrary(g)).unwrap();
        page.set_prev(Option::<PageId>::arbitrary(g)).unwrap();
        for _ in 0..gen_len(g, MAX_LEAF_ITEMS) {
            match page.leaf_insert(ValidatedRow::from_row(Row::arbitrary(g))) {
                Ok(()) | Err(PageError::DuplicateKey) => {}
                Err(PageError::PageFull) => break,
                Err(e) => panic!("unexpected error building a leaf: {e:?}"),
            }
        }
        page
    }

    /// An internal page built through the real API: random key/child pairs inserted until a
    /// generated target count is reached or the page fills up. Every generated page is one
    /// `internal_insert` can actually produce: sorted, unique keys, within size limits,
    /// from empty to full.
    fn arbitrary_internal(g: &mut Gen) -> Page {
        let mut page = Page::empty_page(
            PageId::arbitrary(g),
            PageBody::Internal {
                keys: Vec::new(),
                children: vec![PageId::arbitrary(g)],
            },
        );
        page.last_update = PageLsn(Option::<Lsn>::arbitrary(g));
        let all_int_keys = bool::arbitrary(g);
        for _ in 0..gen_len(g, MAX_INTERNAL_ITEMS) {
            let key = if all_int_keys {
                Key::Integer(i64::arbitrary(g))
            } else {
                Key::String(String::arbitrary(g))
            };
            let child_id = PageId::arbitrary(g);
            match page.internal_insert(key, child_id) {
                Ok(()) | Err(PageError::DuplicateKey) => {}
                Err(PageError::PageFull) => break,
                Err(e) => panic!("unexpected error building an internal page: {e:?}"),
            }
        }
        page
    }

    #[derive(Debug, Clone)]
    struct LeafPage(Page);

    #[derive(Debug, Clone)]
    struct InternalPage(Page);

    impl Arbitrary for LeafPage {
        fn arbitrary(g: &mut Gen) -> Self {
            LeafPage(arbitrary_leaf(g))
        }

        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            let page = self.0.clone();
            let keys: Vec<Key> = page
                .records()
                .unwrap()
                .map(|r| Key::try_from(&r.fields[0]).unwrap())
                .collect();

            Box::new(keys.into_iter().map(move |key| {
                let mut smaller = page.clone();
                smaller.leaf_remove(&key).unwrap();
                LeafPage(smaller)
            }))
        }
    }

    impl Arbitrary for InternalPage {
        fn arbitrary(g: &mut Gen) -> Self {
            InternalPage(arbitrary_internal(g))
        }

        fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
            let page = self.0.clone();
            let keys: Vec<Key> = page.keys().unwrap().cloned().collect();

            Box::new(keys.into_iter().map(move |key| {
                let mut smaller = page.clone();
                smaller.internal_remove(&key).unwrap();
                InternalPage(smaller)
            }))
        }
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

    #[quickcheck]
    fn split_then_merge_roundtrip(mut page: Page) -> TestResult {
        if !page.is_leaf() && !page.is_internal() {
            return TestResult::discard();
        }
        let snapshot = page.clone();
        let new_id = page.page_id.wrapping_add(1);

        let Some((separator, mut new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let freed_id = match &page.body {
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut new_page, separator),
            PageBody::Leaf { .. } => page.leaf_merge_from_right(&mut new_page),
            _ => unreachable!(),
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
            Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch {
                expected: None,
                got: _,
            }))
        );
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&new_page, &snapshot2);

        // trying again but with a `Some` value
        page.set_next(Some(page.page_id)).unwrap();
        let snapshot1 = page.clone();
        let result = page.leaf_merge_from_right(&mut new_page);

        assert_matches!(
            result,
            Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch {
                expected: Some(_),
                got: _,
            }))
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
        // make sure there's something in there
        if page.num_items() == 0 {
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
            _ => unreachable!(),
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
            _ => unreachable!(),
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

                assert_matches!(result, Err(PageError::WrongPageType));
                assert_unchanged(&page1, &snapshot1);
                assert_unchanged(&page2, &snapshot2);
                TestResult::passed()
            }
            (PageBody::Internal { .. }, PageBody::Leaf { .. }) => {
                let result = page1.internal_merge_from_right(&mut page2, dummy_key);
                assert_matches!(result, Err(PageError::WrongPageType));
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
        assert_eq!(half.find_child(&big_key), Some(new_child));

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
                assert_eq!(page.find_child(&key), Some(right_child));
            }
        }

        let expected_keys: Vec<Key> = expected.keys().cloned().collect();
        assert_eq!(
            page.keys().unwrap().cloned().collect::<Vec<_>>(),
            expected_keys
        );
        // children: the untouched leftmost child, then each key's right child in key order
        let expected_children: Vec<PageId> = [leftmost]
            .into_iter()
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

            let is_duplicate = expected.contains_key(&key);
            let is_full = !page.can_insert(validated_row.as_ref());

            let page_clone = page.clone();

            let result = page.leaf_insert(validated_row.clone());
            if is_duplicate {
                assert_matches!(result, Err(PageError::DuplicateKey));
                assert_unchanged(&page_clone, &page);
            } else if is_full {
                assert_matches!(result, Err(PageError::PageFull));
                assert_unchanged(&page_clone, &page);
            } else {
                assert!(result.is_ok());
                expected.insert(key, Row::from(validated_row));
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
            _ => unreachable!(),
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
            _ => unreachable!(),
        };
        TestResult::passed()
    }

    #[quickcheck]
    fn no_rows_lost_in_leaf_split(LeafPage(mut page): LeafPage) -> TestResult {
        let original_records: Vec<Row> = page.records().unwrap().cloned().collect();

        let new_id = page.page_id.wrapping_add(1);

        let Some((_split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let combined_records: Vec<Row> = page
            .records()
            .unwrap()
            .chain(new_page.records().unwrap())
            .cloned()
            .collect();

        assert_eq!(original_records, combined_records);
        TestResult::passed()
    }

    #[quickcheck]
    fn no_keys_or_chidren_lost_in_internal_split(
        InternalPage(mut page): InternalPage,
    ) -> TestResult {
        let original_keys: Vec<Key> = page.keys().unwrap().cloned().collect();
        let original_children: Vec<PageId> = page.children().unwrap().cloned().collect();

        let new_id = page.page_id.wrapping_add(1);

        let Some((split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let combined_keys: Vec<Key> = page
            .keys()
            .unwrap()
            .chain(&[split_key])
            .chain(new_page.keys().unwrap())
            .cloned()
            .collect();

        let combined_children: Vec<PageId> = page
            .children()
            .unwrap()
            .chain(new_page.children().unwrap())
            .cloned()
            .collect();

        assert_eq!(original_keys, combined_keys);
        assert_eq!(original_children, combined_children);
        TestResult::passed()
    }

    #[quickcheck]
    fn no_rows_lost_in_leaf_borrow(LeafPage(mut left): LeafPage) -> TestResult {
        let original_records: Vec<Row> = left.records().unwrap().cloned().collect();

        let right_id = left.page_id.wrapping_add(1);
        let Some((_separator, mut right)) = try_split(&mut left, right_id) else {
            return TestResult::discard();
        };

        // set the pointers
        left.set_next(Some(right.page_id)).unwrap();
        right.set_prev(Some(left.page_id)).unwrap();

        // borrow from right - discard results with empty borrows
        match left.leaf_borrow_from_right(&mut right) {
            Ok(_) => {}
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_))) => {
                return TestResult::discard();
            }
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };
        // borrow from left - empty borrow should be impossible
        match right.leaf_borrow_from_left(&mut left) {
            Ok(_) => {}
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        let new_records: Vec<Row> = left
            .records()
            .unwrap()
            .chain(right.records().unwrap())
            .cloned()
            .collect();

        assert_eq!(original_records, new_records);

        TestResult::passed()
    }

    #[quickcheck]
    fn no_keys_or_chidren_lost_in_internal_borrow(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let original_keys: Vec<Key> = left.keys().unwrap().cloned().collect();
        let original_children: Vec<PageId> = left.children().unwrap().cloned().collect();

        let right_id = left.page_id.wrapping_add(1);
        let Some((separator, mut right)) = try_split(&mut left, right_id) else {
            return TestResult::discard();
        };

        // borrow from the right
        let new_separator = match left.internal_borrow_from_right(&mut right, separator) {
            Ok(sep) => sep,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_))) => {
                return TestResult::discard();
            }
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        // left keys + the separator now in the parent + right keys == the original keys
        let keys_after: Vec<Key> = left
            .keys()
            .unwrap()
            .cloned()
            .chain([new_separator.clone()])
            .chain(right.keys().unwrap().cloned())
            .collect();
        let children_after: Vec<PageId> = left
            .children()
            .unwrap()
            .chain(right.children().unwrap())
            .copied()
            .collect();

        assert_eq!(original_keys, keys_after);
        assert_eq!(original_children, children_after);

        // now borrow from the left
        let new_separator = match right.internal_borrow_from_left(&mut left, new_separator) {
            Ok(sep) => sep,
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        // left keys + the separator now in the parent + right keys == the original keys
        let keys_after: Vec<Key> = left
            .keys()
            .unwrap()
            .cloned()
            .chain([new_separator])
            .chain(right.keys().unwrap().cloned())
            .collect();
        let children_after: Vec<PageId> = left
            .children()
            .unwrap()
            .chain(right.children().unwrap())
            .copied()
            .collect();

        assert_eq!(original_keys, keys_after);
        assert_eq!(original_children, children_after);
        TestResult::passed()
    }

    #[quickcheck]
    fn page_insert_returns_page_full_when_full(payload_len: u16) -> TestResult {
        let (schema, max_payload) = leaf_schema();
        let payload_len = payload_len as usize % (max_payload + 1);
        let row_with_key = |key: i64| {
            schema
                .validate_row(Row {
                    fields: vec![
                        RowValue::Integer(key),
                        RowValue::String("p".repeat(payload_len)),
                    ],
                })
                .unwrap()
        };

        // fill with same-size rows and increasing keys until the next one doesn't fit
        let mut page = Page::empty_leaf(child(1));
        let mut key = 0;
        while page.can_insert(row_with_key(key).as_ref()) {
            page.leaf_insert(row_with_key(key)).unwrap();
            key += 1;
        }

        // one more insert must be rejected, and must not change the page
        let before = page.clone();
        assert_matches!(
            page.leaf_insert(row_with_key(key)),
            Err(PageError::PageFull)
        );
        assert_unchanged(&before, &page);
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
        let validated_row = schema.validate_row(row).unwrap();
        if !page.can_insert(validated_row.as_ref()) {
            return TestResult::discard();
        }
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
            _ => unreachable!(),
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
            _ => unreachable!(),
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
        assert_eq!(page.find_child(&key), Some(expected_child(&page, &key)));
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_on_leaf_is_none(LeafPage(page): LeafPage, key: Key) -> TestResult {
        assert_eq!(page.find_child(&key), None);
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
            assert_eq!(page.find_child(&below), Some(children[0]), "below all");
        }

        // exactly equal to a separator -> the child to its right
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(
                page.find_child(k),
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
                    page.find_child(&Key::Integer(m)),
                    Some(children[i]),
                    "just below keys[{i}]"
                );
            }
        }

        // above every separator -> rightmost child
        // (any String sorts after every Integer; a longer string sorts after its prefix)
        let above = higher_key(last);
        assert_eq!(
            page.find_child(&above),
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
        let Some(last_row) = page.records().unwrap().last().cloned() else {
            return TestResult::discard();
        };

        // we took the last row, which should have the highest key in the page, then derive a higher
        // key that shouldn't be in the page
        let non_existant_key = higher_key(&ValidatedRow::from_row(last_row).primary_key());

        // take a snapshot
        let snapshot = page.clone();

        // try to remove a key that can't be there
        let result = page
            .leaf_remove(&non_existant_key)
            .expect("leaf_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        // if we had more than one record, let's remove one at random
        if page.records().is_some_and(|r| r.count() < 2) {
            return TestResult::passed();
        }

        let target_row = page
            .records()
            .unwrap()
            .nth(target as usize % page.num_items())
            .unwrap()
            .clone();
        let target_key = ValidatedRow::from_row(target_row.clone()).primary_key();

        let result = page
            .leaf_remove(&target_key)
            .expect("leaf_remove shouldn't fail");

        assert_eq!(result, Some(target_row));
        // take a snapshot
        let snapshot = page.clone();
        // now it's gone, let's try again
        let result = page
            .leaf_remove(&target_key)
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
        let Some(last_key) = page.keys().unwrap().last().cloned() else {
            return TestResult::discard();
        };

        // we took the last key, which should have the highest key in the page, then derive a higher
        // key that shouldn't be in the page
        let non_existant_key = higher_key(&last_key);

        // take a snapshot
        let snapshot = page.clone();

        // try to remove a key that can't be there
        let result = page
            .internal_remove(&non_existant_key)
            .expect("internal_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        // if we had more than one record, let's remove one at random
        if page.keys().is_some_and(|k| k.count() < 2) {
            return TestResult::passed();
        }

        let target_key = page
            .keys()
            .unwrap()
            .nth(target as usize % page.num_items())
            .unwrap()
            .clone();

        let result = page
            .internal_remove(&target_key)
            .expect("internal_remove shouldn't fail");

        assert_eq!(result.unwrap().0, target_key);

        // take a snapshot
        let snapshot = page.clone();

        // now it's gone, let's try again
        let result = page
            .internal_remove(&target_key)
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
        // Wrong page type is most senior error type, so an invalid key doesn't matter to this test
        let dummy_key = Key::Integer(0);

        let res1 = internal.leaf_remove(&dummy_key);
        let res2 = leaf.internal_remove(&dummy_key);

        assert_matches!(res1, Err(PageError::WrongPageType));
        assert_matches!(res2, Err(PageError::WrongPageType));

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
                page.find_child(&probe),
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
    fn borrows_fail_with_wrong_page_type(
        LeafPage(mut leaf): LeafPage,
        InternalPage(mut internal): InternalPage,
    ) -> TestResult {
        // This error is top dog so all other problems with this scenario don't matter
        let res1 = internal.leaf_borrow_from_left(&mut leaf);
        let res2 = internal.leaf_borrow_from_right(&mut leaf);
        let res3 = leaf.leaf_borrow_from_left(&mut internal);
        let res4 = leaf.leaf_borrow_from_right(&mut internal);

        assert_matches!(res1, Err(PageError::WrongPageType));
        assert_matches!(res2, Err(PageError::WrongPageType));
        assert_matches!(res3, Err(PageError::WrongPageType));
        assert_matches!(res4, Err(PageError::WrongPageType));

        let sep = Key::Integer(0);

        let res1 = internal.internal_borrow_from_left(&mut leaf, sep.clone());
        let res2 = internal.internal_borrow_from_right(&mut leaf, sep.clone());
        let res3 = leaf.internal_borrow_from_left(&mut internal, sep.clone());
        let res4 = leaf.internal_borrow_from_right(&mut internal, sep);

        assert_matches!(res1, Err(PageError::WrongPageType));
        assert_matches!(res2, Err(PageError::WrongPageType));
        assert_matches!(res3, Err(PageError::WrongPageType));
        assert_matches!(res4, Err(PageError::WrongPageType));

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_from_right_fails_when_dest_full(payload_sizes: Vec<u16>) -> TestResult {
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
        while left.can_insert(ValidatedRow::from_row(right_first.clone()).as_ref()) {
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
    fn leaf_borrows_from_left_fails_when_dest_full(payload_sizes: Vec<u16>) -> TestResult {
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
        if left.records().unwrap().count() < 2 {
            return TestResult::discard();
        }
        let left_last = left.records().unwrap().last().cloned().unwrap();

        // Refill the right page with the smallest possible rows until the left page's last
        // row no longer fits. Keys should all be higher than anything in left or right.
        let (schema, _) = leaf_schema();
        let mut key = i64::MAX;
        while right.can_insert(ValidatedRow::from_row(left_last.clone()).as_ref()) {
            let filler = Row {
                fields: vec![RowValue::Integer(key), RowValue::String(String::new())],
            };
            right
                .leaf_insert(schema.validate_row(filler).unwrap())
                .unwrap();
            key -= 1;
        }

        let (left_before, right_before) = (left.clone(), right.clone());
        let result = right.leaf_borrow_from_left(&mut left);

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

    #[quickcheck]
    fn internal_borrow_returns_error_with_empty_donor(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let mut right = internal_with_one_child(999);
        let sep = Key::Integer(0);

        let (before_left, before_right) = (left.clone(), right.clone());

        let result = left.internal_borrow_from_right(&mut right, sep.clone());

        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        // now switch sides
        let result = left.internal_borrow_from_left(&mut right, sep.clone());
        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        TestResult::passed()
    }

    #[quickcheck]
    fn internal_borrow_fails_when_separator_out_of_order(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let Some((_separator, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };

        let right_borrow_key = left.keys().unwrap().last().cloned().unwrap();
        let result = left.internal_borrow_from_right(&mut right, right_borrow_key.clone());
        assert_matches!(result, Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(k))) if k == right_borrow_key);

        let left_borrow_key = right.keys().unwrap().next().cloned().unwrap();
        let result = right.internal_borrow_from_left(&mut left, left_borrow_key.clone());
        assert_matches!(result, Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(k))) if k == left_borrow_key);

        TestResult::passed()
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

    #[quickcheck]
    fn internal_unsorted_keys_triggers_corrupt(
        InternalPage(mut page): InternalPage,
        i: u8,
    ) -> TestResult {
        if page.num_items() < 2 {
            return TestResult::discard();
        }
        let i = i as usize % (page.num_items() - 1);
        let j = i + 1;

        let PageBody::Internal { keys, .. } = &mut page.body else {
            unreachable!()
        };
        // swap two adjacent keys
        keys.swap(i, j);

        let result = page.serialize(&mut Vec::new());

        assert_matches!(result, Err(PageError::Corrupt { page_id: Some(pid), kind: CorruptionKind::UnsortedKeys { at } }) if pid == page.page_id() && at == j);
        TestResult::passed()
    }

    #[quickcheck]
    fn too_many_entries_triggers_exceed_capacity(mut page: Page) -> TestResult {
        // we want something that's at least half full
        if page.is_underfull() {
            return TestResult::discard();
        }

        // double the number of entries until we're overfull
        while page.free_space().is_some() {
            if page.is_leaf() {
                let PageBody::Leaf { records, .. } = &mut page.body else {
                    unreachable!()
                };
                let rec_clone = records.clone();
                records.extend(rec_clone.into_iter().cycle().take(5));
            } else {
                let PageBody::Internal { keys, children } = &mut page.body else {
                    unreachable!()
                };
                let key_clone = keys.clone();
                let num_keys = key_clone.len();
                let child_clone = children.clone();
                keys.extend(key_clone.clone().into_iter());
                children.extend(child_clone.clone().into_iter().take(num_keys));
            }
        }
        assert_eq!(page.free_space(), None);
        let result = page.as_raw_page();

        assert_matches!(
            result,
            Err(PageError::Corrupt {
                kind: CorruptionKind::ExceedsCapacity,
                ..
            })
        );
        TestResult::passed()
    }

    #[quickcheck]
    fn internal_invalid_key_triggers_corruption(
        InternalPage(page): InternalPage,
        idx: u8,
        bool_val: bool,
        float_val: f64,
    ) -> TestResult {
        let size = page.num_items();
        if size == 0 {
            return TestResult::discard();
        }
        let idx = idx as usize % size;
        let original = page.as_raw_page().expect("page should serialize");

        // the slot array follows the header and the (size + 1) children
        let slot_array = MAX_INTERNAL_HEADER_SIZE + (size + 1) * PAGE_ID_SIZE;
        let (offset, length) = get_slot(&original, slot_array, idx);

        let not_keys = [
            RowValue::Null,
            RowValue::Boolean(bool_val),
            RowValue::Float(float_val),
        ];

        for value in not_keys {
            let mut encoded = Vec::new();
            value.serialize(&mut encoded).unwrap();
            // write it at the start of the key's slot and shrink the slot to match;
            // skip values larger than the key they replace (e.g. a float over a 2-byte key)
            if encoded.len() > length as usize {
                continue;
            }
            let mut bytes = original;
            let start = offset as usize;
            bytes[start..start + encoded.len()].copy_from_slice(&encoded);
            set_slot(&mut bytes, slot_array, idx, offset, encoded.len() as u16);
            fix_checksum(&mut bytes);

            assert_matches!(
                Page::deserialize(&mut &bytes[..]),
                Err(PageError::Corrupt { kind: CorruptionKind::CorruptKey { slot }, .. }) if slot == idx
            );
        }

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_invalid_key_triggers_corruption(
        LeafPage(page): LeafPage,
        idx: u8,
        bool_val: bool,
        float_val: f64,
    ) -> TestResult {
        let size = page.num_items();
        if size == 0 {
            return TestResult::discard();
        }
        let idx = idx as usize % size;
        let original = page.as_raw_page().expect("page should serialize");

        // the slot array follows the header (account for `None` pointers)
        let none_pointers = [page.next().unwrap(), page.prev().unwrap()]
            .iter()
            .filter(|p| p.is_none())
            .count();
        let slot_array = MAX_LEAF_HEADER_SIZE - none_pointers * PAGE_ID_SIZE;
        let (offset, length) = get_slot(&original, slot_array, idx);

        let not_keys = [
            RowValue::Null,
            RowValue::Boolean(bool_val),
            RowValue::Float(float_val),
        ];

        for value in not_keys {
            let mut encoded = Vec::new();
            Row {
                fields: vec![value],
            }
            .serialize(&mut encoded)
            .unwrap();
            if encoded.len() > length as usize {
                continue;
            }
            let mut bytes = original;
            let start = offset as usize;
            bytes[start..start + encoded.len()].copy_from_slice(&encoded);
            set_slot(&mut bytes, slot_array, idx, offset, encoded.len() as u16);
            fix_checksum(&mut bytes);

            assert_matches!(
                Page::deserialize(&mut &bytes[..]),
                Err(PageError::Corrupt {
                    kind: CorruptionKind::InvalidKey(_),
                    ..
                }),
            );
        }

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_get_works(
        LeafPage(mut page): LeafPage,
        idx: u8,
        InternalPage(internal): InternalPage,
    ) -> TestResult {
        // first we check the internal page faults properly
        let result = internal.leaf_get(&Key::Integer(0));
        assert_matches!(result, Err(PageError::WrongPageType));

        let size = page.num_items();
        if size == 0 {
            // this leaf's empty, so we'll do the empty check here and return early
            let result = page.leaf_get(&Key::Integer(0));
            assert_matches!(result, Ok(None));
            return TestResult::passed();
        }

        // Now make a dummy empty leaf to check
        let empty = Page::empty_leaf(child(0));
        let result = empty.leaf_get(&Key::Integer(0));
        assert_matches!(result, Ok(None));

        // Now find a row that actually exists
        let idx = idx as usize % size;
        let expected = page.records().unwrap().nth(idx).cloned().unwrap();
        let key = Key::try_from(&expected.fields[0]).expect("key should be valid");

        let result = page.leaf_get(&key);
        assert_matches!(result, Ok(Some(got)) if got == &expected);

        // Now find create a key that's out of bounds
        let last_row = page.records().unwrap().last().cloned().unwrap();
        let last_key = Key::try_from(&last_row.fields[0]).expect("last key should be valid");

        let result = page.leaf_get(&higher_key(&last_key));
        assert_matches!(result, Ok(None));

        // Now we'll remove a random key and then try to find it
        let expected = page.records().unwrap().nth(idx).cloned().unwrap();
        let key = Key::try_from(&expected.fields[0]).expect("key should be valid");

        page.leaf_remove(&key).expect("remove should succeed");

        let result = page.leaf_get(&key);
        assert_matches!(result, Ok(None));

        // put it back and we should find it
        let valid = ValidatedRow::from_row(expected.clone());
        page.leaf_insert(valid).expect("insert should work");

        let result = page.leaf_get(&key);
        assert_matches!(result, Ok(Some(got)) if got == &expected);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_records_from_works(LeafPage(page): LeafPage, start_key: Key, pick: u8) -> TestResult {
        use std::collections::BTreeMap;

        // reference: the page's rows keyed by primary key
        let model: BTreeMap<Key, Row> = page
            .records()
            .unwrap()
            .map(|r| (Key::try_from(&r.fields[0]).unwrap(), r.clone()))
            .collect();

        // start keys to try: a random key (usually not on the page), an existing key,
        // and one just above an existing key
        let mut starts = vec![start_key];
        if !model.is_empty() {
            let existing = model
                .keys()
                .nth(pick as usize % model.len())
                .unwrap()
                .clone();
            starts.push(higher_key(&existing));
            starts.push(existing);
        }
        for start in &starts {
            let expected: Vec<&Row> = model.range(start..).map(|(_, row)| row).collect();
            let actual: Vec<&Row> = page.leaf_records_from(start).expect("leaf scan").collect();
            assert_eq!(expected, actual, "scan from {start:?}");
        }
        TestResult::passed()
    }

    #[test]
    fn leaf_records_from_iterator_works_after_dropping_key() {
        let page = fill_leaf(child(1), vec![1]);

        // grab the first key in the leaf
        let first_key =
            ValidatedRow::from_row(page.records().unwrap().next().cloned().unwrap()).primary_key();

        // build our row iterator from the first entry
        let mut iterator = page
            .leaf_records_from(&first_key)
            .expect("scan should succeed");

        // drop the reference key
        drop(first_key);

        // make sure iterator is still alive
        assert!(iterator.next().is_some());
    }

    #[quickcheck]
    fn leaf_borrow_with_parent_update_keeps_routing_correct(
        LeafPage(mut left): LeafPage,
    ) -> TestResult {
        // fix the page id
        left.page_id = child(1);
        // split into two pages
        let Some((sep, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };

        // make a new root that references both
        let mut root = Page::new_root(child(3), left.page_id, sep.clone(), right.page_id);

        let new_sep = if right.num_items() > 2 {
            // borrow from right
            left.leaf_borrow_from_right(&mut right)
                .expect("borrow shouldn't fail")
        } else if left.num_items() > 2 {
            // borrow from left
            right
                .leaf_borrow_from_left(&mut left)
                .expect("borrow shouldn't fail")
        } else {
            return TestResult::discard();
        };

        // update the root
        root.internal_replace_key(&sep, new_sep.clone())
            .expect("replace should work");
        // make sure the keys are correct
        assert_eq!(vec![&new_sep], root.keys().unwrap().collect::<Vec<&Key>>());

        for (page, other) in [(&left, &right), (&right, &left)] {
            for row in page.records().unwrap() {
                let key = Key::try_from(&row.fields[0]).unwrap();
                assert_eq!(
                    root.find_child(&key),
                    Some(page.page_id()),
                    "{key:?} routed to the wrong leaf"
                );
                assert_matches!(page.leaf_get(&key), Ok(Some(_))); // finds key in the correct child
                assert_matches!(other.leaf_get(&key), Ok(None)); // and it isn't also on the other page
            }
        }

        TestResult::passed()
    }

    #[test]
    fn internal_insert_too_long_key_works() {
        let test_boundary = MAX_INTERNAL_ENTRY_SIZE - PAGE_ID_SIZE - SLOT_ENTRY_SIZE - 1 - 2;

        let mut page = internal_with_one_child(1);

        let row = Row::try_from(vec![RowValue::String("a".repeat(test_boundary + 1))]).unwrap();

        let key = ValidatedRow::from_row(row).primary_key();

        let result = page.internal_insert(key, child(999));

        assert_matches!(result, Err(PageError::Schema(SchemaError::KeyTooLong(_))));

        let row = Row::try_from(vec![RowValue::String("a".repeat(test_boundary))]).unwrap();

        let key = ValidatedRow::from_row(row).primary_key();

        let result = page.internal_insert(key, child(999));

        assert_matches!(result, Ok(_));
    }

    #[test]
    fn internal_merge_with_min_internal_counting() {
        let sep = Key::Integer(1);
        let mut left = internal_with_one_child(1);
        let mut right = internal_with_one_child(2);

        let foo = left
            .internal_merge_from_right(&mut right, sep)
            .expect("merge should succeed");
        assert_eq!(foo, right.page_id);
        assert_eq!(left.keys().unwrap().count(), 1);
        assert_eq!(left.children().unwrap().count(), 2);
    }

    #[quickcheck]
    fn lsn_sequence_enforced(mut page: Page, new_lsn: u64) -> TestResult {
        if new_lsn == 0 {
            return TestResult::discard();
        }

        // make sure stales are detected
        match page.lsn() {
            Some(lsn) if lsn.get() < 2 => return TestResult::discard(),
            Some(lsn) => {
                let old_lsn = lsn;
                let new_lsn = Lsn::new(old_lsn.get().saturating_sub(1)).unwrap();
                let result = page.set_lsn(new_lsn);
                assert_matches!(result, Err(PageError::StaleLsnUpdate { old, new }) if old == old_lsn && new == new_lsn )
            }
            None => {}
        }

        // make sure updates are accepted
        match page.lsn() {
            Some(lsn) if lsn.get() == u64::MAX => return TestResult::discard(),
            Some(lsn) => {
                let old_lsn = lsn;
                let new_lsn = Lsn::new(old_lsn.get().saturating_add(1)).unwrap();
                page.set_lsn(new_lsn).expect("valid updates should take");
            }
            None => {
                page.set_lsn(Lsn::new(new_lsn).unwrap())
                    .expect("None overwrites should always succeed");
            }
        }

        assert_roundtrip(page);
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_index_round_trip(InternalPage(page): InternalPage) -> TestResult {
        let PageBody::Internal { keys, .. } = &page.body else {
            unreachable!()
        };
        let all_keys: Vec<&Key> = keys.iter().collect();

        for key in all_keys {
            let Some((child_idx, child_id)) = page.find_child_index(key) else {
                unreachable!()
            };
            let found = page.child_at(child_idx);
            assert_eq!(found, Some(child_id));
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn next_prev_getters_setters_only_work_on_leaves_and_leave_original_intact(
        mut page: Page,
    ) -> TestResult {
        if page.is_leaf() {
            return TestResult::discard();
        }
        let before = page.clone();

        let next = page.next();
        assert_matches!(next, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);

        let set_next = page.set_next(None);
        assert_matches!(set_next, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);

        let prev = page.prev();
        assert_matches!(prev, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);

        let set_prev = page.set_prev(None);
        assert_matches!(set_prev, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);
        TestResult::passed()
    }

    /// Runs a replacement that must fail, checks the page is byte-for-byte unchanged, returns the error.
    fn rejected_replace(page: &mut Page, old: &Key, new: Key) -> PageError {
        let before = page.clone();
        let err = page
            .internal_replace_key(old, new)
            .expect_err("replacement should be rejected");
        assert_unchanged(&before, page);
        err
    }

    #[test]
    fn internal_replace_key_rejections_leave_page_unchanged() {
        let int = Key::Integer;

        // keys [10, 20, 30]
        let mut page = Page::new_root(child(1), child(2), int(10), child(3));
        page.internal_insert(int(20), child(4)).unwrap();
        page.internal_insert(int(30), child(5)).unwrap();

        // new <= left neighbor
        for new in [10, 5] {
            assert_matches!(
                rejected_replace(&mut page, &int(20), int(new)),
                PageError::KeyNotInOrder(k) if k == int(new)
            );
        }

        // new >= right neighbor
        for new in [30, 35] {
            assert_matches!(
                rejected_replace(&mut page, &int(20), int(new)),
                PageError::KeyNotInOrder(k) if k == int(new)
            );
        }

        // edges: first key has no left neighbor, last key has no right neighbor
        assert_matches!(
            rejected_replace(&mut page, &int(10), int(20)),
            PageError::KeyNotInOrder(_)
        );
        assert_matches!(
            rejected_replace(&mut page, &int(30), int(20)),
            PageError::KeyNotInOrder(_)
        );

        // old key missing: both keys come back to the caller
        assert_matches!(
            rejected_replace(&mut page, &int(25), int(26)),
            PageError::MissingKey { search_key, new_key }
                if search_key == int(25) && new_key == int(26)
        );

        // sanity check: the fixture accepts a legal replacement
        page.internal_replace_key(&int(20), int(25))
            .expect("key should have been accepted");

        // larger key on a full page. Keys are padded_key(0), padded_key(2), ...,
        // so padded_key(1, ..) sorts between the first two.
        let mut full = fill_internal(child(100), vec![0]);
        let first = full.keys().unwrap().next().unwrap().clone();
        let bigger = padded_key(1, max_internal_len());
        assert!(
            full.free_space().unwrap() + Page::internal_entry_size(&first)
                < Page::internal_entry_size(&bigger),
            "fixture must be too full for the bigger key"
        );
        assert_matches!(
            rejected_replace(&mut full, &first, bigger),
            PageError::PageFull
        );

        // leaf page
        let mut leaf = Page::empty_leaf(child(50));
        assert_matches!(
            rejected_replace(&mut leaf, &int(1), int(2)),
            PageError::WrongPageType
        );
    }
}

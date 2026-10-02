use crate::commontypes::{Key, Lsn, PageId, PageLsn, SlotEntry};
use crate::commontypes::{PAGE_ID_SIZE, SLOT_ENTRY_SIZE};
use crate::schema::{Row, Schema, SchemaError, ValidatedRow};
use crate::traits::Serializable;
use std::cmp::Ordering;
use std::io::Cursor;
use std::ops::Range;

pub mod codec;
pub mod error;
mod internal;
pub mod invariants;
mod leaf;
mod meta;
#[cfg(test)]
pub(crate) mod tests;

pub use error::*;
use integer_encoding::VarInt;
pub(crate) use internal::ChildIndex;

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

pub type RawPage = [u8; PAGE_SIZE];
pub const EMPTY_RAW: RawPage = [0u8; PAGE_SIZE];

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
        schema: Schema,
        table_name: String,
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

    /// Converts `Page` of any type into a free `Page`
    pub fn make_free(&mut self) {
        self.body = PageBody::Free { next: None };
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
                PageBody::Meta {
                    schema, table_name, ..
                } => {
                    PAGE_ID_SIZE + // root_id
                    size_of::<u32>() + // page_count
                    1 + PAGE_ID_SIZE + // free_list_head
                    schema.encoded_size() + // schema
                    table_name.len().required_space() + // name varint size
                    table_name.len()
                }
            };
        PAGE_SIZE.checked_sub(used_size)
    }

    /// Returns `true` if the `Page` is underfull and can be merged with any other underfull `Page`
    pub fn is_underfull(&self) -> bool {
        match &self.body {
            PageBody::Leaf { .. } => self.entries_size() < LEAF_UNDERFULL_BYTES,
            PageBody::Internal { .. } => self.entries_size() < INTERNAL_UNDERFULL_BYTES,
            _ => false,
        }
    }

    ///  Returns true if a delete below this page makes everything above it safe.
    ///  - A borrow from below might replace a `Key` with a bigger one, so we first check
    ///    that there's room for the maximum size `Key` in the `Page`
    ///  - A merge below will remove a `Key` from the `Page` and we need to be sure
    ///    that doesn't make this `Page` underfull and require another a merge or borrow.
    pub(crate) fn is_delete_safe(&self) -> bool {
        match &self.body {
            PageBody::Internal { .. } => {
                self.free_space()
                    .is_some_and(|f| f >= MAX_INTERNAL_ENTRY_SIZE)
                    && self.entries_size() >= INTERNAL_UNDERFULL_BYTES + MAX_INTERNAL_ENTRY_SIZE
            }
            PageBody::Leaf { .. } => {
                self.entries_size() >= LEAF_UNDERFULL_BYTES + MAX_LEAF_ENTRY_SIZE
            }
            _ => false,
        }
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

    /// Thin dispatch to cover use cases for callers
    pub(crate) fn can_merge_with(&self, other: &Page, separator: &Key) -> bool {
        if self.is_internal() {
            self.internal_can_merge_with(other, separator)
        } else {
            self.leaf_can_merge_with(other)
        }
    }

    /// Thin dispatch to cover use cases for callers
    pub(crate) fn merge_from_right(
        &mut self,
        right: &mut Page,
        separator: Key,
    ) -> Result<PageId, PageError> {
        if self.is_internal() {
            self.internal_merge_from_right(right, separator)
        } else {
            self.leaf_merge_from_right(right)
        }
    }

    /// Thin dispatch to cover use cases for callers
    pub(crate) fn borrow_from_right(
        &mut self,
        right: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        if self.is_internal() {
            self.internal_borrow_from_right(right, parent_sep)
        } else {
            self.leaf_borrow_from_right(right)
        }
    }

    /// Used in tree rebalancing to avoid many hits of single borrows
    /// donor will provide entries until the receiver has an equal number of entries in it
    pub(crate) fn bulk_borrow_from_right(
        &mut self,
        right: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        let mut sep = self.borrow_from_right(right, parent_sep)?; // the first move has to succeed
        while self.entries_size() < right.entries_size() {
            match self.borrow_from_right(right, sep.clone()) {
                Ok(new_sep) => sep = new_sep,
                // the next entry wouldn't fit, or the donor is down to its minimum: stop here
                Err(PageError::PageFull | PageError::InvalidBorrow(_)) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(sep)
    }

    /// Thin dispatch to cover use cases for callers
    pub(crate) fn borrow_from_left(
        &mut self,
        left: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        if self.is_internal() {
            self.internal_borrow_from_left(left, parent_sep)
        } else {
            self.leaf_borrow_from_left(left)
        }
    }

    /// Used in tree rebalancing to avoid many hits of single borrows
    /// donor will provide entries until the receiver has an equal number of entries in it
    pub(crate) fn bulk_borrow_from_left(
        &mut self,
        left: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        let mut sep = self.borrow_from_left(left, parent_sep)?; // the first move has to succeed
        while self.entries_size() < left.entries_size() {
            match self.borrow_from_left(left, sep.clone()) {
                Ok(new_sep) => sep = new_sep,
                // the next entry wouldn't fit, or the donor is down to its minimum: stop here
                Err(PageError::PageFull | PageError::InvalidBorrow(_)) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(sep)
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

    pub(crate) fn replace(&mut self, other: Page) {
        *self = other;
    }
}

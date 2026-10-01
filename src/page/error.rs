use crate::commontypes::{Key, KeyError, Lsn, LsnError, PageId};
use crate::schema::{RowValue, RowValueError, SchemaError};
use std::ops::Range;
use thiserror::Error;

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
    PageFull, // TODO: have this variant return the row or key!
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
    BadSchema,
}

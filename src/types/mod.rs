mod ids;
mod lsn;
mod slot;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod generators;

pub use ids::{FrameId, PageId, SlotIndex, TableId};
pub use lsn::{Lsn, LsnError, PageLsn};
pub use slot::SlotEntry;

pub const SLOT_ENTRY_SIZE: usize = 4; // two u16s
pub const PAGE_ID_SIZE: usize = 8; // two u32s

use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::page::Page;

#[derive(Default, Debug)]
pub struct Frame {
    pub(crate) latch: RwLock<Option<Page>>, // guards the page contents
    pub(crate) pin_count: AtomicU32,
    pub(crate) dirty: AtomicBool,
    pub(crate) referenced: AtomicBool,
}

impl Frame {
    /// Returns whether anybody holds an active reference to the `Frame`
    pub(crate) fn is_pinned(&self) -> bool {
        self.pin_count.load(Ordering::Acquire) != 0
    }

    /// Returns whether anybody holds an active reference to the `Frame`
    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// Returns whether the frame was used since the last sweep, and clears the flag.
    /// Used for clock eviction
    pub(crate) fn take_referenced(&self) -> bool {
        self.referenced.swap(false, Ordering::Acquire)
    }
}

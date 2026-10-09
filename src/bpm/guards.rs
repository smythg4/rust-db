use std::ops::{Deref, DerefMut};
use std::sync::atomic::Ordering;
use std::sync::{RwLockReadGuard, RwLockWriteGuard};

use crate::bpm::BufferPoolManager;
use crate::page::Page;
use crate::traits::{DiskManager, EvictionPolicy};
use crate::types::FrameId;

pub struct PageReadGuard<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    bpm: &'a BufferPoolManager<Dm, Ep>,
    frame: FrameId,
    data: Option<RwLockReadGuard<'a, Option<Page>>>,
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> PageReadGuard<'a, Dm, Ep> {
    pub fn new(
        bpm: &'a BufferPoolManager<Dm, Ep>,
        frame: FrameId,
        slot: RwLockReadGuard<'a, Option<Page>>,
    ) -> Self {
        Self {
            bpm,
            frame,
            data: Some(slot),
        }
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> std::fmt::Debug for PageReadGuard<'a, Dm, Ep> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Read Guard")
            .field("Page", &self.data)
            .finish()
    }
}

pub struct PageWriteGuard<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    bpm: &'a BufferPoolManager<Dm, Ep>,
    frame: FrameId,
    data: Option<RwLockWriteGuard<'a, Option<Page>>>,
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> PageWriteGuard<'a, Dm, Ep> {
    pub fn new(
        bpm: &'a BufferPoolManager<Dm, Ep>,
        frame: FrameId,
        slot: RwLockWriteGuard<'a, Option<Page>>,
    ) -> Self {
        Self {
            bpm,
            frame,
            data: Some(slot),
        }
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> std::fmt::Debug for PageWriteGuard<'a, Dm, Ep> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Write Guard")
            .field("Page", &self.data)
            .finish()
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> Drop for PageReadGuard<'a, Dm, Ep> {
    fn drop(&mut self) {
        // Drop ordering would normally drop the ReadGuard after unpinning, this forces the latch drop first
        let _ = self.data.take();
        self.bpm.unpin_frame(self.frame);
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> Drop for PageWriteGuard<'a, Dm, Ep> {
    fn drop(&mut self) {
        // Frame is marked dirty only through `DerefMut`, so we avoid marking dirty for `WriteGuard`
        // acquisitions that didn't actually modify anything.

        // Drop ordering would normally drop the WriteGuard after unpinning, this forces the latch drop first
        let _ = self.data.take();
        self.bpm.unpin_frame(self.frame);
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> Deref for PageReadGuard<'a, Dm, Ep> {
    type Target = Page;
    fn deref(&self) -> &Self::Target {
        self.data
            .as_ref()
            .expect("guard is live until drop")
            .as_ref()
            .expect("pinned frame holds a page")
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> Deref for PageWriteGuard<'a, Dm, Ep> {
    type Target = Page;
    fn deref(&self) -> &Self::Target {
        self.data
            .as_ref()
            .expect("guard is live until drop")
            .as_ref()
            .expect("pinned frame holds a page")
    }
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> DerefMut for PageWriteGuard<'a, Dm, Ep> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        // mark the frame as dirty -- always assuming that someone's gonna change something
        // if they took a mutable reference.
        self.bpm.frames[self.frame]
            .dirty
            .store(true, Ordering::Release);

        self.data
            .as_mut()
            .expect("guard is live until drop")
            .as_mut()
            .expect("pinned frame holds a page")
    }
}

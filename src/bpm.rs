use crate::commontypes::{FrameId, PageId};
use crate::page::{EMPTY_RAW, Page, PageError};
use crate::traits::{DiskManager, Serializable};
use std::collections::HashMap;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use thiserror::Error;

pub const BUFFER_SIZE: usize = 512;

#[derive(Debug, Error)]
pub enum BpmError {
    // TODO: Write actual error types
    #[error("Unexpected error with a BPM operation")]
    Unexpected,
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error(transparent)]
    Page(#[from] PageError),
    #[error("No free frames available")]
    NoFreeFrames,
}
// Dummy structs to hold me over until I actually make them
struct Wal;
struct EvictionPolicy;

impl EvictionPolicy {
    /// TODO: This is just a placeholder until I figure this out...
    fn evict_frame(&self) -> Option<FrameId> {
        Some(FrameId::new(0))
    }
}
pub(crate) struct Frame {
    latch: RwLock<Option<Page>>, // guards the page contents
    pin_count: AtomicU32,
    dirty: AtomicBool,
}

struct BpmState {
    page_table: HashMap<PageId, FrameId>,
    free_frames: Vec<FrameId>,
    // TODO: Make real versions of these...
    eviction_policy: EvictionPolicy,
    _wal: Option<Wal>,
}

pub struct BufferPoolManager<Dm: DiskManager> {
    frames: Box<[Frame]>,
    persistant_layer: Dm,
    // TODO: Replace this with refined concurrency control
    big_dumb_lock: Arc<Mutex<BpmState>>,
}

impl<Dm: DiskManager> BufferPoolManager<Dm> {
    /// Returns a `ReadGuard` providing access to the `Page` associated with the given `PageId`
    pub fn fetch_read(&self, page_id: PageId) -> Result<PageReadGuard<'_, Dm>, BpmError> {
        // take the big dumb lock
        let mut state = self.big_dumb_lock.lock().unwrap();

        // see if the frame is in the active list, if not, call `load_page` to get it
        // loaded in the active list
        let frame = match state.page_table.get(&page_id) {
            Some(frame) => *frame,
            None => self.load_page(&mut state, page_id)?,
        };
        // pin this frame to avoid eviction from concurrent readers or writers
        let _ = self.pin_frame(frame);
        // drop the big dumb lock since we don't need it from here
        drop(state);
        // now grab a read lock for it
        let guard = self.frames[frame].latch.read().unwrap();
        let data = Some(guard);
        // build and return a `PageReadGuard` with reference to the underlying data
        Ok(PageReadGuard {
            bpm: self,
            frame,
            data,
        })
    }

    /// Returns a `WriteGuard` providing mutable access to the `Page` associated with the given `PageId`
    pub fn fetch_write(&self, page_id: PageId) -> Result<PageWriteGuard<'_, Dm>, BpmError> {
        // take the big dumb lock
        let mut state = self.big_dumb_lock.lock().unwrap();

        // see if the frame is in the active list, if not, call `load_page` to get it
        // loaded in the active list
        let frame = match state.page_table.get(&page_id) {
            Some(frame) => *frame,
            None => self.load_page(&mut state, page_id)?,
        };
        // pin this frame to avoid eviction from concurrent readers or writers
        let _ = self.pin_frame(frame);
        // drop the big dumb lock since we don't need it from here
        drop(state);
        // now grab a write lock for it
        let guard = self.frames[frame].latch.write().unwrap();
        let data = Some(guard);
        // build and return a `PageReadGuard` with reference to the underlying data
        Ok(PageWriteGuard {
            bpm: self,
            frame,
            data,
        })
    }

    /// If a `Page` isn't cached, we need to load it from the persistant layer. This will populate
    /// the frames table and return the new `FrameId` on success
    fn load_page(&self, state: &mut BpmState, page_id: PageId) -> Result<FrameId, BpmError> {
        // read raw data from the persistant layer
        let mut buf = EMPTY_RAW;
        self.persistant_layer.read_page(page_id, &mut buf)?;
        // deserialize it into our usable structure
        let page = Page::deserialize(&mut &buf[..])?;

        // try to grab a `FrameId` off the free_list, if one isn't available, go to the
        // eviction_policy, which will find a victim, flush it to disk if needed, and return
        // a fresh `FrameId` for us to use.
        let frame_id = match state.free_frames.pop() {
            Some(f) => f,
            None => {
                // ask the eviction policy for a victim frame
                let victim_id = state
                    .eviction_policy
                    .evict_frame()
                    .ok_or(BpmError::NoFreeFrames)?;
                let victim = &self.frames[victim_id];
                // if the frame was dirty, we need to write it to disk and flush before returning
                if victim.dirty.load(Ordering::Acquire) {
                    let guard = victim.latch.read().unwrap();
                    let pid = guard.as_ref().expect("data shouldn't be empty").page_id();
                    let raw = guard
                        .as_ref()
                        .expect("page shouldn't be empty")
                        .as_raw_page()?;
                    self.persistant_layer.write_page(pid, &raw)?;
                    self.persistant_layer.sync()?;
                    state.page_table.remove(&pid);
                }
                victim_id
            }
        };
        // Prepare the data to push into the `frames` list
        let frame_data = Some(page);
        // grab the frame sitting in this slot from the active list
        let frame = &self.frames[frame_id];
        // try to grab its write lock
        let mut slot = frame
            .latch
            .try_write()
            .expect("unpinned, unmapped frame has no readers");
        // update the `FrameData` inside
        *slot = frame_data;
        // make sure the frame isn't marked dirty
        frame.dirty.store(false, Ordering::Relaxed);

        // update the active page table
        state.page_table.insert(page_id, frame_id);

        // return the id
        Ok(frame_id)
    }

    /// Increments the `pin_count` for a given `FrameId`, called by acquiring read and write guards
    /// Returns the pin count before the increment
    fn pin_frame(&self, id: FrameId) -> u32 {
        let prev = self.frames[id].pin_count.fetch_add(1, Ordering::Release);
        debug_assert!(
            prev < u32::MAX,
            "pinned frame {id:?} with pin_count u32::MAX"
        );
        prev
    }
    /// Decrements the `pin_count` for a given `FrameId`, called by `drop` on read and write guards.
    /// Returns the pin count before the decrement
    fn unpin_frame(&self, id: FrameId) -> u32 {
        let prev = self.frames[id].pin_count.fetch_sub(1, Ordering::Release);
        debug_assert!(prev > 0, "unpinned frame {id:?} with pin_count 0");
        prev
    }
}

pub struct PageReadGuard<'a, Dm: DiskManager> {
    bpm: &'a BufferPoolManager<Dm>,
    frame: FrameId,
    data: Option<RwLockReadGuard<'a, Option<Page>>>,
}
pub struct PageWriteGuard<'a, Dm: DiskManager> {
    bpm: &'a BufferPoolManager<Dm>,
    frame: FrameId,
    data: Option<RwLockWriteGuard<'a, Option<Page>>>,
}

impl<'a, Dm: DiskManager> Drop for PageReadGuard<'a, Dm> {
    fn drop(&mut self) {
        // Drop ordering would normally drop the ReadGuard after unpinning, this forces the latch drop first
        let _ = self.data.take();
        self.bpm.unpin_frame(self.frame);
    }
}

impl<'a, Dm: DiskManager> Drop for PageWriteGuard<'a, Dm> {
    fn drop(&mut self) {
        // Drop ordering would normally drop the WriteGuard after unpinning, this forces the latch drop first
        let _ = self.data.take();
        self.bpm.unpin_frame(self.frame);
    }
}

impl<'a, Dm: DiskManager> Deref for PageReadGuard<'a, Dm> {
    type Target = Page;
    fn deref(&self) -> &Self::Target {
        self.data
            .as_ref()
            .expect("guard is live until drop")
            .as_ref()
            .expect("pinned frame holds a page")
    }
}

impl<'a, Dm: DiskManager> Deref for PageWriteGuard<'a, Dm> {
    type Target = Page;
    fn deref(&self) -> &Self::Target {
        self.data
            .as_ref()
            .expect("guard is live until drop")
            .as_ref()
            .expect("pinned frame holds a page")
    }
}

impl<'a, Dm: DiskManager> DerefMut for PageWriteGuard<'a, Dm> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.data
            .as_mut()
            .expect("guard is live until drop")
            .as_mut()
            .expect("pinned frame holds a page")
    }
}

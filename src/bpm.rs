use crate::commontypes::{FrameId, PageId};
use crate::page::{EMPTY_RAW, Page, PageError};

use crate::traits::{DiskManager, EvictionPolicy, Serializable};
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
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Page(#[from] PageError),
    #[error("No free frames available")]
    NoFreeFrames,
    #[error("Page Id didn't match on fetch. Expected: {expected}, Got: {got}")]
    WrongPage { expected: PageId, got: PageId },
    #[error("Tried to create a page that already exists {0}")]
    AlreadyExists(PageId),
}

// Dummy struct to hold me over until I actually make them
struct Wal;

#[derive(Default, Debug)]
pub struct Frame {
    latch: RwLock<Option<Page>>, // guards the page contents
    pin_count: AtomicU32,
    dirty: AtomicBool,
    referenced: AtomicBool,
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

struct BpmState<Ep: EvictionPolicy> {
    page_table: HashMap<PageId, FrameId>,
    free_frames: Vec<FrameId>,
    eviction_policy: Ep,
    // TODO: Make an actual Wal at some point
    _wal: Option<Wal>,
}

pub struct BufferPoolManager<Dm: DiskManager, Ep: EvictionPolicy> {
    frames: Box<[Frame]>,
    persistant_layer: Dm,
    // TODO: Replace this with refined concurrency control
    big_dumb_lock: Arc<Mutex<BpmState<Ep>>>,
}

impl<Dm: DiskManager, Ep: EvictionPolicy> BufferPoolManager<Dm, Ep> {
    pub fn new(disk: Dm, eviction_policy: Ep, pool_size: usize) -> Self {
        assert!(pool_size > 0, "can't have a 0 sized pool");
        let state = BpmState {
            page_table: HashMap::with_capacity(pool_size),
            free_frames: (0..pool_size).map(FrameId::new).collect(),
            eviction_policy,
            _wal: None,
        };
        Self {
            frames: (0..pool_size).map(|_| Frame::default()).collect(),
            persistant_layer: disk,
            big_dumb_lock: Arc::new(Mutex::new(state)),
        }
    }

    /// Creates a new `Page` based on a deserialized input and returns a WriteGuard for it
    pub fn new_page(&self, page: Page) -> Result<PageWriteGuard<'_, Dm, Ep>, BpmError> {
        let mut state = self.big_dumb_lock.lock().unwrap();
        if state.page_table.contains_key(&page.page_id()) {
            return Err(BpmError::AlreadyExists(page.page_id()));
        }
        let page_id = page.page_id();

        let frame_id = self.claim_frame(&mut state)?;

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
        frame.dirty.store(true, Ordering::Release);
        // update the active page table
        state.page_table.insert(page_id, frame_id);
        let _ = self.pin_frame(frame_id);
        // this has to be down here to prevent another thread from evicting this frame after we grab it, but before we pin it
        drop(state);
        Ok(PageWriteGuard {
            bpm: self,
            frame: frame_id,
            data: Some(slot),
        })
    }

    /// Returns a `ReadGuard` providing access to the `Page` associated with the given `PageId`
    pub fn fetch_read(&self, page_id: PageId) -> Result<PageReadGuard<'_, Dm, Ep>, BpmError> {
        // take the big dumb lock
        let mut state = self.big_dumb_lock.lock().unwrap();
        let frame = self.checkout_page(&mut state, page_id)?;
        // drop the big dumb lock since we don't need it from here
        drop(state);
        // now grab a read lock for it
        let guard = self.frames[frame].latch.read().unwrap();

        // make sure the page that we got matches the page we wanted.
        if let Some(pg) = guard.as_ref()
            && pg.page_id() != page_id
        {
            // no need for an explicit unpin call since the guard will drop here on error.
            return Err(BpmError::WrongPage {
                expected: page_id,
                got: pg.page_id(),
            });
        }
        let data = Some(guard);
        // build and return a `PageReadGuard` with reference to the underlying data
        Ok(PageReadGuard {
            bpm: self,
            frame,
            data,
        })
    }

    /// Returns a `WriteGuard` providing mutable access to the `Page` associated with the given `PageId`
    pub fn fetch_write(&self, page_id: PageId) -> Result<PageWriteGuard<'_, Dm, Ep>, BpmError> {
        // take the big dumb lock
        let mut state = self.big_dumb_lock.lock().unwrap();
        let frame = self.checkout_page(&mut state, page_id)?;
        // drop the big dumb lock since we don't need it from here
        drop(state);
        // now grab a write lock for it
        let guard = self.frames[frame].latch.write().unwrap();

        // make sure the page that we got matches the page we wanted.
        if let Some(pg) = guard.as_ref()
            && pg.page_id() != page_id
        {
            // no need for an explicit unpin call since the guard will drop here on error.
            return Err(BpmError::WrongPage {
                expected: page_id,
                got: pg.page_id(),
            });
        }
        let data = Some(guard);
        // build and return a `PageWriteGuard` with reference to the underlying data
        Ok(PageWriteGuard {
            bpm: self,
            frame,
            data,
        })
    }

    /// takes a `PageId` and `&mut BpmState` to provide a usable `FrameId` for the requested page.
    fn checkout_page(
        &self,
        state: &mut BpmState<Ep>,
        page_id: PageId,
    ) -> Result<FrameId, BpmError> {
        // see if the frame is in the active list, if not, call `load_page` to get it
        // loaded into the active list
        let frame = match state.page_table.get(&page_id) {
            Some(frame) => *frame,
            None => self.load_page(state, page_id)?,
        };
        // pin this frame to avoid eviction from concurrent readers or writers
        let _ = self.pin_frame(frame);
        Ok(frame)
    }

    /// Tries to grab a `FrameId` off the free_list, if one isn't available, go to the
    /// eviction_policy, which will find a victim, flush it to disk if needed, and return
    /// a fresh `FrameId` for us to use.
    fn claim_frame(&self, state: &mut BpmState<Ep>) -> Result<FrameId, BpmError> {
        match state.free_frames.pop() {
            Some(f) => Ok(f),
            None => {
                // ask the eviction policy for a victim frame
                let victim_id = state
                    .eviction_policy
                    .find_victim(&self.frames)
                    .ok_or(BpmError::NoFreeFrames)?;
                let victim = &self.frames[victim_id];
                if victim.is_pinned() {
                    return Err(BpmError::NoFreeFrames);
                }

                let guard = victim
                    .latch
                    .try_write()
                    .map_err(|_| BpmError::NoFreeFrames)?;
                let pid = guard.as_ref().expect("data shouldn't be empty").page_id();

                // if the frame was dirty, we need to write it to disk before returning
                if victim.dirty.load(Ordering::Acquire) {
                    log::debug!("Flushing dirty frame {} to disk", victim_id);
                    let raw = guard
                        .as_ref()
                        .expect("page shouldn't be empty")
                        .as_raw_page()?;
                    self.persistant_layer.write_page(pid, &raw)?;
                }
                log::debug!("Evicting page {pid} from the pool...");
                // remove the victim from the active page table
                state.page_table.remove(&pid);
                Ok(victim_id)
            }
        }
    }

    /// If a `Page` isn't cached, we need to load it from the persistant layer. This will populate
    /// the frames table and return the new `FrameId` on success
    fn load_page(&self, state: &mut BpmState<Ep>, page_id: PageId) -> Result<FrameId, BpmError> {
        // read raw data from the persistant layer
        let mut buf = EMPTY_RAW;
        self.persistant_layer.read_page(page_id, &mut buf)?;
        // deserialize it into our usable structure
        let page = Page::deserialize(&mut &buf[..])?;

        let frame_id = self.claim_frame(state)?;

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
        // first mark this `Frame` as referenced for the eviction policy
        self.frames[id].referenced.store(true, Ordering::Release);
        // then bump its pin_count
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

    /// Forces a flush to disk and call a sync
    pub fn flush_all(&self) -> Result<(), BpmError> {
        for frame in &self.frames {
            let guard = frame.latch.read().unwrap();
            if frame.dirty.load(Ordering::Acquire) {
                if let Some(page) = guard.as_ref() {
                    log::info!("flushing frame {:?} to disk", frame);
                    let raw = page.as_raw_page()?;
                    self.persistant_layer.write_page(page.page_id(), &raw)?;
                }
                frame.dirty.store(false, Ordering::Release);
            }
        }
        self.persistant_layer.sync()?;
        Ok(())
    }

    pub(crate) fn swap_file(&self, pages: Vec<Page>) -> Result<(), BpmError> {
        self.flush_all()?;
        let mut state = self.big_dumb_lock.lock().unwrap();
        self.persistant_layer.swap_file(pages)?;
        for frame in self.frames.iter() {
            assert!(!frame.is_pinned());
            *frame.latch.write().unwrap() = None;
            frame.dirty.store(false, Ordering::Release);
            frame.referenced.store(false, Ordering::Release);
        }
        state.page_table.clear();
        state.free_frames = (0..self.frames.len()).map(FrameId::new).collect();
        Ok(())
    }

    /// Flushes every dirty page and syncs. Call this for a clean shutdown and handle the error.
    pub fn close(self) -> Result<(), BpmError> {
        self.flush_all()
        // Drop runs afterwards, but finds nothing dirty
    }
}

impl<Dm: DiskManager, Ep: EvictionPolicy> Drop for BufferPoolManager<Dm, Ep> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return; // pages may be half-modified, and latches may be poisoned
        }
        if let Err(e) = self.flush_all() {
            log::error!("buffer pool flush on drop failed: {e}");
        }
    }
}

pub struct PageReadGuard<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    bpm: &'a BufferPoolManager<Dm, Ep>,
    frame: FrameId,
    data: Option<RwLockReadGuard<'a, Option<Page>>>,
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

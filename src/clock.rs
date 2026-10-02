use crate::bpm::Frame;
use crate::commontypes::FrameId;
use crate::traits::EvictionPolicy;

#[derive(Default)]
pub struct ClockEvictor {
    hand: usize,
}

impl EvictionPolicy for ClockEvictor {
    fn find_victim(&mut self, frames: &[Frame]) -> Option<FrameId> {
        // go through the `Frames` two times in case something cleared up in between
        for _ in 0..frames.len() * 2 {
            let id = self.hand;
            self.hand = (self.hand + 1) % frames.len();
            let frame = &frames[id];
            if frame.is_pinned() {
                continue; // currently in use, not a candidate
            }
            if frame.take_referenced() {
                continue; // used recently. clear the bit and give it a second chance
            }
            return Some(FrameId::new(id));
        }
        None
    }
}

/// This is a simple round robin eviction policy.
#[derive(Default)]
pub struct Replacer {
    hand: usize,
}

impl EvictionPolicy for Replacer {
    /// TODO: This is just a placeholder until I figure this out...
    fn find_victim(&mut self, frames: &[Frame]) -> Option<FrameId> {
        for _ in 0..frames.len() {
            let id = self.hand;
            self.hand = (self.hand + 1) % frames.len();
            if !frames[id].is_pinned() {
                return Some(FrameId::new(id));
            }
        }
        None // everything pinned → NoFreeFrames
    }
}

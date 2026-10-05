use crate::commontypes::PageId;
use crate::page::PageError;
use thiserror::Error;

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

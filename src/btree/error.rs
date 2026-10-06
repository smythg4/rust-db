use crate::bpm::BpmError;
use crate::page::PageError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BTreeError {
    #[error(transparent)]
    Bpm(#[from] BpmError),
    #[error(transparent)]
    Page(#[from] PageError),
}

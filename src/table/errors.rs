use crate::bpm::BpmError;
use crate::btree::BTreeError;
use crate::commontypes::TableId;
use crate::page::PageError;
use crate::schema::SchemaError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TableError {
    #[error(transparent)]
    Bpm(#[from] BpmError),
    #[error(transparent)]
    Page(#[from] PageError),
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error(transparent)]
    BTree(#[from] BTreeError),
    #[error("Table already exists: {0}")]
    AlreadyExists(TableId),
    #[error("Unexpected table error")]
    Unexpected,
}

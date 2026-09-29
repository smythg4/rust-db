use crate::bpm::{BpmError, PageWriteGuard};
use crate::page::{Page, PageError};
use crate::schema::ValidatedRow;
use crate::table::Table;
use crate::traits::{DiskManager, EvictionPolicy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BTreeError {
    #[error(transparent)]
    Bpm(#[from] BpmError),
    #[error(transparent)]
    Page(#[from] PageError),
}
pub struct BTree<'t, Dm: DiskManager, Ep: EvictionPolicy> {
    table: &'t Table<'t, Dm, Ep>,
}

impl<'t, 'bpm, Dm: DiskManager, Ep: EvictionPolicy> BTree<'t, Dm, Ep> {
    pub fn new(table: &'t Table<'bpm, Dm, Ep>) -> Self {
        Self { table }
    }

    pub fn insert(&self, row: ValidatedRow) -> Result<(), BTreeError> {
        let root_id = loop {
            let id = self.table.root_id().expect("failed to get root id");
            let guard = self.table.bpm.fetch_write(id)?;
            if self.table.root_id().expect("failed to get root id") == id {
                break id;
            }
            drop(guard);
        };

        let mut curr_page = self.table.bpm.fetch_write(root_id)?;
        let key = row.primary_key();
        let mut ancestors = Vec::new();

        // descend through the internal pages
        while curr_page.is_internal()
            && let Some(ch) = curr_page.find_child(&key)
        {
            ancestors.push(curr_page);
            curr_page = self.table.bpm.fetch_write(ch)?;
        }
        // now we know we're at a leaf page
        if curr_page.can_insert(row.as_ref()) {
            Ok(curr_page.leaf_insert(row)?)
        } else {
            // make sure the key isn't already in the leaf or we're splitting for no good reason
            if curr_page.leaf_get(&key)?.is_some() {
                return Err(PageError::DuplicateKey.into());
            }
            self.split_and_insert(curr_page, row, ancestors)
        }
    }

    pub fn split_and_insert(
        &self,
        mut page: PageWriteGuard<'_, Dm, Ep>,
        row: ValidatedRow,
        mut ancestors: Vec<PageWriteGuard<'_, Dm, Ep>>,
    ) -> Result<(), BTreeError> {
        let mut right = self.table.allocate().expect("table allocation failed");
        let (mut sep, new_page) = page.split_page(right.page_id())?;
        right.replace(new_page);

        let mut left_id = page.page_id();
        let mut right_id = right.page_id();
        drop(page);
        drop(right);
        loop {
            match ancestors.pop() {
                None => {
                    let mut new_root = self.table.allocate().expect("table allocation failed");
                    let root = Page::new_root(new_root.page_id(), left_id, sep.clone(), right_id);
                    new_root.replace(root);
                    self.table
                        .set_root_id(new_root.page_id())
                        .expect("setting root node failed");
                    if row.primary_key() >= sep {
                        self.table.bpm.fetch_write(right_id)?.leaf_insert(row)?;
                    } else {
                        self.table.bpm.fetch_write(left_id)?.leaf_insert(row)?;
                    }
                    return Ok(());
                }
                Some(mut parent) if parent.can_insert_separator(&sep) => {
                    parent.internal_insert(sep, right_id)?;
                    return Ok(());
                }
                Some(mut parent) => {
                    // parent was full...
                    let mut new_right_internal =
                        self.table.allocate().expect("table allocation failed");
                    let (promoted, page) = parent.split_page(new_right_internal.page_id())?;
                    new_right_internal.replace(page);
                    if sep > promoted {
                        new_right_internal.internal_insert(sep, right_id)?;
                    } else {
                        parent.internal_insert(sep, right_id)?;
                    }
                    left_id = parent.page_id();
                    right_id = new_right_internal.page_id();
                    sep = promoted;
                }
            }
        }
    }
}

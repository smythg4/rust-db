use crate::bpm::{PageReadGuard, PageWriteGuard};
use crate::btree::BTreeError;
use crate::btree::sibling::{Side, pick_sibling};
use crate::page::{ChildIndex, Page, PageError};
use crate::schema::{Key, Row, ValidatedRow};
use crate::table::Table;
use crate::traits::{DiskManager, EvictionPolicy};
use std::cmp::Ordering;
use std::ops::{Bound, RangeBounds};

pub struct BTree<'t, Dm: DiskManager, Ep: EvictionPolicy> {
    table: &'t Table<'t, Dm, Ep>,
}

impl<'t, 'bpm, Dm: DiskManager, Ep: EvictionPolicy> BTree<'t, Dm, Ep> {
    pub fn new(table: &'t Table<'bpm, Dm, Ep>) -> Self {
        Self { table }
    }

    pub fn get_root_write(&self) -> Result<PageWriteGuard<'_, Dm, Ep>, BTreeError> {
        // basically a CAS loop to make sure we have the absolute newest root_id
        let root_page = loop {
            let id = self.table.root_id().expect("failed to get root id");
            let guard = self.table.bpm.fetch_write(id)?;
            if self.table.root_id().expect("failed to get root id") == id {
                break guard;
            }
            drop(guard);
        };
        Ok(root_page)
    }

    pub fn get_root_read(&self) -> Result<PageReadGuard<'_, Dm, Ep>, BTreeError> {
        // basically a CAS loop to make sure we have the absolute newest root_id
        let root_page = loop {
            let id = self.table.root_id().expect("failed to get root id");
            let guard = self.table.bpm.fetch_read(id)?;
            if self.table.root_id().expect("failed to get root id") == id {
                break guard;
            }
            drop(guard);
        };
        Ok(root_page)
    }

    pub fn insert(&self, row: ValidatedRow) -> Result<(), BTreeError> {
        let mut curr_page = self.get_root_write()?;

        let key = row.primary_key();

        // a stack of parents traversed on the way down to finding the leaf page
        // for insertion. Holding `WriteGuards` along the way.
        let mut ancestors = Vec::new();

        // descend through the internal pages
        while curr_page.is_internal()
            && let Some(ch) = curr_page.find_child(&key)
        {
            ancestors.push(curr_page);
            curr_page = self.table.bpm.fetch_write(ch)?;

            // check if it's safe to split this page (if it's internal) or if a leaf
            // leaf can accept the entry. If either is true, then we don't need
            // the ancestors anymore and can release their guards by clearing the vec.
            if curr_page.is_split_safe() || curr_page.can_insert(row.as_ref()) {
                log::debug!("Clearing ancestors above page {}", curr_page.page_id());
                ancestors.clear(); // drops the guards: releases latches and unpins, root first
            }
        }

        // now we know we're at a leaf page
        if curr_page.can_insert(row.as_ref()) {
            Ok(curr_page.leaf_insert(row)?)
        } else {
            // make sure the key isn't already in the leaf or we're splitting for no good reason
            if curr_page.leaf_get(&key)?.is_some() {
                return Err(PageError::DuplicateKey.into());
            }
            log::info!("Splitting page: {}", curr_page.page_id());
            // we need to split the leaf and insert the entry into the proper side
            self.split_and_insert(curr_page, row, ancestors)
        }
    }

    pub fn split_and_insert(
        &self,
        mut page: PageWriteGuard<'_, Dm, Ep>,
        row: ValidatedRow,
        mut ancestors: Vec<PageWriteGuard<'_, Dm, Ep>>,
    ) -> Result<(), BTreeError> {
        // allocate a new page from the table
        let mut right = self.table.allocate().expect("table allocation failed");

        // split the leaf page, returning the separator key and a new page
        let (mut sep, new_page) = page.split_page(right.page_id())?;

        // overwrite the page we got from the allocator with the new_page from the split
        right.replace(new_page);

        // fix up the page pointers
        if let Some(n) = right.next()? {
            self.table
                .bpm
                .fetch_write(n)?
                .set_prev(Some(right.page_id()))?;
        }

        let mut left_id = page.page_id();
        let mut right_id = right.page_id();

        // insert the row into the appropriate leaf page
        if row.primary_key() <= sep {
            page.leaf_insert(row)?;
        } else {
            right.leaf_insert(row)?;
        }

        loop {
            match ancestors.pop() {
                None => {
                    // the root node was split, we need to allocate a new root
                    let mut new_root = self.table.allocate().expect("table allocation failed");
                    let root = Page::new_root(new_root.page_id(), left_id, sep.clone(), right_id);
                    new_root.replace(root);
                    self.table
                        .set_root_id(new_root.page_id())
                        .expect("setting root node failed");
                    return Ok(());
                }
                Some(mut parent) if parent.can_insert_separator(&sep) => {
                    parent.internal_insert(sep, right_id)?;
                    return Ok(());
                }
                Some(mut parent) => {
                    // parent was full...

                    // allocate a new page, this guard should drop at the end of the loop iteration
                    let mut new_right_internal =
                        self.table.allocate().expect("table allocation failed");

                    // split the parent page - same drill as before with the leaf
                    let (promoted, page) = parent.split_page(new_right_internal.page_id())?;
                    new_right_internal.replace(page);

                    // put the separator from below in the correct half
                    if sep > promoted {
                        new_right_internal.internal_insert(sep, right_id)?;
                    } else {
                        parent.internal_insert(sep, right_id)?;
                    }

                    // now we're moving up the tree with a new separator
                    left_id = parent.page_id();
                    right_id = new_right_internal.page_id();
                    sep = promoted;
                }
            }
        }
    }

    pub fn get(&self, key: &Key) -> Result<Option<Row>, BTreeError> {
        let mut curr_page = self.get_root_read()?;

        // descend through the internal pages
        while curr_page.is_internal()
            && let Some(ch) = curr_page.find_child(key)
        {
            curr_page = self.table.bpm.fetch_read(ch)?;
        }

        // now we're in a leaf
        Ok(curr_page.leaf_get(key)?.cloned())
    }

    pub fn get_range(
        &self,
        range: impl RangeBounds<Key>,
        filter_fn: impl Fn(&Row) -> bool,
    ) -> Result<Vec<Row>, BTreeError> {
        let (start, end) = (range.start_bound(), range.end_bound());
        let mut curr_page = self.get_root_read()?;

        while curr_page.is_internal() {
            let child = match start {
                Bound::Unbounded => curr_page
                    .children()
                    .expect("internal page")
                    .next()
                    .expect("internal page has a child"),
                Bound::Included(k) | Bound::Excluded(k) => {
                    &curr_page.find_child(k).expect("internal page")
                }
            };
            curr_page = self.table.bpm.fetch_read(*child)?;
        }

        let mut result = Vec::new();
        'leaves: loop {
            for row in curr_page.records().expect("leaf page") {
                if before_start(row, start) {
                    continue;
                }
                if past_end(row, end) {
                    break 'leaves;
                }
                if filter_fn(row) {
                    result.push(row.clone());
                }
            }
            match curr_page.next()? {
                None => break,
                Some(n) => curr_page = self.table.bpm.fetch_read(n)?,
            }
        }
        Ok(result)
    }

    pub fn get_all(&self, filter_fn: impl Fn(&Row) -> bool) -> Result<Vec<Row>, BTreeError> {
        self.get_range(.., filter_fn)
    }

    pub fn delete(&self, key: &Key) -> Result<Option<Row>, BTreeError> {
        let mut curr_page = self.get_root_write()?;
        let mut ancestors = Vec::new();

        // descend down to leaf page
        while curr_page.is_internal() {
            // push the path you took to get here onto an ancestors stack
            let (idx, child_id) = curr_page
                .find_child_index(key)
                .expect("internal page always has a child for any key");

            // check to make sure a delete below will stop at this page
            // this frees up the latches at all levels higher in the tree
            if curr_page.is_delete_safe() {
                ancestors.clear();
            }
            ancestors.push((curr_page, idx));
            curr_page = self.table.bpm.fetch_write(child_id)?;
        }

        // now we're at a leaf - if there was nothing to remove, return early
        let Some(row) = curr_page.leaf_remove(key)? else {
            return Ok(None);
        };

        self.rebalance(curr_page, ancestors)?;

        Ok(Some(row))
    }

    fn rebalance(
        &self,
        mut page: PageWriteGuard<'t, Dm, Ep>,
        mut ancestors: Vec<(PageWriteGuard<'t, Dm, Ep>, ChildIndex)>,
    ) -> Result<(), BTreeError> {
        while page.is_underfull() {
            let Some((mut parent, idx)) = ancestors.pop() else {
                break;
            };
            let sib = pick_sibling(&parent, idx).expect("non-root has a sibling");

            // name the pair left/right so there's only one path for each case
            let (mut left, mut right) = match sib.side {
                Side::Right => (page, self.table.bpm.fetch_write(sib.id)?),
                Side::Left => {
                    let right_id = page.page_id();
                    drop(page);
                    (
                        self.table.bpm.fetch_write(sib.id)?,
                        self.table.bpm.fetch_write(right_id)?,
                    )
                }
            };

            if left.can_merge_with(&right, &sib.separator) {
                log::info!("merging pages {} and {}", left.page_id(), right.page_id());
                // merge with the right neighbor
                let _freed_id = left.merge_from_right(&mut right, sib.separator.clone())?;
                // remove the separator from the parent
                let removed = parent.internal_remove(&sib.separator)?;
                debug_assert_eq!(removed.map(|(_, id)| id), Some(right.page_id()));

                // Fix up leaf pointers
                if left.is_leaf()
                    && let Some(n) = left.next()?
                {
                    self.table
                        .bpm
                        .fetch_write(n)?
                        .set_prev(Some(left.page_id()))?;
                }
                // free the merged page
                self.table.free(right).expect("failed to free right page");
                // move up the tree
                page = parent;
            } else {
                log::info!("{} borrowing from {}", left.page_id(), right.page_id());
                // we couldn't merge, so let's try to borrow
                let new_sep = match sib.side {
                    Side::Right => {
                        left.bulk_borrow_from_right(&mut right, sib.separator.clone())?
                    } // node = left, takes from the right
                    Side::Left => right.bulk_borrow_from_left(&mut left, sib.separator.clone())?, // node = right, takes from the left
                };
                parent.internal_replace_key(&sib.separator, new_sep)?;
                return Ok(());
            }
        }

        // the loop only leaves `page` as an internal page with no ancestors if it walked all the way up to the root
        if page.page_id() == self.table.root_id().expect("failed to fetch table root id")
            && page.is_internal()
            && page.num_items() == 0
        {
            let only_child = *page
                .children()
                .expect("internal")
                .next()
                .expect("one child");
            self.table
                .set_root_id(only_child)
                .expect("failed to set new root page"); // while still holding the old root's latch
            page.make_free();
            self.table
                .free(page)
                .expect("failed to free lonely root page");
        }
        Ok(())
    }
}

fn before_start(row: &Row, start: Bound<&Key>) -> bool {
    match start {
        Bound::Unbounded => false,
        Bound::Included(k) => row.cmp_key(k) == Ordering::Less,
        Bound::Excluded(k) => row.cmp_key(k) != Ordering::Greater,
    }
}

fn past_end(row: &Row, start: Bound<&Key>) -> bool {
    match start {
        Bound::Unbounded => false,
        Bound::Included(k) => row.cmp_key(k) == Ordering::Greater,
        Bound::Excluded(k) => row.cmp_key(k) != Ordering::Less,
    }
}

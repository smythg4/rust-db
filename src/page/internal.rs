use crate::page::*;
use crate::types::*;

/// Position of a child pointer in an internal page (0..=keys.len()).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ChildIndex(usize);

#[allow(dead_code)] // used only in tests right now
impl ChildIndex {
    pub(crate) fn new(i: usize) -> Self {
        ChildIndex(i)
    }
    /// The separator between this child and the next one.
    pub(crate) fn right_separator(self) -> KeyIndex {
        KeyIndex(self.0)
    }

    /// The separator between the previous child and this one (`None` for the first child).
    pub(crate) fn left_separator(self) -> Option<KeyIndex> {
        self.0.checked_sub(1).map(KeyIndex)
    }

    pub(crate) fn right_sibling(self) -> ChildIndex {
        ChildIndex(self.0 + 1)
    }

    pub(crate) fn left_sibling(self) -> Option<ChildIndex> {
        self.0.checked_sub(1).map(ChildIndex)
    }
}

/// Position of a separator key in an internal page (0..keys.len()).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct KeyIndex(usize);

#[allow(dead_code)] // used only in tests right now
impl KeyIndex {
    pub(crate) fn new(i: usize) -> Self {
        KeyIndex(i)
    }
}
impl Page {
    /// Returns the fully loaded cost for inserting a `Key` into an Internal page to include the slot entry
    /// and child page pointer
    pub(crate) fn internal_entry_size(k: &Key) -> usize {
        k.encoded_size() + SLOT_ENTRY_SIZE + PAGE_ID_SIZE
    }

    /// Inserts a separator `Key` and associated child `PageId` into an internal `Page`.
    /// Primarily called by parent datastructure (e.g. `BTree`) after splitting a `Page`
    /// lower in the tree.
    pub fn internal_insert(
        &mut self,
        separator: Key,
        right_child: PageId,
    ) -> Result<(), PageError> {
        if Self::internal_entry_size(&separator) > MAX_INTERNAL_ENTRY_SIZE {
            return Err(PageError::Schema(SchemaError::KeyTooLong(
                Self::internal_entry_size(&separator),
            )));
        }
        let can_insert = matches!(self.free_space(), Some(free_space) if free_space >= Self::internal_entry_size(&separator));

        let PageBody::Internal { keys, children } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };

        let insert_pos = match keys.binary_search(&separator) {
            Ok(_) => return Err(PageError::DuplicateKey),
            Err(i) => i,
        };

        if !can_insert {
            return Err(PageError::PageFull);
        }

        keys.insert(insert_pos, separator);
        children.insert(insert_pos + 1, right_child);
        self.debug_check_invariants("internal_insert");
        Ok(())
    }

    /// Returns `true` if `key` can fit in the `Page` without overflowing
    /// a `RawPage` when converted to raw bytes.
    pub fn can_insert_separator(&self, key: &Key) -> bool {
        if !self.is_internal() {
            return false;
        }
        // immediately return `false` if the page is already overfull
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        // new key will be encoded, child index and a corresponding slot index is allocated, all parts need to fit
        Self::internal_entry_size(key) <= free_space
    }

    /// Returns `true` if `Page` is internal and can accept the largest possible `Key` value
    pub(crate) fn is_split_safe(&self) -> bool {
        if !self.is_internal() {
            return false;
        }
        // immediately return `false` if the page is already overfull
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        MAX_INTERNAL_ENTRY_SIZE <= free_space
    }

    /// Tells you if this page is able to merge with `right`
    pub(crate) fn internal_can_merge_with(&self, right: &Page, separator: &Key) -> bool {
        if !self.is_internal() || !right.is_internal() {
            return false;
        }
        self.free_space()
            .is_some_and(|fs| fs >= right.entries_size() + Page::internal_entry_size(separator))
    }

    /// If the merge is legal, this will drain the right `Page` of any entries and merge them into
    /// this one. Right `Page` won't be affected in the event of failure. `separator` will be installed
    /// at the right point in the newly merged `Page`. Returns the `PageId` of the
    /// merged `Page` that can be recycled to a free list. `right` is converted into a free page
    /// upon success
    pub fn internal_merge_from_right(
        &mut self,
        right: &mut Page,
        separator: Key,
    ) -> Result<PageId, PageError> {
        if !self.is_internal() || !right.is_internal() {
            return Err(PageError::WrongPageType);
        }
        if !self.internal_can_merge_with(right, &separator) {
            return Err(PageError::PageFull);
        }
        let right_id = right.page_id;
        match (&mut self.body, &mut right.body) {
            (
                PageBody::Internal { keys, children },
                PageBody::Internal {
                    keys: right_keys,
                    children: right_children,
                },
            ) => {
                // reject if there's a key on the right and it's less than the separator
                // assumes keys list is sorted
                if right_keys.first().is_some_and(|k| k <= &separator) {
                    return Err(PageError::InvalidMerge(MergeFailReason::Keys));
                }
                // reject if there's a key on the left and it's greater than the separator
                // assumes keys list is sorted
                if keys.last().is_some_and(|k| k >= &separator) {
                    return Err(PageError::InvalidMerge(MergeFailReason::Keys));
                }
                let right_keys = right_keys.drain(..);
                let right_children = right_children.drain(..);

                // push the key in between the two lists
                keys.push(separator);
                // add the right side keys
                keys.extend(right_keys);
                // add the right side children
                children.extend(right_children);
                self.debug_check_invariants("internal_merge_from_right");
                // don't check invariants on the right page since it's drained.
                right.make_free();
                Ok(right_id)
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    /// Accepts a right_page and parent separator key. Will steal a child from the right page and insert that with
    /// the parent_sep into the `Page` (parent_sep, stolen_children). Returns new separator (first `Key` from right_page)
    /// on success. If the separator invariants aren't upheld, `PageError::InvalidBorrow(KeysOutOfOrder(parent_sep))` will
    /// return the value back to the caller, but will drop it in all other failure modes.
    pub fn internal_borrow_from_right(
        &mut self,
        right_page: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        if !self.is_internal() || !right_page.is_internal() {
            return Err(PageError::WrongPageType);
        }

        // Note: No way to ensure these are actually siblings apart from the separator check below.

        // make sure there's a key to borrow from the right page.
        let Some(borrow_key) = right_page.keys().unwrap().next() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                right_page.page_id,
            )));
        };

        // make sure the separator is valid (all keys in this page are less than, borrow key is greater)
        if self
            .keys()
            .unwrap()
            .last()
            .is_some_and(|r| r >= &parent_sep)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        if borrow_key <= &parent_sep {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        // no follow up right_page size check required since worst case right_page will end up with 0 keys and 1 child

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::internal_entry_size(&parent_sep))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the key/child pair!
        let PageBody::Internal { keys, children, .. } = &mut right_page.body else {
            unreachable!()
        };

        let stolen_key = keys.remove(0);
        let stolen_child = children.remove(0);

        // promoted key is the key we stole
        let promoted_key = stolen_key;

        let PageBody::Internal { keys, children, .. } = &mut self.body else {
            unreachable!()
        };
        // add the parent_sep to the keys list
        keys.push(parent_sep);
        // add the stolen child to the children list
        children.push(stolen_child);

        self.debug_check_invariants("internal_borrow_from_right");
        right_page.debug_check_invariants("internal_borrow_from_right");

        Ok(promoted_key)
    }

    /// Accepts a left and parent separator key. Will steal a child from the left page and insert that with
    /// the parent_sep into the `Page` (parent_sep, stolen_child). Returns new separator (last `Key` from left_page)
    /// on success. If the separator invariants aren't upheld, `PageError::InvalidBorrow(KeysOutOfOrder(parent_sep))` will
    /// return the value back to the caller, but will drop it in all other failure modes.
    pub fn internal_borrow_from_left(
        &mut self,
        left_page: &mut Page,
        parent_sep: Key,
    ) -> Result<Key, PageError> {
        if !self.is_internal() || !left_page.is_internal() {
            return Err(PageError::WrongPageType);
        }

        // Note: No way to ensure these are actually siblings apart from the separator check below.

        // make sure there's a key to borrow from the left page.
        let Some(borrow_key) = left_page.keys().unwrap().last() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                left_page.page_id,
            )));
        };

        // make sure the separator is valid (all keys in this page are greater than, borrow key is less than)
        if self
            .keys()
            .unwrap()
            .next()
            .is_some_and(|r| r <= &parent_sep)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        if borrow_key >= &parent_sep {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                parent_sep,
            )));
        }

        // no follow up left_page size check required since worst case left_page will end up with 0 keys and 1 child

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::internal_entry_size(&parent_sep))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the key/child pair!
        let PageBody::Internal { keys, children, .. } = &mut left_page.body else {
            unreachable!()
        };
        let stolen_key = keys.pop().expect("left_page has a key");
        let stolen_child = children.pop().expect("left_page has a child");

        // promoted key is the key we stole
        let promoted_key = stolen_key;

        let PageBody::Internal { keys, children, .. } = &mut self.body else {
            unreachable!()
        };
        // add the parent_sep to the keys list
        keys.insert(0, parent_sep);
        // add the stolen child to the children list
        children.insert(0, stolen_child);

        self.debug_check_invariants("internal_borrow_from_left");
        left_page.debug_check_invariants("internal_borrow_from_left");

        Ok(promoted_key)
    }

    pub(crate) fn internal_remove(
        &mut self,
        key: &Key,
    ) -> Result<Option<(Key, PageId)>, PageError> {
        // check before remove operation to make sure there's a child at `i + 1`
        self.debug_check_invariants("internal_remove");

        let PageBody::Internal { keys, children } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        let removed = match keys.binary_search(key) {
            Ok(i) => Some((keys.remove(i), children.remove(i + 1))),
            Err(_) => None,
        };
        self.debug_check_invariants("internal_remove");
        Ok(removed)
    }

    pub fn find_child(&self, search_key: &Key) -> Option<PageId> {
        self.find_child_index(search_key).map(|(_, id)| id)
    }

    pub(crate) fn find_child_index(&self, search_key: &Key) -> Option<(ChildIndex, PageId)> {
        let PageBody::Internal { keys, children } = &self.body else {
            return None;
        };
        let i = match keys.binary_search(search_key) {
            Ok(i) => i + 1, // equal to a separator: route right
            Err(i) => i,    // between separators
        };
        Some((ChildIndex(i), children[i]))
    }

    #[allow(dead_code)] // I'll need it for BTrees
    pub(crate) fn child_at(&self, idx: ChildIndex) -> Option<PageId> {
        let PageBody::Internal { children, .. } = &self.body else {
            return None;
        };
        children.get(idx.0).copied()
    }

    #[allow(dead_code)] // I'll need it for BTrees
    pub(crate) fn key_at(&self, idx: KeyIndex) -> Option<&Key> {
        let PageBody::Internal { keys, .. } = &self.body else {
            return None;
        };
        keys.get(idx.0)
    }

    /// Accepts a `Key` by value to replace a `&Key` in an internal `Page`
    /// Used to replace a separator after a borrow, keeping its children in place
    pub fn internal_replace_key(&mut self, old: &Key, new: Key) -> Result<(), PageError> {
        let fits = self.free_space().is_some_and(|free| {
            free + Self::internal_entry_size(old) >= Self::internal_entry_size(&new)
        });
        let PageBody::Internal { keys, .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        let idx = keys.binary_search(old).map_err(|_| PageError::MissingKey {
            search_key: old.clone(),
            new_key: new.clone(),
        })?;

        let above_left = idx == 0 || keys[idx - 1] < new;
        let below_right = idx + 1 == keys.len() || new < keys[idx + 1];
        if !(above_left && below_right) {
            return Err(PageError::KeyNotInOrder(new));
        }
        if !fits {
            return Err(PageError::PageFull);
        }

        keys[idx] = new;
        self.debug_check_invariants("internal_replace_key");
        Ok(())
    }
}

use crate::page::*;

impl Page {
    pub fn empty_leaf(page_id: PageId) -> Self {
        let body = PageBody::Leaf {
            records: Vec::new(),
            next: None,
            prev: None,
        };
        Self::empty_page(page_id, body)
    }

    /// Returns the fully loaded cost for inserting a `Row` into an Leaf page to include the slot entry
    pub(crate) fn leaf_entry_size(r: &Row) -> usize {
        r.encoded_size() + SLOT_ENTRY_SIZE
    }

    /// Returns `true` if `row` can fit in the `Page` without overflowing
    /// a `RawPage` when converted to raw bytes.
    pub fn can_insert(&self, row: &Row) -> bool {
        if !self.is_leaf() {
            return false;
        }
        // immediately return `false` if the page is already overfull
        let free_space = match self.free_space() {
            None => return false,
            Some(s) => s,
        };
        // new row will be encoded and a corresponding slot index is allocated, both parts need to fit
        Self::leaf_entry_size(row) <= free_space
    }

    /// Inserts a `ValidatedRow` into the `Page`. `ValidatedRow`s are those that are checked
    /// by a `Schema` type to ensure the `Row` conforms to the rules in the `Schema`.
    /// Will return `Error` if `Page` isn't of the `Leaf` variety, the `Key` is a duplicate,
    /// or the `Page` can't fit it.
    pub fn leaf_insert(&mut self, validated_row: ValidatedRow) -> Result<(), PageError> {
        let PageBody::Leaf { .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        let new_key = validated_row.primary_key();
        let can_insert = self.can_insert(validated_row.as_ref());
        let PageBody::Leaf { records, .. } = &mut self.body else {
            unreachable!();
        };

        let insert_pos = match records.binary_search_by(|r| r.cmp_key(&new_key)) {
            Ok(_) => return Err(PageError::DuplicateKey),
            Err(i) => i,
        };

        if !can_insert {
            return Err(PageError::PageFull);
        }
        records.insert(insert_pos, validated_row.into());
        self.debug_check_invariants("leaf_insert");
        Ok(())
    }

    /// Tells you if this page is able to merge with `right`
    pub(crate) fn leaf_can_merge_with(&self, right: &Page) -> bool {
        if !self.is_leaf() || !right.is_leaf() {
            return false;
        }
        self.free_space()
            .is_some_and(|fs| fs >= right.entries_size())
    }

    /// If the merge is legal, this will drain the right `Page` of any entries and merge them into
    /// this one. Right `Page` won't be affected in the event of failure. Returns the `PageId` of the
    /// merged `Page` that can be recycled to a free list. `right` is converted into a free page
    /// upon success
    pub fn leaf_merge_from_right(&mut self, right: &mut Page) -> Result<PageId, PageError> {
        if !self.is_leaf() || !right.is_leaf() {
            return Err(PageError::WrongPageType);
        }
        if !self.leaf_can_merge_with(right) {
            return Err(PageError::PageFull);
        }
        let right_id = right.page_id;

        match (&mut self.body, &mut right.body) {
            (
                PageBody::Leaf { records, next, .. },
                PageBody::Leaf {
                    records: right_records,
                    next: right_next,
                    ..
                },
            ) => {
                match *next {
                    Some(id) if id == right_id => {
                        if let Some(left_max) = records.last()
                            && right_records.first().is_some_and(|right_min| {
                                let left_key = Key::try_from(&left_max.fields[0]).unwrap();
                                let right_key = Key::try_from(&right_min.fields[0]).unwrap();
                                right_key <= left_key
                            })
                        {
                            return Err(PageError::InvalidMerge(MergeFailReason::Keys));
                        }
                        // leaf merge: append right_records, take over right_next
                        let right_records = right_records.drain(..);
                        records.extend(right_records);
                        *next = *right_next;
                        self.debug_check_invariants("leaf_merge_from_right");
                        // no need to check the right page since it's drained and now ready for re-issue
                        right.make_free();
                        Ok(right_id)
                    }
                    _ => Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch {
                        expected: *next,
                        got: Some(right_id),
                    })),
                }
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    /// Accepts a row from the right leaf neighbor. Returns the promoted Key to replace in the parent on success
    pub fn leaf_borrow_from_right(&mut self, right_page: &mut Page) -> Result<Key, PageError> {
        if !self.is_leaf() || !right_page.is_leaf() {
            return Err(PageError::WrongPageType);
        }

        // make sure the right page is actually THIS page's right page
        let right_page_id = right_page.page_id;
        match self.next()? {
            Some(np) if np == right_page_id => {}
            other => {
                return Err(PageError::InvalidBorrow(
                    BorrowFailReason::PointerMismatch {
                        expected: other,
                        got: Some(right_page_id),
                    },
                ));
            }
        };

        // make sure there's a record to borrow from the right page.
        let Some(borrow_rec) = right_page.records().unwrap().next() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                right_page.page_id,
            )));
        };

        let borrowed_key =
            Key::try_from(&borrow_rec.fields[0]).expect("stored row has a valid key");

        // make sure this key is greater than all this page's current keys
        if self
            .records()
            .unwrap()
            .last()
            .is_some_and(|r| r.cmp_key(&borrowed_key) != Ordering::Less)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                borrowed_key.clone(),
            )));
        }

        // need some spare rows to leave the right neighbor alive and parent pointers valid
        if right_page.records().is_some_and(|recs| recs.count() < 2) {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                right_page.page_id,
            )));
        }

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::leaf_entry_size(borrow_rec))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the record!
        let PageBody::Leaf { records, .. } = &mut right_page.body else {
            unreachable!()
        };
        let stolen_record = records.remove(0);

        // we checked that there were at least two records in the right_page
        let promoted_key = Key::try_from(&records.first().unwrap().fields[0]).unwrap();

        let PageBody::Leaf { records, .. } = &mut self.body else {
            unreachable!()
        };
        records.push(stolen_record);

        self.debug_check_invariants("leaf_borrow_from_right");
        right_page.debug_check_invariants("leaf_borrow_from_right");

        Ok(promoted_key)
    }

    /// Accepts a row from the left leaf neighbor. Returns the promoted Key to replace in the parent on success
    pub fn leaf_borrow_from_left(&mut self, left_page: &mut Page) -> Result<Key, PageError> {
        if !self.is_leaf() || !left_page.is_leaf() {
            return Err(PageError::WrongPageType);
        }

        // make sure the left page is actually THIS page's left page
        let left_page_id = left_page.page_id;
        match self.prev()? {
            Some(pp) if pp == left_page_id => {}
            other => {
                return Err(PageError::InvalidBorrow(
                    BorrowFailReason::PointerMismatch {
                        expected: other,
                        got: Some(left_page_id),
                    },
                ));
            }
        };

        // make sure there's a record to borrow from the left page.
        let Some(borrow_rec) = left_page.records().unwrap().last() else {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                left_page.page_id,
            )));
        };

        let borrowed_key =
            Key::try_from(&borrow_rec.fields[0]).expect("stored row has a valid key");

        // make sure this key is less than all this page's current keys
        if self
            .records()
            .unwrap()
            .next()
            .is_some_and(|r| r.cmp_key(&borrowed_key) != Ordering::Greater)
        {
            return Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                borrowed_key.clone(),
            )));
        }

        // need some spare rows to leave the left neighbor alive and parent pointers valid
        if left_page.records().is_some_and(|recs| recs.count() < 2) {
            return Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(
                left_page.page_id,
            )));
        }

        // make sure there's space in this page to put it
        if self
            .free_space()
            .is_none_or(|fs| fs < Page::leaf_entry_size(borrow_rec))
        {
            return Err(PageError::PageFull);
        }

        // now we're ready to actually take the record!
        let PageBody::Leaf { records, .. } = &mut left_page.body else {
            unreachable!()
        };
        let stolen_record = records.pop().expect("left page has entries");

        // in this case we use the borrowed_key as the promoted separator key
        let promoted_key = borrowed_key.clone();

        let PageBody::Leaf { records, .. } = &mut self.body else {
            unreachable!()
        };
        records.insert(0, stolen_record);

        self.debug_check_invariants("leaf_borrow_from_left");
        left_page.debug_check_invariants("leaf_borrow_from_left");

        Ok(promoted_key)
    }

    #[allow(dead_code)] // TODO: Remove the lint catcher later
    pub(crate) fn leaf_remove(&mut self, key: &Key) -> Result<Option<Row>, PageError> {
        let PageBody::Leaf { records, .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        let removed = match records.binary_search_by(|r| r.cmp_key(key)) {
            Ok(i) => Some(records.remove(i)),
            Err(_) => None,
        };
        self.debug_check_invariants("leaf_remove");
        Ok(removed)
    }

    pub(crate) fn next(&self) -> Result<Option<PageId>, PageError> {
        match self.body {
            PageBody::Leaf { next, .. } => Ok(next),
            _ => Err(PageError::WrongPageType),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn set_next(&mut self, new: Option<PageId>) -> Result<(), PageError> {
        match &mut self.body {
            PageBody::Leaf { next, .. } => {
                *next = new;
                Ok(())
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    pub(crate) fn prev(&self) -> Result<Option<PageId>, PageError> {
        match self.body {
            PageBody::Leaf { prev, .. } => Ok(prev),
            _ => Err(PageError::WrongPageType),
        }
    }
    #[allow(dead_code)] // TODO: Remove the lint catcher later
    pub(crate) fn set_prev(&mut self, new: Option<PageId>) -> Result<(), PageError> {
        match &mut self.body {
            PageBody::Leaf { prev, .. } => {
                *prev = new;
                Ok(())
            }
            _ => Err(PageError::WrongPageType),
        }
    }

    /// Finds a record in a leaf given a key to search. Returns `None` if the
    /// key isn't in the `Page`
    pub fn leaf_get(&self, key: &Key) -> Result<Option<&Row>, PageError> {
        let PageBody::Leaf { records, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(records
            .binary_search_by(|r| r.cmp_key(key))
            .ok()
            .map(|i| &records[i]))
    }

    /// Returns an iterator of records in a leaf that are >= `start_key`
    pub fn leaf_records_from<'a>(
        &'a self,
        start_key: &Key,
    ) -> Result<impl Iterator<Item = &'a Row> + use<'a>, PageError> {
        let PageBody::Leaf { records, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        let start = records.partition_point(|r| r.cmp_key(start_key) == Ordering::Less);
        Ok(records[start..].iter())
    }
}

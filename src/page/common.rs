#[cfg(test)]
mod tests {
    use crate::commontypes::*;
    use crate::page::*;
    use std::cmp::Ordering;

    use crate::commontypes::{Lsn, TableId};
    use crate::page::tests::generators::{InternalPage, LeafPage};
    use crate::page::tests::generators::{SchemaRowPair, SchemaWithRows};
    use crate::schema::RowValue;
    use crate::test_support::*;
    use quickcheck::TestResult;
    use quickcheck_macros::quickcheck;

    /// A two-row leaf (both pointers `None`) and the byte offset of its slot array.
    fn two_row_leaf() -> (RawPage, usize) {
        let (schema, _) = leaf_schema();
        let mut page = Page::empty_leaf(child(1));
        for key in [1, 2] {
            let row = Row {
                fields: vec![RowValue::Integer(key), RowValue::String("abc".into())],
            };
            page.leaf_insert(schema.validate_row(row).unwrap()).unwrap();
        }
        // header with both sibling pointers None: MAX_LEAF_HEADER_SIZE minus 8 bytes each
        (
            page.as_raw_page().unwrap(),
            MAX_LEAF_HEADER_SIZE - 2 * PAGE_ID_SIZE,
        )
    }

    fn get_slot(bytes: &RawPage, slot_array: usize, slot: usize) -> (u16, u16) {
        let at = slot_array + slot * SLOT_ENTRY_SIZE;
        (
            u16::from_be_bytes([bytes[at], bytes[at + 1]]),
            u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]),
        )
    }

    fn set_slot(bytes: &mut RawPage, slot_array: usize, slot: usize, offset: u16, length: u16) {
        let at = slot_array + slot * SLOT_ENTRY_SIZE;
        bytes[at..at + 2].copy_from_slice(&offset.to_be_bytes());
        bytes[at + 2..at + 4].copy_from_slice(&length.to_be_bytes());
    }

    fn fix_checksum(bytes: &mut RawPage) {
        let crc = Page::page_checksum(bytes);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
    }

    #[quickcheck]
    fn split_then_merge_roundtrip(mut page: Page) -> TestResult {
        if !page.is_leaf() && !page.is_internal() {
            return TestResult::discard();
        }
        let snapshot = page.clone();
        let new_id = page.page_id.wrapping_add(1);

        let Some((separator, mut new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let freed_id = match &page.body {
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut new_page, separator),
            PageBody::Leaf { .. } => page.leaf_merge_from_right(&mut new_page),
            _ => unreachable!(),
        }
        .unwrap();

        // make sure the PageId returned is the one we put in as the right page
        assert_eq!(freed_id, new_id);

        // make sure the page is identical to where we started after the roundtrip
        assert_unchanged(&page, &snapshot);

        // make sure the "freed" page was cleared of any entries
        assert_eq!(new_page.entries_size(), 0);

        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_with_invalid_pointers(LeafPage(mut page): LeafPage) -> TestResult {
        // if it's under full, we know it can fit into itself. We only want leaf pages for this test
        if !page.is_underfull() {
            return TestResult::discard();
        }

        // generate an id that's different from the given page's
        let page_id = page.page_id.wrapping_add(1);

        let Some((_, mut new_page)) = try_split(&mut page, page_id) else {
            return TestResult::discard();
        };

        let snapshot2 = new_page.clone();

        // now we have a new page that we're positive we can merge! Let's mess with it.
        page.set_next(None).unwrap();
        let snapshot1 = page.clone();
        let result = page.leaf_merge_from_right(&mut new_page);

        assert_matches!(
            result,
            Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch {
                expected: None,
                got: _,
            }))
        );
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&new_page, &snapshot2);

        // trying again but with a `Some` value
        page.set_next(Some(page.page_id)).unwrap();
        let snapshot1 = page.clone();
        let result = page.leaf_merge_from_right(&mut new_page);

        assert_matches!(
            result,
            Err(PageError::InvalidMerge(MergeFailReason::PointerMismatch {
                expected: Some(_),
                got: _,
            }))
        );
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&new_page, &snapshot2);

        // trying again with the right pointer value
        page.set_next(Some(page_id)).unwrap();

        let result = page
            .leaf_merge_from_right(&mut new_page)
            .expect("it should work this time");

        assert_eq!(result, page_id, "freed PageId doesn't match");

        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_with_duplicate_keys(mut page: Page) -> TestResult {
        // if it's under full, we know it can fit into itself
        if !page.is_underfull() {
            return TestResult::discard();
        }
        // make sure there's something in there
        if page.num_items() == 0 {
            return TestResult::discard();
        }

        let mut page2 = page.clone();
        // make up a key to serve as the separator
        let dummy_key = Key::Integer(0);

        let mut snapshot1 = page.clone();
        let snapshot2 = page2.clone();

        let result = match &mut page.body {
            PageBody::Leaf { next, .. } => {
                // tidy up the next pointer so we know that's not causing the error
                *next = Some(page2.page_id);
                // update the snapshot
                snapshot1 = page.clone();

                page.leaf_merge_from_right(&mut page2)
            }
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut page2, dummy_key),
            _ => unreachable!(),
        };

        assert_matches!(result, Err(PageError::InvalidMerge(MergeFailReason::Keys)));
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&page2, &snapshot2);
        TestResult::passed()
    }

    #[quickcheck]
    fn internal_merge_counts_the_separator_size(key_sizes: Vec<u16>) -> TestResult {
        if key_sizes.is_empty() {
            return TestResult::discard();
        }

        let left = fill_internal(child(1), key_sizes);
        let free = left.free_space().unwrap();

        // build the right page as empty as possible
        let right = internal_with_one_child(999);

        // separator_with generates a valid separator based on the left pages last entry. Discard results where
        // left page is empty
        let keys = left.keys().unwrap();

        let Some(Key::String(last)) = keys.last() else {
            return TestResult::discard();
        };
        let separator_with = |extra: usize| Key::String(format!("{last}{}", "z".repeat(extra)));

        // find the smallest key that won't fit
        let too_big = (1..)
            .find(|&n| Page::internal_entry_size(&separator_with(n)) > free)
            .unwrap();

        // make sure that the too big key results in a full page
        let (mut l, mut r) = (left.clone(), right.clone());
        let result = l.internal_merge_from_right(&mut r, separator_with(too_big));
        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&l, &left);
        assert_unchanged(&r, &right);

        // try a key that's one byte smaller and confirm that it fits
        if too_big > 1 {
            let (mut l, mut r) = (left.clone(), right.clone());
            let result = l.internal_merge_from_right(&mut r, separator_with(too_big - 1));
            assert!(
                result.is_ok(),
                "separator that fits was rejected: {result:?}"
            );
            assert!(l.as_raw_page().is_ok(), "merged page doesn't serialize");
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_when_not_small_enough(mut page: Page) -> TestResult {
        // make sure we a page whose data size is less than the free space available
        if page.entries_size() < page.free_space().unwrap_or(usize::MAX) {
            return TestResult::discard();
        }
        // clone that page, so we know that these can't safely merge
        let mut page2 = page.clone();

        // make up a key to serve as the separator
        let dummy_key = Key::Integer(0);

        let mut snapshot1 = page.clone();
        let snapshot2 = page2.clone();

        // this example will have duplicate keys and failed separator checks, but
        // the overfull check should occur first

        let result = match &mut page.body {
            PageBody::Leaf { next, .. } => {
                // make sure this page points to the correct way
                *next = Some(page2.page_id);
                // update the snapshot
                snapshot1 = page.clone();
                page.leaf_merge_from_right(&mut page2)
            }
            PageBody::Internal { .. } => page.internal_merge_from_right(&mut page2, dummy_key),
            _ => unreachable!(),
        };

        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&page, &snapshot1);
        assert_unchanged(&page2, &snapshot2);
        TestResult::passed()
    }

    #[quickcheck]
    fn merges_fail_with_dissimilar_pages(mut page1: Page, mut page2: Page) -> TestResult {
        // make sure both pages are underfull
        if !page1.is_underfull() || !page2.is_underfull() {
            return TestResult::discard();
        }
        let dummy_key = Key::String("dummy".into());
        let snapshot1 = page1.clone();
        let snapshot2 = page2.clone();
        // make sure the pages are of different types
        match (&mut page1.body, &mut page2.body) {
            (PageBody::Leaf { .. }, PageBody::Internal { .. }) => {
                let result = page1.leaf_merge_from_right(&mut page2);

                assert_matches!(result, Err(PageError::WrongPageType));
                assert_unchanged(&page1, &snapshot1);
                assert_unchanged(&page2, &snapshot2);
                TestResult::passed()
            }
            (PageBody::Internal { .. }, PageBody::Leaf { .. }) => {
                let result = page1.internal_merge_from_right(&mut page2, dummy_key);
                assert_matches!(result, Err(PageError::WrongPageType));
                assert_unchanged(&page1, &snapshot1);
                assert_unchanged(&page2, &snapshot2);
                TestResult::passed()
            }
            _ => TestResult::discard(),
        }
    }

    #[quickcheck]
    fn max_size_row_fits_after_leaf_split(payload_sizes: Vec<u16>, target: u8) -> TestResult {
        if payload_sizes.is_empty() {
            return TestResult::discard();
        }
        let (schema, max_payload) = leaf_schema();

        // helper closure to build a row from the schema
        let make_row = |key: i64, len: usize| {
            schema
                .validate_row(Row {
                    fields: vec![RowValue::Integer(key), RowValue::String("p".repeat(len))],
                })
                .unwrap()
        };

        let mut page = fill_leaf(child(1), payload_sizes);
        let count = page.records().unwrap().count() as i64;
        let new_id = page.page_id.wrapping_add(1);

        // split the page! (the new one will have a duplicate page id, but that's fine for the test)
        let (separator, mut right) = page.split_page(new_id).unwrap();

        // generate a key that lands between entries
        let key = (target as i64 % (count + 1)) * 10 - 5;
        // generate a max size payload
        let big_row = make_row(key, max_payload);

        let result = if Key::Integer(key) >= separator {
            right.leaf_insert(big_row)
        } else {
            page.leaf_insert(big_row)
        };

        assert!(
            result.is_ok(),
            "max size row didn't fit after split: {result:?}"
        );

        TestResult::passed()
    }

    #[quickcheck]
    fn max_size_key_fits_after_internal_split(key_sizes: Vec<u16>, target: u16) -> TestResult {
        if key_sizes.is_empty() {
            return TestResult::discard();
        }

        // largest string key whose internal entry still fits the limit
        let max_key_len = (0..MAX_INTERNAL_ENTRY_SIZE)
            .rev()
            .find(|&len| Page::internal_entry_size(&padded_key(0, len)) <= MAX_INTERNAL_ENTRY_SIZE)
            .unwrap();

        let mut page = fill_internal(child(1), key_sizes);
        let key_count = page.num_items();
        let new_id = page.page_id.wrapping_add(1);

        let Some((separator, mut right)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let big_key = padded_key((target as usize % (key_count + 1)) * 2 + 1, max_key_len);
        let new_child = child(1_000_000);
        let half = if big_key >= separator {
            &mut right
        } else {
            &mut page
        };

        let result = half.internal_insert(big_key.clone(), new_child);
        assert!(
            result.is_ok(),
            "max-size key didn't fit after split: {result:?}"
        );

        // the new child sits immediately right of the new key
        assert_eq!(half.find_child(&big_key), Some(new_child));

        TestResult::passed()
    }

    #[quickcheck]
    fn insertion_order_on_internals(mut keys: Vec<Key>) -> TestResult {
        use std::collections::BTreeMap;

        if keys.is_empty() {
            return TestResult::discard();
        }

        let first = keys.remove(0);

        let leftmost = child(2);
        let mut expected = BTreeMap::new();
        expected.insert(first.clone(), child(3));
        let mut page = Page::new_root(child(1), leftmost, first, child(3));

        for (i, key) in keys.into_iter().enumerate() {
            let right_child = child(4 + i as u32);
            let is_duplicate = expected.contains_key(&key);
            let is_full = Page::internal_entry_size(&key) > page.free_space().unwrap();
            let before = page.clone();

            let result = page.internal_insert(key.clone(), right_child);

            if is_duplicate {
                assert_matches!(result, Err(PageError::DuplicateKey));
                assert_unchanged(&before, &page);
            } else if is_full {
                assert_matches!(result, Err(PageError::PageFull));
                assert_unchanged(&before, &page);
            } else {
                assert!(result.is_ok(), "{result:?}");
                expected.insert(key.clone(), right_child);
                // routing the new key must land on the child inserted with it
                assert_eq!(page.find_child(&key), Some(right_child));
            }
        }

        let expected_keys: Vec<Key> = expected.keys().cloned().collect();
        assert_eq!(
            page.keys().unwrap().cloned().collect::<Vec<_>>(),
            expected_keys
        );
        // children: the untouched leftmost child, then each key's right child in key order
        let expected_children: Vec<PageId> = [leftmost]
            .into_iter()
            .chain(expected.values().copied())
            .collect();
        assert_eq!(
            page.children().unwrap().cloned().collect::<Vec<_>>(),
            expected_children
        );

        TestResult::passed()
    }

    #[quickcheck]
    fn insertion_order_on_leaves(SchemaWithRows(schema, rows): SchemaWithRows) -> TestResult {
        let mut page = Page::empty_leaf(child(1));
        use std::collections::BTreeMap;
        let mut expected = BTreeMap::new();

        for row in rows {
            let validated_row = schema.validate_row(row).unwrap();
            let key = validated_row.primary_key();

            let is_duplicate = expected.contains_key(&key);
            let is_full = !page.can_insert(validated_row.as_ref());

            let page_clone = page.clone();

            let result = page.leaf_insert(validated_row.clone());
            if is_duplicate {
                assert_matches!(result, Err(PageError::DuplicateKey));
                assert_unchanged(&page_clone, &page);
            } else if is_full {
                assert_matches!(result, Err(PageError::PageFull));
                assert_unchanged(&page_clone, &page);
            } else {
                assert!(result.is_ok());
                expected.insert(key, Row::from(validated_row));
            }
        }
        let actual_records: Vec<Row> = page.records().unwrap().cloned().collect();
        let expected_records: Vec<Row> = expected.into_values().collect();
        assert_eq!(expected_records, actual_records);
        TestResult::passed()
    }

    #[quickcheck]
    fn split_page_roundtrip(mut page: Page) -> TestResult {
        let new_id = page.page_id.wrapping_add(1);
        let Some((_, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        assert_roundtrip(page);
        assert_roundtrip(new_page);
        TestResult::passed()
    }

    #[quickcheck]
    fn split_page_key_in_right_spot(mut page: Page) -> TestResult {
        let new_id = page.page_id.wrapping_add(1);
        let Some((split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };
        match page.body {
            PageBody::Internal { keys, .. } => {
                assert!(!keys.is_empty());
                assert!(keys.iter().all(|k| k < &split_key));
                let new_keys: Vec<Key> = new_page.keys().unwrap().cloned().collect();
                assert!(!new_keys.is_empty());
                assert!(new_keys.iter().all(|k| k > &split_key));
            }
            PageBody::Leaf { records, .. } => {
                assert!(!records.is_empty());
                assert!(
                    records
                        .iter()
                        .all(|r| r.cmp_key(&split_key) == Ordering::Less)
                );
                let new_records: Vec<Row> = new_page.records().unwrap().cloned().collect();
                assert!(!new_records.is_empty());
                assert!(
                    new_records
                        .iter()
                        .all(|r| r.cmp_key(&split_key) != Ordering::Less)
                );
            }
            _ => unreachable!(),
        };
        TestResult::passed()
    }

    #[quickcheck]
    fn sibling_pointers_correct_after_split(LeafPage(mut page): LeafPage) -> TestResult {
        let original_id = page.page_id;
        let old_next = page.next().unwrap();
        let old_prev = page.prev().unwrap();
        let new_id = page.page_id.wrapping_add(1);
        let Some((_split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };
        match page.body {
            PageBody::Internal { .. } => unreachable!(),
            PageBody::Leaf { next, prev, .. } => {
                assert_eq!(prev, old_prev, "original page prev pointer wasn't retained");
                assert_eq!(
                    next,
                    Some(new_id),
                    "original page doesn't point to new page"
                );
                let new_prev = new_page.prev().unwrap();
                let new_next = new_page.next().unwrap();
                assert_eq!(
                    new_prev,
                    Some(original_id),
                    "new page prev pointer doesn't point to original page"
                );
                assert_eq!(
                    new_next, old_next,
                    "new page next pointer doesn't point to original page's original next"
                );
            }
            _ => unreachable!(),
        };
        TestResult::passed()
    }

    #[quickcheck]
    fn no_rows_lost_in_leaf_split(LeafPage(mut page): LeafPage) -> TestResult {
        let original_records: Vec<Row> = page.records().unwrap().cloned().collect();

        let new_id = page.page_id.wrapping_add(1);

        let Some((_split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let combined_records: Vec<Row> = page
            .records()
            .unwrap()
            .chain(new_page.records().unwrap())
            .cloned()
            .collect();

        assert_eq!(original_records, combined_records);
        TestResult::passed()
    }

    #[quickcheck]
    fn no_keys_or_chidren_lost_in_internal_split(
        InternalPage(mut page): InternalPage,
    ) -> TestResult {
        let original_keys: Vec<Key> = page.keys().unwrap().cloned().collect();
        let original_children: Vec<PageId> = page.children().unwrap().cloned().collect();

        let new_id = page.page_id.wrapping_add(1);

        let Some((split_key, new_page)) = try_split(&mut page, new_id) else {
            return TestResult::discard();
        };

        let combined_keys: Vec<Key> = page
            .keys()
            .unwrap()
            .chain(&[split_key])
            .chain(new_page.keys().unwrap())
            .cloned()
            .collect();

        let combined_children: Vec<PageId> = page
            .children()
            .unwrap()
            .chain(new_page.children().unwrap())
            .cloned()
            .collect();

        assert_eq!(original_keys, combined_keys);
        assert_eq!(original_children, combined_children);
        TestResult::passed()
    }

    #[quickcheck]
    fn no_rows_lost_in_leaf_borrow(LeafPage(mut left): LeafPage) -> TestResult {
        let original_records: Vec<Row> = left.records().unwrap().cloned().collect();

        let right_id = left.page_id.wrapping_add(1);
        let Some((_separator, mut right)) = try_split(&mut left, right_id) else {
            return TestResult::discard();
        };

        // set the pointers
        left.set_next(Some(right.page_id)).unwrap();
        right.set_prev(Some(left.page_id)).unwrap();

        // borrow from right - discard results with empty borrows
        match left.leaf_borrow_from_right(&mut right) {
            Ok(_) => {}
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_))) => {
                return TestResult::discard();
            }
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };
        // borrow from left - empty borrow should be impossible
        match right.leaf_borrow_from_left(&mut left) {
            Ok(_) => {}
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        let new_records: Vec<Row> = left
            .records()
            .unwrap()
            .chain(right.records().unwrap())
            .cloned()
            .collect();

        assert_eq!(original_records, new_records);

        TestResult::passed()
    }

    #[quickcheck]
    fn no_keys_or_chidren_lost_in_internal_borrow(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let original_keys: Vec<Key> = left.keys().unwrap().cloned().collect();
        let original_children: Vec<PageId> = left.children().unwrap().cloned().collect();

        let right_id = left.page_id.wrapping_add(1);
        let Some((separator, mut right)) = try_split(&mut left, right_id) else {
            return TestResult::discard();
        };

        // borrow from the right
        let new_separator = match left.internal_borrow_from_right(&mut right, separator) {
            Ok(sep) => sep,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_))) => {
                return TestResult::discard();
            }
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        // left keys + the separator now in the parent + right keys == the original keys
        let keys_after: Vec<Key> = left
            .keys()
            .unwrap()
            .cloned()
            .chain([new_separator.clone()])
            .chain(right.keys().unwrap().cloned())
            .collect();
        let children_after: Vec<PageId> = left
            .children()
            .unwrap()
            .chain(right.children().unwrap())
            .copied()
            .collect();

        assert_eq!(original_keys, keys_after);
        assert_eq!(original_children, children_after);

        // now borrow from the left
        let new_separator = match right.internal_borrow_from_left(&mut left, new_separator) {
            Ok(sep) => sep,
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        // left keys + the separator now in the parent + right keys == the original keys
        let keys_after: Vec<Key> = left
            .keys()
            .unwrap()
            .cloned()
            .chain([new_separator])
            .chain(right.keys().unwrap().cloned())
            .collect();
        let children_after: Vec<PageId> = left
            .children()
            .unwrap()
            .chain(right.children().unwrap())
            .copied()
            .collect();

        assert_eq!(original_keys, keys_after);
        assert_eq!(original_children, children_after);
        TestResult::passed()
    }

    #[quickcheck]
    fn page_insert_returns_page_full_when_full(payload_len: u16) -> TestResult {
        let (schema, max_payload) = leaf_schema();
        let payload_len = payload_len as usize % (max_payload + 1);
        let row_with_key = |key: i64| {
            schema
                .validate_row(Row {
                    fields: vec![
                        RowValue::Integer(key),
                        RowValue::String("p".repeat(payload_len)),
                    ],
                })
                .unwrap()
        };

        // fill with same-size rows and increasing keys until the next one doesn't fit
        let mut page = Page::empty_leaf(child(1));
        let mut key = 0;
        while page.can_insert(row_with_key(key).as_ref()) {
            page.leaf_insert(row_with_key(key)).unwrap();
            key += 1;
        }

        // one more insert must be rejected, and must not change the page
        let before = page.clone();
        assert_matches!(
            page.leaf_insert(row_with_key(key)),
            Err(PageError::PageFull)
        );
        assert_unchanged(&before, &page);
        TestResult::passed()
    }

    #[quickcheck]
    fn random_inputs_never_panic_on_deserialize(raw_page: [u8; PAGE_SIZE]) -> TestResult {
        let _ = Page::deserialize(&mut Cursor::new(raw_page));
        // this will likely always fail, but it's possible that quickcheck generated a valid random input
        // we only care that the call doesn't panic
        TestResult::passed()
    }

    #[quickcheck]
    fn any_burst_of_up_to_4_bytes_is_detected(
        page: Page,
        pos: u16,
        len: u8,
        masks: [u8; 4],
    ) -> TestResult {
        let mut bytes = page.as_raw_page().unwrap();

        // CRC32 is guaranteed to detect any change confined to 4 consecutive bytes.
        let len = 1 + len as usize % 4;
        let start = pos as usize % (PAGE_SIZE - len + 1);

        // XOR with a nonzero mask always changes the byte (writing a value might not).
        for (offset, mask) in masks.iter().take(len).enumerate() {
            bytes[start + offset] ^= (*mask).max(1);
        }

        let result = Page::deserialize(&mut &bytes[..]);
        assert!(
            matches!(
                result,
                Err(PageError::Corrupt {
                    kind: CorruptionKind::CheckSumMismatch,
                    ..
                })
            ),
            "{len} changed byte(s) at offset {start} not detected: {result:?}"
        );
        TestResult::passed()
    }

    #[quickcheck]
    fn mutated_pages_never_panic(page: Page, mutations: Vec<(u16, u8)>) -> TestResult {
        // initial input Page is valid, we deconstruct it into raw bytes and make random
        // modifications.
        let mut bytes = page.as_raw_page().unwrap();
        for &(pos, val) in &mutations {
            bytes[pos as usize % PAGE_SIZE] = val;
        }
        // reset the checksum so that the checksum trigger doesn't catch the error
        let crc = Page::page_checksum(&bytes);
        bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());

        let result = Page::deserialize(&mut &bytes[..]);

        if let Err(e) = &result {
            assert_matches!(e, PageError::Corrupt { .. });
        }

        if mutations.is_empty() {
            assert_unchanged(&result.unwrap(), &page);
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn accepted_pages_are_valid(page: Page, mutations: Vec<(u16, u8)>) -> TestResult {
        let mut bytes = page.as_raw_page().unwrap();
        for &(pos, val) in &mutations {
            bytes[pos as usize % PAGE_SIZE] = val;
        }
        match Page::deserialize(&mut &bytes[..]) {
            Ok(decoded) => {
                let again = decoded
                    .as_raw_page()
                    .expect("accepted page doesn't re-serialize");
                assert_unchanged(&Page::deserialize(&mut &again[..]).unwrap(), &decoded);
                TestResult::passed()
            }
            Err(_) => TestResult::discard(),
        }
    }

    #[quickcheck]
    fn duplicate_keys_trigger_error_on_insert(
        LeafPage(mut page): LeafPage,
        SchemaRowPair(schema, row): SchemaRowPair,
    ) -> TestResult {
        let validated_row = schema.validate_row(row).unwrap();
        if !page.can_insert(validated_row.as_ref()) {
            return TestResult::discard();
        }
        let _ = page.leaf_insert(validated_row.clone()); // this might error if the key already exists, but we're guaranteed to have it in there after calling it
        let result = page.leaf_insert(validated_row); // this is the check that matters

        assert_matches!(result, Err(PageError::DuplicateKey));
        TestResult::passed()
    }

    #[quickcheck]
    fn header_constant_is_right(page: Page) -> TestResult {
        let mut cursor = Cursor::new([0u8; PAGE_SIZE]);
        page.write_header(&mut cursor).unwrap();
        let mut none_count = 0;
        match page.body {
            PageBody::Internal { .. } => {
                assert_eq!(
                    cursor.position() + none_count * PAGE_ID_SIZE as u64,
                    MAX_INTERNAL_HEADER_SIZE as u64
                )
            }
            PageBody::Leaf { prev, next, .. } => {
                none_count += prev.is_none() as u64 + next.is_none() as u64;
                assert_eq!(
                    cursor.position() + none_count * PAGE_ID_SIZE as u64,
                    MAX_LEAF_HEADER_SIZE as u64
                )
            }
            _ => unreachable!(),
        };
        TestResult::passed()
    }

    #[test]
    fn basic_leaf_page_roundtrip() {
        let mut records: Vec<Row> = (1..=5)
            .map(|j| Row {
                fields: (0..10)
                    .map(|i| match i % 4 {
                        0 => RowValue::Integer(i * j),
                        1 => RowValue::String(format!("{i}{j}")),
                        2 => RowValue::Float(i as f64 / j as f64),
                        3 => RowValue::Null,
                        _ => unreachable!(),
                    })
                    .collect(),
            })
            .collect();
        records.sort_by_key(|r| Key::try_from(&r.fields[0]).unwrap());
        records.dedup_by_key(|r| Key::try_from(&r.fields[0]).unwrap());

        let lpage = Page {
            page_id: PageId::new(TableId::new(1), 10),
            last_update: PageLsn(Some(Lsn::new(10).unwrap())),
            body: PageBody::Leaf {
                records,
                next: Some(PageId::new(TableId::new(1), 3)),
                prev: None,
            },
        };
        let mut bytes = Vec::with_capacity(PAGE_SIZE);
        lpage.serialize(&mut bytes).unwrap();

        assert_eq!(bytes.len(), PAGE_SIZE);
        let deser = Page::deserialize(&mut Cursor::new(bytes)).unwrap();

        assert_unchanged(&deser, &lpage);
    }

    #[test]
    fn basic_internal_page_roundtrip() {
        let keys: Vec<Key> = (0..10).map(|i| Key::String(i.to_string())).collect();
        let children: Vec<PageId> = (0..11).map(|i| PageId::new(TableId::new(1), i)).collect();
        let ipage = Page {
            page_id: PageId::new(TableId::new(10), 10),
            last_update: PageLsn(Some(Lsn::new(10).unwrap())),
            body: PageBody::Internal { keys, children },
        };
        assert_roundtrip(ipage);
    }

    #[quickcheck]
    fn free_space_works(page: Page) -> TestResult {
        let Some(free_space) = page.free_space() else {
            return TestResult::discard();
        };

        let mut raw_page = Cursor::new([0u8; PAGE_SIZE]);
        page.write_header(&mut raw_page).unwrap();
        let header_end = raw_page.position() as usize;
        let (slot_offset, data_offset) = page.write_body(&mut raw_page, header_end).unwrap();

        let actual_free_space = data_offset - slot_offset;

        let max_header = match page.body {
            PageBody::Internal { .. } => MAX_INTERNAL_HEADER_SIZE,
            PageBody::Leaf { .. } => MAX_LEAF_HEADER_SIZE,
            _ => unreachable!(),
        };
        assert_eq!(free_space + (max_header - header_end), actual_free_space);
        TestResult::passed()
    }

    #[quickcheck]
    fn page_quickcheck_roundtrip(page: Page) -> TestResult {
        assert_roundtrip(page);
        TestResult::passed()
    }

    /// Linear scan to find appropriate child entry. First index where separator <= key
    fn expected_child(page: &Page, key: &Key) -> PageId {
        let keys: Vec<_> = page.keys().unwrap().cloned().collect();
        let idx = keys.iter().filter(|k| *k <= key).count();
        page.children().unwrap().nth(idx).copied().unwrap()
    }

    #[quickcheck]
    fn find_child_matches_linear_scan(InternalPage(page): InternalPage, key: Key) -> TestResult {
        assert_eq!(page.find_child(&key), Some(expected_child(&page, &key)));
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_on_leaf_is_none(LeafPage(page): LeafPage, key: Key) -> TestResult {
        assert_eq!(page.find_child(&key), None);
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_boundaries(InternalPage(page): InternalPage) -> TestResult {
        let keys: Vec<Key> = page.keys().unwrap().cloned().collect();
        let children: Vec<PageId> = page.children().unwrap().cloned().collect();

        // check that we have at least one key
        let (Some(first), Some(last)) = (keys.first(), keys.last()) else {
            return TestResult::discard();
        };

        // below every separator -> leftmost child
        // (Integer(i64::MIN) is the smallest possible Key; skip if it's already a separator)
        let below = Key::Integer(i64::MIN);
        if &below < first {
            assert_eq!(page.find_child(&below), Some(children[0]), "below all");
        }

        // exactly equal to a separator -> the child to its right
        for (i, k) in keys.iter().enumerate() {
            assert_eq!(
                page.find_child(k),
                Some(children[i + 1]),
                "equal to keys[{i}]"
            );
        }

        // one below an integer separator -> the child to its left
        for (i, k) in keys.iter().enumerate() {
            if let Key::Integer(n) = k
                && let Some(m) = n.checked_sub(1)
            {
                assert_eq!(
                    page.find_child(&Key::Integer(m)),
                    Some(children[i]),
                    "just below keys[{i}]"
                );
            }
        }

        // above every separator -> rightmost child
        // (any String sorts after every Integer; a longer string sorts after its prefix)
        let above = higher_key(last);
        assert_eq!(
            page.find_child(&above),
            children.last().copied(),
            "above all"
        );

        TestResult::passed()
    }

    #[quickcheck]
    fn remove_then_insert_leaf_stays_same(LeafPage(mut page): LeafPage, target: u8) -> TestResult {
        if page.records().unwrap().next().is_none() {
            return TestResult::discard();
        }
        let snapshot = page.clone();
        let target = target as usize % page.records().unwrap().count();
        let target_row = page.records().unwrap().nth(target).cloned().unwrap();
        let target_key = Key::try_from(&target_row.fields[0]).unwrap();

        // remove the target key
        let returned_row = page
            .leaf_remove(&target_key)
            .unwrap()
            .expect("key is on the page");

        // make sure what came out is what we expected
        assert_eq!(target_row, returned_row);

        // insert back what was returned
        page.leaf_insert(ValidatedRow::from_row(returned_row))
            .unwrap();

        // make sure nothing changed
        assert_unchanged(&snapshot, &page);

        // remove it again
        let returned_row = page
            .leaf_remove(&target_key)
            .unwrap()
            .expect("key is on the page");

        // make sure what came out is what we expected
        assert_eq!(target_row, returned_row);

        // insert back what was returned - again
        page.leaf_insert(ValidatedRow::from_row(returned_row))
            .unwrap();

        // make sure nothing changed
        assert_unchanged(&snapshot, &page);
        TestResult::passed()
    }

    #[quickcheck]
    fn remove_then_insert_internal_stays_same(
        InternalPage(mut page): InternalPage,
        target: u8,
    ) -> TestResult {
        if page.keys().unwrap().next().is_none() || page.children().unwrap().next().is_none() {
            return TestResult::discard();
        }
        let snapshot = page.clone();

        let target = target as usize % page.keys().unwrap().count();
        let target_key = page.keys().unwrap().nth(target).cloned().unwrap();
        let target_child = page.children().unwrap().nth(target + 1).cloned().unwrap();

        // try to remove the key
        let (returned_key, returned_child) = page
            .internal_remove(&target_key)
            .unwrap()
            .expect("key is on the page");
        // make sure the returned values match
        assert_eq!(target_key, returned_key);
        assert_eq!(target_child, returned_child);

        // insert what was returned
        page.internal_insert(returned_key, returned_child).unwrap();

        // make sure the page is back to its original shape
        assert_unchanged(&snapshot, &page);

        // remove it again
        let (returned_key, returned_child) = page
            .internal_remove(&target_key)
            .unwrap()
            .expect("key is on the page");
        // make sure the returned values match
        assert_eq!(target_key, returned_key);
        assert_eq!(target_child, returned_child);

        // insert what was returned - again
        page.internal_insert(returned_key, returned_child).unwrap();

        // make sure the page is back to its original shape
        assert_unchanged(&snapshot, &page);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_remove_returns_none_when_its_not_there(
        LeafPage(mut page): LeafPage,
        target: u8,
    ) -> TestResult {
        let Some(last_row) = page.records().unwrap().last().cloned() else {
            return TestResult::discard();
        };

        // we took the last row, which should have the highest key in the page, then derive a higher
        // key that shouldn't be in the page
        let non_existant_key = higher_key(&ValidatedRow::from_row(last_row).primary_key());

        // take a snapshot
        let snapshot = page.clone();

        // try to remove a key that can't be there
        let result = page
            .leaf_remove(&non_existant_key)
            .expect("leaf_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        // if we had more than one record, let's remove one at random
        if page.records().is_some_and(|r| r.count() < 2) {
            return TestResult::passed();
        }

        let target_row = page
            .records()
            .unwrap()
            .nth(target as usize % page.num_items())
            .unwrap()
            .clone();
        let target_key = ValidatedRow::from_row(target_row.clone()).primary_key();

        let result = page
            .leaf_remove(&target_key)
            .expect("leaf_remove shouldn't fail");

        assert_eq!(result, Some(target_row));
        // take a snapshot
        let snapshot = page.clone();
        // now it's gone, let's try again
        let result = page
            .leaf_remove(&target_key)
            .expect("leaf_remove shouldn't fail");
        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        TestResult::passed()
    }

    #[quickcheck]
    fn internal_remove_returns_none_when_its_not_there(
        InternalPage(mut page): InternalPage,
        target: u8,
    ) -> TestResult {
        let Some(last_key) = page.keys().unwrap().last().cloned() else {
            return TestResult::discard();
        };

        // we took the last key, which should have the highest key in the page, then derive a higher
        // key that shouldn't be in the page
        let non_existant_key = higher_key(&last_key);

        // take a snapshot
        let snapshot = page.clone();

        // try to remove a key that can't be there
        let result = page
            .internal_remove(&non_existant_key)
            .expect("internal_remove shouldn't fail");

        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        // if we had more than one record, let's remove one at random
        if page.keys().is_some_and(|k| k.count() < 2) {
            return TestResult::passed();
        }

        let target_key = page
            .keys()
            .unwrap()
            .nth(target as usize % page.num_items())
            .unwrap()
            .clone();

        let result = page
            .internal_remove(&target_key)
            .expect("internal_remove shouldn't fail");

        assert_eq!(result.unwrap().0, target_key);

        // take a snapshot
        let snapshot = page.clone();

        // now it's gone, let's try again
        let result = page
            .internal_remove(&target_key)
            .expect("internal_remove shouldn't fail");
        assert_eq!(result, None);
        assert_unchanged(&snapshot, &page);

        TestResult::passed()
    }

    #[quickcheck]
    fn removes_fail_with_wrong_page_type(
        LeafPage(mut leaf): LeafPage,
        InternalPage(mut internal): InternalPage,
    ) -> TestResult {
        // Wrong page type is most senior error type, so an invalid key doesn't matter to this test
        let dummy_key = Key::Integer(0);

        let res1 = internal.leaf_remove(&dummy_key);
        let res2 = leaf.internal_remove(&dummy_key);

        assert_matches!(res1, Err(PageError::WrongPageType));
        assert_matches!(res2, Err(PageError::WrongPageType));

        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_after_internal_remove(
        InternalPage(mut page): InternalPage,
        target: u8,
        probes: Vec<Key>,
    ) -> TestResult {
        let old_keys: Vec<Key> = page.keys().unwrap().cloned().collect();
        let old_children: Vec<PageId> = page.children().unwrap().cloned().collect();
        if old_keys.is_empty() {
            return TestResult::discard();
        }

        // remove separator i (and it's right child i+1)
        let i = target as usize % old_keys.len();
        page.internal_remove(&old_keys[i])
            .unwrap()
            .expect("separator is one the page");

        let expected_after_remove = |probe: &Key| {
            let old_index = old_keys.iter().filter(|k| *k <= probe).count();
            if old_index == i + 1 {
                old_children[i]
            } else {
                old_children[old_index]
            }
        };

        let boundary_probes = old_keys
            .iter()
            .cloned()
            .chain([higher_key(old_keys.last().unwrap())]);
        for probe in probes.into_iter().chain(boundary_probes) {
            assert_eq!(
                page.find_child(&probe),
                Some(expected_after_remove(&probe)),
                "routing {probe:?} after removing keys[{i}]"
            );
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn borrow_and_borrow_back_leaf_remains_same(LeafPage(mut left): LeafPage) -> TestResult {
        let left_page_id = left.page_id;
        let Some((separator, mut right)) = try_split(&mut left, left_page_id.wrapping_add(1))
        else {
            return TestResult::discard();
        };
        let (left_before, right_before) = (left.clone(), right.clone());

        // borrow the right page's first row into the left page
        let key1 = match left.leaf_borrow_from_right(&mut right) {
            Ok(k) => k,
            Err(
                PageError::PageFull | PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)),
            ) => return TestResult::discard(),
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };
        // the new separator is the right page's new first key
        let right_first = right.records().unwrap().next().unwrap();
        assert_eq!(right_first.cmp_key(&key1), Ordering::Equal);

        // borrow it straight back
        let key2 = right
            .leaf_borrow_from_left(&mut left)
            .expect("borrow back must succeed");
        assert_eq!(
            key2, separator,
            "borrow-back must restore the original separator"
        );

        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn borrow_and_borrow_back_internal_remains_same(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let left_page_id = left.page_id;
        let Some((separator, mut right)) = try_split(&mut left, left_page_id.wrapping_add(1))
        else {
            return TestResult::discard();
        };
        let (left_before, right_before) = (left.clone(), right.clone());

        // the separator comes down, the right page's first child moves over, its first key goes up
        let key1 = match left.internal_borrow_from_right(&mut right, separator.clone()) {
            Ok(k) => k,
            Err(
                PageError::PageFull | PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)),
            ) => return TestResult::discard(),
            Err(e) => panic!("unexpected borrow error: {e:?}"),
        };

        // the new separator is the old right page's first key
        let right_first = right_before.keys().unwrap().next().unwrap();
        assert_eq!(right_first, &key1);

        // borrow it straight back
        let key2 = right
            .internal_borrow_from_left(&mut left, key1)
            .expect("borrow back must succeed");
        assert_eq!(
            key2, separator,
            "borrow-back must restore the original separator"
        );

        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn borrows_fail_with_wrong_page_type(
        LeafPage(mut leaf): LeafPage,
        InternalPage(mut internal): InternalPage,
    ) -> TestResult {
        // This error is top dog so all other problems with this scenario don't matter
        let res1 = internal.leaf_borrow_from_left(&mut leaf);
        let res2 = internal.leaf_borrow_from_right(&mut leaf);
        let res3 = leaf.leaf_borrow_from_left(&mut internal);
        let res4 = leaf.leaf_borrow_from_right(&mut internal);

        assert_matches!(res1, Err(PageError::WrongPageType));
        assert_matches!(res2, Err(PageError::WrongPageType));
        assert_matches!(res3, Err(PageError::WrongPageType));
        assert_matches!(res4, Err(PageError::WrongPageType));

        let sep = Key::Integer(0);

        let res1 = internal.internal_borrow_from_left(&mut leaf, sep.clone());
        let res2 = internal.internal_borrow_from_right(&mut leaf, sep.clone());
        let res3 = leaf.internal_borrow_from_left(&mut internal, sep.clone());
        let res4 = leaf.internal_borrow_from_right(&mut internal, sep);

        assert_matches!(res1, Err(PageError::WrongPageType));
        assert_matches!(res2, Err(PageError::WrongPageType));
        assert_matches!(res3, Err(PageError::WrongPageType));
        assert_matches!(res4, Err(PageError::WrongPageType));

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_from_right_fails_when_dest_full(payload_sizes: Vec<u16>) -> TestResult {
        if payload_sizes.is_empty() {
            return TestResult::discard();
        }
        // Two valid, adjacent leaves: fill one page, then split it.
        // fill_leaf uses keys 0, 10, 20, ... so every key on the left page is >= 0.
        let mut left = fill_leaf(child(1), payload_sizes);
        let Some((_separator, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };
        // the donor must have a row to spare, or the borrow is rejected for that reason instead
        if right.records().unwrap().count() < 2 {
            return TestResult::discard();
        }
        let right_first = right.records().unwrap().next().cloned().unwrap();

        // Refill the left page with the smallest possible rows until the right page's first
        // row no longer fits. Negative keys are below every existing key, so they're unique
        // and always sort before the separator: the only thing wrong is the size.
        let (schema, _) = leaf_schema();
        let mut key = -1i64;
        while left.can_insert(ValidatedRow::from_row(right_first.clone()).as_ref()) {
            let filler = Row {
                fields: vec![RowValue::Integer(key), RowValue::String(String::new())],
            };
            left.leaf_insert(schema.validate_row(filler).unwrap())
                .unwrap();
            key -= 1;
        }

        let (left_before, right_before) = (left.clone(), right.clone());
        let result = left.leaf_borrow_from_right(&mut right);

        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_from_left_fails_when_dest_full(payload_sizes: Vec<u16>) -> TestResult {
        if payload_sizes.is_empty() {
            return TestResult::discard();
        }
        // Two valid, adjacent leaves: fill one page, then split it.
        // fill_leaf uses keys 0, 10, 20, ... so every key on the left page is >= 0.
        let mut left = fill_leaf(child(1), payload_sizes);
        let Some((_separator, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };
        // the donor must have a row to spare, or the borrow is rejected for that reason instead
        if left.records().unwrap().count() < 2 {
            return TestResult::discard();
        }
        let left_last = left.records().unwrap().last().cloned().unwrap();

        // Refill the right page with the smallest possible rows until the left page's last
        // row no longer fits. Keys should all be higher than anything in left or right.
        let (schema, _) = leaf_schema();
        let mut key = i64::MAX;
        while right.can_insert(ValidatedRow::from_row(left_last.clone()).as_ref()) {
            let filler = Row {
                fields: vec![RowValue::Integer(key), RowValue::String(String::new())],
            };
            right
                .leaf_insert(schema.validate_row(filler).unwrap())
                .unwrap();
            key -= 1;
        }

        let (left_before, right_before) = (left.clone(), right.clone());
        let result = right.leaf_borrow_from_left(&mut left);

        assert_matches!(result, Err(PageError::PageFull));
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrow_returns_error_with_empty_donor(LeafPage(mut left): LeafPage) -> TestResult {
        let mut right = Page::empty_leaf(left.page_id.wrapping_add(1));

        left.set_next(Some(right.page_id)).unwrap();

        let before_left = left.clone();
        let before_right = right.clone();
        let result = left.leaf_borrow_from_right(&mut right);

        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        // now switch sides
        left.set_prev(Some(right.page_id)).unwrap();
        let before_left = left.clone();

        let result = left.leaf_borrow_from_left(&mut right);
        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        TestResult::passed()
    }

    #[quickcheck]
    fn internal_borrow_returns_error_with_empty_donor(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let mut right = internal_with_one_child(999);
        let sep = Key::Integer(0);

        let (before_left, before_right) = (left.clone(), right.clone());

        let result = left.internal_borrow_from_right(&mut right, sep.clone());

        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        // now switch sides
        let result = left.internal_borrow_from_left(&mut right, sep.clone());
        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_)))
        );
        assert_unchanged(&before_left, &left);
        assert_unchanged(&before_right, &right);

        TestResult::passed()
    }

    #[quickcheck]
    fn internal_borrow_fails_when_separator_out_of_order(
        InternalPage(mut left): InternalPage,
    ) -> TestResult {
        let Some((_separator, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };

        let right_borrow_key = left.keys().unwrap().last().cloned().unwrap();
        let result = left.internal_borrow_from_right(&mut right, right_borrow_key.clone());
        assert_matches!(result, Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(k))) if k == right_borrow_key);

        let left_borrow_key = right.keys().unwrap().next().cloned().unwrap();
        let result = right.internal_borrow_from_left(&mut left, left_borrow_key.clone());
        assert_matches!(result, Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(k))) if k == left_borrow_key);

        TestResult::passed()
    }

    #[test]
    fn overlapping_slots_are_corrupt() {
        let (mut bytes, slots) = two_row_leaf();
        // slot 1's row sits just before slot 0's; stretch it one byte into slot 0's row
        let (offset, length) = get_slot(&bytes, slots, 1);
        set_slot(&mut bytes, slots, 1, offset, length + 1);
        fix_checksum(&mut bytes);
        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt {
                kind: CorruptionKind::OverlappingSlots { .. },
                ..
            })
        );
    }

    #[test]
    fn slot_pointing_into_slot_array_is_corrupt() {
        let (mut bytes, slots) = two_row_leaf();
        set_slot(&mut bytes, slots, 0, slots as u16, 4);
        fix_checksum(&mut bytes);
        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt {
                kind: CorruptionKind::InvalidRange { slot: 0, .. },
                ..
            })
        );
    }

    #[test]
    fn slot_with_trailing_bytes_is_corrupt() {
        let (mut bytes, slots) = two_row_leaf();
        // move slot 1's row one byte earlier (into free space) and grow its slot by one,
        // leaving one extra byte after the row that nothing else uses
        let (offset, length) = get_slot(&bytes, slots, 1);
        let (o, l) = (offset as usize, length as usize);
        bytes.copy_within(o..o + l, o - 1);
        bytes[o - 1 + l] = 0xAB;
        set_slot(&mut bytes, slots, 1, (o - 1) as u16, length + 1);
        fix_checksum(&mut bytes);
        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt {
                kind: CorruptionKind::TrailingBytes { slot: 1 },
                ..
            })
        );
    }

    #[quickcheck]
    fn too_many_slots_triggers_error(mut num_items: u16) -> TestResult {
        let (mut bytes, slot_array) = two_row_leaf();
        num_items = num_items.saturating_add(1 + MAX_LEAF_ITEMS as u16);

        // num_items is always the two bytes preceding the slot_array
        let offset = slot_array - 2;
        // write our big num_items value into the right spot
        bytes[offset..offset + 2].copy_from_slice(&num_items.to_be_bytes());
        fix_checksum(&mut bytes);

        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt { kind: CorruptionKind::TooManyItems(n), .. }) if n == num_items as usize
        );
        TestResult::passed()
    }

    #[test]
    fn slot_count_at_boundary_doesnt_trigger_toomanyitems() {
        let (mut bytes, slot_array) = two_row_leaf();
        let offset = slot_array - 2;
        // check right at the boundary
        let num_items = MAX_LEAF_ITEMS as u16;
        bytes[offset..offset + 2].copy_from_slice(&num_items.to_be_bytes());
        fix_checksum(&mut bytes);

        assert_matches!(
            Page::deserialize(&mut &bytes[..]),
            Err(PageError::Corrupt { kind, .. }) if !matches!(kind, CorruptionKind::TooManyItems(_))
        );
    }

    #[quickcheck]
    fn internal_unsorted_keys_triggers_corrupt(
        InternalPage(mut page): InternalPage,
        i: u8,
    ) -> TestResult {
        if page.num_items() < 2 {
            return TestResult::discard();
        }
        let i = i as usize % (page.num_items() - 1);
        let j = i + 1;

        let PageBody::Internal { keys, .. } = &mut page.body else {
            unreachable!()
        };
        // swap two adjacent keys
        keys.swap(i, j);

        let result = page.serialize(&mut Vec::new());

        assert_matches!(result, Err(PageError::Corrupt { page_id: Some(pid), kind: CorruptionKind::UnsortedKeys { at } }) if pid == page.page_id() && at == j);
        TestResult::passed()
    }

    #[quickcheck]
    fn too_many_entries_triggers_exceed_capacity(mut page: Page) -> TestResult {
        // we want something that's at least half full
        if page.is_underfull() {
            return TestResult::discard();
        }

        // double the number of entries until we're overfull
        while page.free_space().is_some() {
            if page.is_leaf() {
                let PageBody::Leaf { records, .. } = &mut page.body else {
                    unreachable!()
                };
                let rec_clone = records.clone();
                records.extend(rec_clone.into_iter().cycle().take(5));
            } else {
                let PageBody::Internal { keys, children } = &mut page.body else {
                    unreachable!()
                };
                let key_clone = keys.clone();
                let num_keys = key_clone.len();
                let child_clone = children.clone();
                keys.extend(key_clone.clone().into_iter());
                children.extend(child_clone.clone().into_iter().take(num_keys));
            }
        }
        assert_eq!(page.free_space(), None);
        let result = page.as_raw_page();

        assert_matches!(
            result,
            Err(PageError::Corrupt {
                kind: CorruptionKind::ExceedsCapacity,
                ..
            })
        );
        TestResult::passed()
    }

    #[quickcheck]
    fn internal_invalid_key_triggers_corruption(
        InternalPage(page): InternalPage,
        idx: u8,
        bool_val: bool,
        float_val: f64,
    ) -> TestResult {
        let size = page.num_items();
        if size == 0 {
            return TestResult::discard();
        }
        let idx = idx as usize % size;
        let original = page.as_raw_page().expect("page should serialize");

        // the slot array follows the header and the (size + 1) children
        let slot_array = MAX_INTERNAL_HEADER_SIZE + (size + 1) * PAGE_ID_SIZE;
        let (offset, length) = get_slot(&original, slot_array, idx);

        let not_keys = [
            RowValue::Null,
            RowValue::Boolean(bool_val),
            RowValue::Float(float_val),
        ];

        for value in not_keys {
            let mut encoded = Vec::new();
            value.serialize(&mut encoded).unwrap();
            // write it at the start of the key's slot and shrink the slot to match;
            // skip values larger than the key they replace (e.g. a float over a 2-byte key)
            if encoded.len() > length as usize {
                continue;
            }
            let mut bytes = original;
            let start = offset as usize;
            bytes[start..start + encoded.len()].copy_from_slice(&encoded);
            set_slot(&mut bytes, slot_array, idx, offset, encoded.len() as u16);
            fix_checksum(&mut bytes);

            assert_matches!(
                Page::deserialize(&mut &bytes[..]),
                Err(PageError::Corrupt { kind: CorruptionKind::CorruptKey { slot }, .. }) if slot == idx
            );
        }

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_invalid_key_triggers_corruption(
        LeafPage(page): LeafPage,
        idx: u8,
        bool_val: bool,
        float_val: f64,
    ) -> TestResult {
        let size = page.num_items();
        if size == 0 {
            return TestResult::discard();
        }
        let idx = idx as usize % size;
        let original = page.as_raw_page().expect("page should serialize");

        // the slot array follows the header (account for `None` pointers)
        let none_pointers = [page.next().unwrap(), page.prev().unwrap()]
            .iter()
            .filter(|p| p.is_none())
            .count();
        let slot_array = MAX_LEAF_HEADER_SIZE - none_pointers * PAGE_ID_SIZE;
        let (offset, length) = get_slot(&original, slot_array, idx);

        let not_keys = [
            RowValue::Null,
            RowValue::Boolean(bool_val),
            RowValue::Float(float_val),
        ];

        for value in not_keys {
            let mut encoded = Vec::new();
            Row {
                fields: vec![value],
            }
            .serialize(&mut encoded)
            .unwrap();
            if encoded.len() > length as usize {
                continue;
            }
            let mut bytes = original;
            let start = offset as usize;
            bytes[start..start + encoded.len()].copy_from_slice(&encoded);
            set_slot(&mut bytes, slot_array, idx, offset, encoded.len() as u16);
            fix_checksum(&mut bytes);

            assert_matches!(
                Page::deserialize(&mut &bytes[..]),
                Err(PageError::Corrupt {
                    kind: CorruptionKind::InvalidKey(_),
                    ..
                }),
            );
        }

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_get_works(
        LeafPage(mut page): LeafPage,
        idx: u8,
        InternalPage(internal): InternalPage,
    ) -> TestResult {
        // first we check the internal page faults properly
        let result = internal.leaf_get(&Key::Integer(0));
        assert_matches!(result, Err(PageError::WrongPageType));

        let size = page.num_items();
        if size == 0 {
            // this leaf's empty, so we'll do the empty check here and return early
            let result = page.leaf_get(&Key::Integer(0));
            assert_matches!(result, Ok(None));
            return TestResult::passed();
        }

        // Now make a dummy empty leaf to check
        let empty = Page::empty_leaf(child(0));
        let result = empty.leaf_get(&Key::Integer(0));
        assert_matches!(result, Ok(None));

        // Now find a row that actually exists
        let idx = idx as usize % size;
        let expected = page.records().unwrap().nth(idx).cloned().unwrap();
        let key = Key::try_from(&expected.fields[0]).expect("key should be valid");

        let result = page.leaf_get(&key);
        assert_matches!(result, Ok(Some(got)) if got == &expected);

        // Now find create a key that's out of bounds
        let last_row = page.records().unwrap().last().cloned().unwrap();
        let last_key = Key::try_from(&last_row.fields[0]).expect("last key should be valid");

        let result = page.leaf_get(&higher_key(&last_key));
        assert_matches!(result, Ok(None));

        // Now we'll remove a random key and then try to find it
        let expected = page.records().unwrap().nth(idx).cloned().unwrap();
        let key = Key::try_from(&expected.fields[0]).expect("key should be valid");

        page.leaf_remove(&key).expect("remove should succeed");

        let result = page.leaf_get(&key);
        assert_matches!(result, Ok(None));

        // put it back and we should find it
        let valid = ValidatedRow::from_row(expected.clone());
        page.leaf_insert(valid).expect("insert should work");

        let result = page.leaf_get(&key);
        assert_matches!(result, Ok(Some(got)) if got == &expected);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_records_from_works(LeafPage(page): LeafPage, start_key: Key, pick: u8) -> TestResult {
        use std::collections::BTreeMap;

        // reference: the page's rows keyed by primary key
        let model: BTreeMap<Key, Row> = page
            .records()
            .unwrap()
            .map(|r| (Key::try_from(&r.fields[0]).unwrap(), r.clone()))
            .collect();

        // start keys to try: a random key (usually not on the page), an existing key,
        // and one just above an existing key
        let mut starts = vec![start_key];
        if !model.is_empty() {
            let existing = model
                .keys()
                .nth(pick as usize % model.len())
                .unwrap()
                .clone();
            starts.push(higher_key(&existing));
            starts.push(existing);
        }
        for start in &starts {
            let expected: Vec<&Row> = model.range(start..).map(|(_, row)| row).collect();
            let actual: Vec<&Row> = page.leaf_records_from(start).expect("leaf scan").collect();
            assert_eq!(expected, actual, "scan from {start:?}");
        }
        TestResult::passed()
    }

    #[test]
    fn leaf_records_from_iterator_works_after_dropping_key() {
        let page = fill_leaf(child(1), vec![1]);

        // grab the first key in the leaf
        let first_key =
            ValidatedRow::from_row(page.records().unwrap().next().cloned().unwrap()).primary_key();

        // build our row iterator from the first entry
        let mut iterator = page
            .leaf_records_from(&first_key)
            .expect("scan should succeed");

        // drop the reference key
        drop(first_key);

        // make sure iterator is still alive
        assert!(iterator.next().is_some());
    }

    #[quickcheck]
    fn leaf_borrow_with_parent_update_keeps_routing_correct(
        LeafPage(mut left): LeafPage,
    ) -> TestResult {
        // fix the page id
        left.page_id = child(1);
        // split into two pages
        let Some((sep, mut right)) = try_split(&mut left, child(2)) else {
            return TestResult::discard();
        };

        // make a new root that references both
        let mut root = Page::new_root(child(3), left.page_id, sep.clone(), right.page_id);

        let new_sep = if right.num_items() > 2 {
            // borrow from right
            left.leaf_borrow_from_right(&mut right)
                .expect("borrow shouldn't fail")
        } else if left.num_items() > 2 {
            // borrow from left
            right
                .leaf_borrow_from_left(&mut left)
                .expect("borrow shouldn't fail")
        } else {
            return TestResult::discard();
        };

        // update the root
        root.internal_replace_key(&sep, new_sep.clone())
            .expect("replace should work");
        // make sure the keys are correct
        assert_eq!(vec![&new_sep], root.keys().unwrap().collect::<Vec<&Key>>());

        for (page, other) in [(&left, &right), (&right, &left)] {
            for row in page.records().unwrap() {
                let key = Key::try_from(&row.fields[0]).unwrap();
                assert_eq!(
                    root.find_child(&key),
                    Some(page.page_id()),
                    "{key:?} routed to the wrong leaf"
                );
                assert_matches!(page.leaf_get(&key), Ok(Some(_))); // finds key in the correct child
                assert_matches!(other.leaf_get(&key), Ok(None)); // and it isn't also on the other page
            }
        }

        TestResult::passed()
    }

    #[test]
    fn internal_insert_too_long_key_works() {
        let test_boundary = MAX_INTERNAL_ENTRY_SIZE - PAGE_ID_SIZE - SLOT_ENTRY_SIZE - 1 - 2;

        let mut page = internal_with_one_child(1);

        let row = Row::try_from(vec![RowValue::String("a".repeat(test_boundary + 1))]).unwrap();

        let key = ValidatedRow::from_row(row).primary_key();

        let result = page.internal_insert(key, child(999));

        assert_matches!(result, Err(PageError::Schema(SchemaError::KeyTooLong(_))));

        let row = Row::try_from(vec![RowValue::String("a".repeat(test_boundary))]).unwrap();

        let key = ValidatedRow::from_row(row).primary_key();

        let result = page.internal_insert(key, child(999));

        assert_matches!(result, Ok(_));
    }

    #[test]
    fn internal_merge_with_min_internal_counting() {
        let sep = Key::Integer(1);
        let mut left = internal_with_one_child(1);
        let mut right = internal_with_one_child(2);

        let foo = left
            .internal_merge_from_right(&mut right, sep)
            .expect("merge should succeed");
        assert_eq!(foo, right.page_id);
        assert_eq!(left.keys().unwrap().count(), 1);
        assert_eq!(left.children().unwrap().count(), 2);
    }

    #[quickcheck]
    fn lsn_sequence_enforced(mut page: Page, new_lsn: u64) -> TestResult {
        if new_lsn == 0 {
            return TestResult::discard();
        }

        // make sure stales are detected
        match page.lsn() {
            Some(lsn) if lsn.get() < 2 => return TestResult::discard(),
            Some(lsn) => {
                let old_lsn = lsn;
                let new_lsn = Lsn::new(old_lsn.get().saturating_sub(1)).unwrap();
                let result = page.set_lsn(new_lsn);
                assert_matches!(result, Err(PageError::StaleLsnUpdate { old, new }) if old == old_lsn && new == new_lsn )
            }
            None => {}
        }

        // make sure updates are accepted
        match page.lsn() {
            Some(lsn) if lsn.get() == u64::MAX => return TestResult::discard(),
            Some(lsn) => {
                let old_lsn = lsn;
                let new_lsn = Lsn::new(old_lsn.get().saturating_add(1)).unwrap();
                page.set_lsn(new_lsn).expect("valid updates should take");
            }
            None => {
                page.set_lsn(Lsn::new(new_lsn).unwrap())
                    .expect("None overwrites should always succeed");
            }
        }

        assert_roundtrip(page);
        TestResult::passed()
    }

    #[quickcheck]
    fn find_child_index_round_trip(InternalPage(page): InternalPage) -> TestResult {
        let PageBody::Internal { keys, .. } = &page.body else {
            unreachable!()
        };
        let all_keys: Vec<&Key> = keys.iter().collect();

        for key in all_keys {
            let Some((child_idx, child_id)) = page.find_child_index(key) else {
                unreachable!()
            };
            let found = page.child_at(child_idx);
            assert_eq!(found, Some(child_id));
        }
        TestResult::passed()
    }

    #[quickcheck]
    fn next_prev_getters_setters_only_work_on_leaves_and_leave_original_intact(
        mut page: Page,
    ) -> TestResult {
        if page.is_leaf() {
            return TestResult::discard();
        }
        let before = page.clone();

        let next = page.next();
        assert_matches!(next, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);

        let set_next = page.set_next(None);
        assert_matches!(set_next, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);

        let prev = page.prev();
        assert_matches!(prev, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);

        let set_prev = page.set_prev(None);
        assert_matches!(set_prev, Err(PageError::WrongPageType));
        assert_unchanged(&before, &page);
        TestResult::passed()
    }

    /// Runs a replacement that must fail, checks the page is byte-for-byte unchanged, returns the error.
    fn rejected_replace(page: &mut Page, old: &Key, new: Key) -> PageError {
        let before = page.clone();
        let err = page
            .internal_replace_key(old, new)
            .expect_err("replacement should be rejected");
        assert_unchanged(&before, page);
        err
    }

    #[test]
    fn internal_replace_key_rejections_leave_page_unchanged() {
        let int = Key::Integer;

        // keys [10, 20, 30]
        let mut page = Page::new_root(child(1), child(2), int(10), child(3));
        page.internal_insert(int(20), child(4)).unwrap();
        page.internal_insert(int(30), child(5)).unwrap();

        // new <= left neighbor
        for new in [10, 5] {
            assert_matches!(
                rejected_replace(&mut page, &int(20), int(new)),
                PageError::KeyNotInOrder(k) if k == int(new)
            );
        }

        // new >= right neighbor
        for new in [30, 35] {
            assert_matches!(
                rejected_replace(&mut page, &int(20), int(new)),
                PageError::KeyNotInOrder(k) if k == int(new)
            );
        }

        // edges: first key has no left neighbor, last key has no right neighbor
        assert_matches!(
            rejected_replace(&mut page, &int(10), int(20)),
            PageError::KeyNotInOrder(_)
        );
        assert_matches!(
            rejected_replace(&mut page, &int(30), int(20)),
            PageError::KeyNotInOrder(_)
        );

        // old key missing: both keys come back to the caller
        assert_matches!(
            rejected_replace(&mut page, &int(25), int(26)),
            PageError::MissingKey { search_key, new_key }
                if search_key == int(25) && new_key == int(26)
        );

        // sanity check: the fixture accepts a legal replacement
        page.internal_replace_key(&int(20), int(25))
            .expect("key should have been accepted");

        // larger key on a full page. Keys are padded_key(0), padded_key(2), ...,
        // so padded_key(1, ..) sorts between the first two.
        let mut full = fill_internal(child(100), vec![0]);
        let first = full.keys().unwrap().next().unwrap().clone();
        let bigger = padded_key(1, max_internal_len());
        assert!(
            full.free_space().unwrap() + Page::internal_entry_size(&first)
                < Page::internal_entry_size(&bigger),
            "fixture must be too full for the bigger key"
        );
        assert_matches!(
            rejected_replace(&mut full, &first, bigger),
            PageError::PageFull
        );

        // leaf page
        let mut leaf = Page::empty_leaf(child(50));
        assert_matches!(
            rejected_replace(&mut leaf, &int(1), int(2)),
            PageError::WrongPageType
        );
    }

    #[quickcheck]
    fn as_raw_page_rejects_broken_pages(mut page: Page, pick: u8, at: u16) -> TestResult {
        let expected = match &mut page.body {
            PageBody::Internal { keys, children } => match pick % 2 {
                0 if keys.len() >= 2 => {
                    let i = at as usize % (keys.len() - 1);
                    keys.swap(i, i + 1);
                    CorruptionKind::UnsortedKeys { at: i + 1 }
                }
                1 => {
                    children.push(child(0));
                    CorruptionKind::ChildCountMismatch {
                        keys: keys.len(),
                        children: children.len(),
                    }
                }
                _ => return TestResult::discard(),
            },
            PageBody::Leaf { records, .. } if !records.is_empty() => {
                let i = at as usize % records.len();
                match pick % 3 {
                    0 if records.len() >= 2 => {
                        let i = i.min(records.len() - 2);
                        records.swap(i, i + 1);
                        CorruptionKind::UnsortedKeys { at: i + 1 }
                    }
                    1 => {
                        records[i].fields[0] = RowValue::Null;
                        CorruptionKind::InvalidKey(RowValue::Null)
                    }
                    2 => {
                        records[i].fields.clear();
                        CorruptionKind::MissingKey
                    }
                    _ => return TestResult::discard(),
                }
            }
            _ => return TestResult::discard(),
        };
        assert_matches!(page.as_raw_page(), Err(PageError::Corrupt { kind, .. }) if kind == expected);
        TestResult::passed()
    }

    #[quickcheck]
    fn internal_borrow_rejects_separator_that_doesnt_fit(key_sizes: Vec<u16>) -> TestResult {
        if key_sizes.is_empty() {
            return TestResult::discard();
        }

        let mut left = fill_internal(child(1), key_sizes);
        let n = left.num_items();

        let sep = padded_key(2 * n - 1, max_internal_len());
        assert!(
            !left.can_insert_separator(&sep),
            "a full page can't take a max-size separator"
        );

        let mut right = internal_with_one_child(1000);
        right
            .internal_insert(padded_key(2 * n + 1, 6), child(1002))
            .unwrap();

        let (left_before, right_before) = (left.clone(), right.clone());

        assert_matches!(
            left.borrow_from_right(&mut right, sep),
            Err(PageError::PageFull)
        );
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);

        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_in_wrong_directions_fail(LeafPage(mut left): LeafPage) -> TestResult {
        let Some((parent_sep, mut right)) = try_split(&mut left, child(999)) else {
            return TestResult::discard();
        };

        // left borrow from left with right
        let expected_prev = left.prev().unwrap();
        let got_prev = right.page_id();
        let (left_before, right_before) = (left.clone(), right.clone());
        let result = left.borrow_from_left(&mut right, parent_sep.clone());
        assert_matches!(result, Err(PageError::InvalidBorrow(BorrowFailReason::PointerMismatch { expected, got })) if expected == expected_prev && got == Some(got_prev));
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);

        // right borrow from right with right
        let expected_next = right.next().unwrap();
        let got_next = left.page_id();
        let (left_before, right_before) = (left.clone(), right.clone());

        let result = right.borrow_from_right(&mut left, parent_sep.clone());
        assert_matches!(result, Err(PageError::InvalidBorrow(BorrowFailReason::PointerMismatch { expected, got })) if expected == expected_next && got == Some(got_next));
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }

    #[quickcheck]
    fn leaf_borrows_with_overlapping_ranges_fail(
        SchemaWithRows(schema, rows): SchemaWithRows,
    ) -> TestResult {
        if rows.is_empty() {
            return TestResult::discard();
        }
        let mut rows: Vec<ValidatedRow> = rows
            .into_iter()
            .map(|r| schema.validate_row(r).unwrap())
            .collect();
        rows.sort_by_key(|a| a.primary_key());
        rows.dedup_by(|a, b| a.primary_key() == b.primary_key());

        let mut left = Page::empty_leaf(child(1));
        let mut right = Page::empty_leaf(child(2));

        left.set_next(Some(right.page_id())).unwrap();

        // alternate sorted rows: left gets 0, 2, 4, …; right gets 1, 3, 5, …
        for (i, row) in rows.into_iter().enumerate() {
            let page = if i % 2 == 0 { &mut left } else { &mut right };
            match page.leaf_insert(row) {
                Ok(()) | Err(PageError::PageFull) => {}
                Err(e) => panic!("unexpected insert error: {e:?}"),
            }
        }

        // make sure one of them didn't fill up too fast with large payloads so we have valid key ordering
        let left_max = left
            .records()
            .unwrap()
            .last()
            .map(|r| Key::try_from(&r.fields[0]).unwrap());
        let right_min = right
            .records()
            .unwrap()
            .next()
            .map(|r| Key::try_from(&r.fields[0]).unwrap());
        match (left_max, right_min) {
            (Some(l), Some(r)) if r < l => {}
            _ => return TestResult::discard(),
        }

        let (left_before, right_before) = (left.clone(), right.clone());

        let result = left.leaf_borrow_from_right(&mut right);
        assert_matches!(
            result,
            Err(PageError::InvalidBorrow(BorrowFailReason::KeysOutOfOrder(
                _
            )))
        );
        assert_unchanged(&left_before, &left);
        assert_unchanged(&right_before, &right);
        TestResult::passed()
    }
}

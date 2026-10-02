use crate::commontypes::{Key, Lsn, PAGE_ID_SIZE, PageId, PageLsn, TableId};
use crate::page::internal::{ChildIndex, KeyIndex};
use crate::page::tests::generators::*;
use crate::page::{
    BorrowFailReason, CorruptionKind, MAX_LEAF_HEADER_SIZE, MergeFailReason, PAGE_SIZE, Page,
    PageBody, PageError,
};
use crate::schema::{Row, RowValue, ValidatedRow};
use crate::test_support::*;
use crate::traits::Serializable;
use quickcheck::TestResult;
use quickcheck_macros::quickcheck;
use std::cmp::Ordering;
use std::io::Cursor;

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
fn find_child_on_leaf_is_none(LeafPage(page): LeafPage, key: Key) -> TestResult {
    assert_eq!(page.find_child(&key), None);
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
fn borrow_and_borrow_back_leaf_remains_same(LeafPage(mut left): LeafPage) -> TestResult {
    let left_page_id = left.page_id;
    let Some((separator, mut right)) = try_split(&mut left, left_page_id.wrapping_add(1)) else {
        return TestResult::discard();
    };
    let (left_before, right_before) = (left.clone(), right.clone());

    // borrow the right page's first row into the left page
    let key1 = match left.leaf_borrow_from_right(&mut right) {
        Ok(k) => k,
        Err(PageError::PageFull | PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_))) => {
            return TestResult::discard();
        }
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

#[quickcheck]
fn leaf_doesnt_work_for_child_at_or_key_at(LeafPage(page): LeafPage) -> TestResult {
    for i in 0..page.num_items() {
        assert!(page.child_at(ChildIndex::new(i)).is_none());
        assert!(page.key_at(KeyIndex::new(i)).is_none());
    }

    TestResult::passed()
}

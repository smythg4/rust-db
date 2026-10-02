use crate::commontypes::{Key, Lsn, PAGE_ID_SIZE, PageId, PageLsn, SLOT_ENTRY_SIZE, TableId};
use crate::page::internal::{ChildIndex, KeyIndex};
use crate::page::tests::generators::*;
use crate::page::{
    BorrowFailReason, CorruptionKind, MAX_INTERNAL_ENTRY_SIZE, MAX_INTERNAL_HEADER_SIZE, Page,
    PageBody, PageError,
};
use crate::schema::{Row, RowValue, SchemaError, ValidatedRow};
use crate::test_support::*;
use crate::traits::Serializable;
use quickcheck::TestResult;
use quickcheck_macros::quickcheck;

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
fn borrow_and_borrow_back_internal_remains_same(
    InternalPage(mut left): InternalPage,
) -> TestResult {
    let left_page_id = left.page_id;
    let Some((separator, mut right)) = try_split(&mut left, left_page_id.wrapping_add(1)) else {
        return TestResult::discard();
    };
    let (left_before, right_before) = (left.clone(), right.clone());

    // the separator comes down, the right page's first child moves over, its first key goes up
    let key1 = match left.internal_borrow_from_right(&mut right, separator.clone()) {
        Ok(k) => k,
        Err(PageError::PageFull | PageError::InvalidBorrow(BorrowFailReason::EmptyBorrow(_))) => {
            return TestResult::discard();
        }
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
fn no_keys_or_chidren_lost_in_internal_borrow(InternalPage(mut left): InternalPage) -> TestResult {
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
fn internal_key_child_ats_work(InternalPage(page): InternalPage) -> TestResult {
    let num_childs = page.children().unwrap().count();
    let num_keys = page.keys().unwrap().count();

    assert_eq!(num_keys + 1, num_childs);
    for i in 0..num_childs * 2 {
        if i < num_childs {
            assert!(page.child_at(ChildIndex::new(i)).is_some());
        } else {
            assert!(page.child_at(ChildIndex::new(i)).is_none());
        }
        if i < num_childs - 1 {
            assert!(page.key_at(KeyIndex::new(i)).is_some());
        } else {
            assert!(page.key_at(KeyIndex::new(i)).is_none());
        }
    }

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
fn no_keys_or_chidren_lost_in_internal_split(InternalPage(mut page): InternalPage) -> TestResult {
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

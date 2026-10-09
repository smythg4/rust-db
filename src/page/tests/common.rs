use crate::page::tests::generators::{InternalPage, LeafPage};
use crate::page::{
    CHECKSUM_OFFSET, CorruptionKind, MAX_INTERNAL_HEADER_SIZE, MAX_LEAF_HEADER_SIZE,
    MAX_LEAF_ITEMS, MergeFailReason, PAGE_SIZE, Page, PageBody, PageError,
};
use crate::schema::{Key, Row, RowValue};
use crate::test_support::*;
use crate::traits::Serializable;
use crate::types::{Lsn, PAGE_ID_SIZE};
use quickcheck::TestResult;
use quickcheck_macros::quickcheck;
use std::cmp::Ordering;
use std::io::Cursor;

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

#[test]
fn split_page_limits_hold() {
    let (schema, _max_size) = leaf_schema();

    let mut leaf = Page::empty_leaf(child(1));

    // empty leaf should fail
    let result = leaf.split_page(child(2));
    assert_matches!(result, Err(PageError::TooSmallToSplit(pid)) if pid == child(1));

    // leaf with single entry should also fail
    let vr = schema
        .validate_row(Row {
            fields: vec![RowValue::Integer(1), RowValue::String("dummy".into())],
        })
        .unwrap();
    leaf.leaf_insert(vr).unwrap();
    let result = leaf.split_page(child(2));
    assert_matches!(result, Err(PageError::TooSmallToSplit(pid)) if pid == child(1));

    // two rows should succeed - check that the right separator came out
    let vr = schema
        .validate_row(Row {
            fields: vec![RowValue::Integer(2), RowValue::String("dummy".into())],
        })
        .unwrap();
    leaf.leaf_insert(vr).unwrap();
    let result = leaf.split_page(child(2));
    assert_matches!(result, Ok(kp) if kp.0 == Key::Integer(2));

    let mut internal = internal_with_one_child(3);

    // empty internal should fail
    let result = internal.split_page(child(4));
    assert_matches!(result, Err(PageError::TooSmallToSplit(pid)) if pid == child(3));

    // should fail with 1 key too
    let key = Key::Integer(0);
    internal.internal_insert(key, child(0)).unwrap();
    let result = internal.split_page(child(4));
    assert_matches!(result, Err(PageError::TooSmallToSplit(pid)) if pid == child(3));

    // should fail with 2 keys too
    let key = Key::Integer(1);
    internal.internal_insert(key, child(0)).unwrap();
    let result = internal.split_page(child(4));
    assert_matches!(result, Err(PageError::TooSmallToSplit(pid)) if pid == child(3));

    // should succeed with 3 keys - check the proper separator
    let key = Key::Integer(2);
    internal.internal_insert(key, child(0)).unwrap();
    let result = internal.split_page(child(4));
    assert_matches!(result, Ok(kp) if kp.0 == Key::Integer(1));
}

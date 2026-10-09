use crate::schema::{Key, KeyError, RowValue};
use crate::test_support::*;
use crate::traits::Serializable;
use crate::types::{
    Lsn, LsnError, PAGE_ID_SIZE, PageId, PageLsn, SLOT_ENTRY_SIZE, SlotEntry, SlotIndex, TableId,
};
use quickcheck::TestResult;
use quickcheck_macros::quickcheck;

#[test]
fn page_id_size_constant_is_accurate() {
    let mut buffer = Vec::with_capacity(PAGE_ID_SIZE);
    let tid = TableId::new(0);
    let pid = PageId::new(tid, 0);
    pid.serialize(&mut buffer).unwrap();
    assert_eq!(PAGE_ID_SIZE, buffer.len());
}

#[test]
fn slot_entry_size_constant_is_accurate() {
    let mut buffer = Vec::with_capacity(SLOT_ENTRY_SIZE);
    let se = SlotEntry::new(0, 0);
    se.serialize(&mut buffer).unwrap();
    assert_eq!(SLOT_ENTRY_SIZE, buffer.len());
}

#[test]
fn zero_lsns_trigger_errors() {
    let result = Lsn::new(0);
    assert_matches!(result, Err(LsnError::ZeroLsn))
}

#[test]
fn null_and_float_keys_trigger_errors() {
    let null_result = Key::try_from(&RowValue::Null);
    let float_result = Key::try_from(&RowValue::Float(10.0));

    assert_matches!(null_result, Err(KeyError::NullKey));
    assert_matches!(float_result, Err(KeyError::NotOrderable));
}

#[quickcheck]
fn basic_types_size_and_roundtrip(
    key: Key,
    table_id: TableId,
    page_id: PageId,
    lsn: Lsn,
    slot_index: SlotIndex,
    slot_entry: SlotEntry,
    page_lsn: PageLsn,
) -> TestResult {
    assert_roundtrip(key);
    assert_roundtrip(table_id);
    assert_roundtrip(page_id);
    let mut o_page_id = Some(page_id);
    assert_roundtrip(o_page_id);
    o_page_id = None;
    assert_roundtrip(o_page_id);
    assert_roundtrip(lsn);
    assert_roundtrip(slot_index);
    assert_roundtrip(slot_entry);
    assert_roundtrip(page_lsn);
    TestResult::passed()
}

#[quickcheck]
fn higher_key_helper_works(k: Key) -> TestResult {
    let hk = higher_key(&k);

    assert!(hk > k);
    TestResult::passed()
}

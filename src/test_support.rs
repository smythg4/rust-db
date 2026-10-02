use crate::commontypes::{Key, PAGE_ID_SIZE, PageId, SLOT_ENTRY_SIZE, TableId};
use crate::page::{
    CHECKSUM_OFFSET, MAX_INTERNAL_ENTRY_SIZE, MAX_LEAF_ENTRY_SIZE, MAX_LEAF_HEADER_SIZE, Page,
    PageBody, PageError, RawPage,
};
use crate::schema::{Column, Row, RowValue, Schema, ValidatedRow};
use crate::traits::Serializable;
use std::io::Cursor;

/// Like `assert!(matches!(..))`, but prints the actual value on failure.
macro_rules! assert_matches {
      ($value:expr, $pattern:pat $(if $guard:expr)? $(,)?) => {
          match $value {
              $pattern $(if $guard)? => {}
              ref other => panic!(
                  "assertion failed: `{}` does not match `{}`\n  value: {:?}",
                  stringify!($value),
                  stringify!($pattern),
                  other
              ),
          }
      };
  }
pub(crate) use assert_matches;

/// A two-row leaf (both pointers `None`) and the byte offset of its slot array.
pub(crate) fn two_row_leaf() -> (RawPage, usize) {
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

pub(crate) fn get_slot(bytes: &RawPage, slot_array: usize, slot: usize) -> (u16, u16) {
    let at = slot_array + slot * SLOT_ENTRY_SIZE;
    (
        u16::from_be_bytes([bytes[at], bytes[at + 1]]),
        u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]),
    )
}

pub(crate) fn set_slot(
    bytes: &mut RawPage,
    slot_array: usize,
    slot: usize,
    offset: u16,
    length: u16,
) {
    let at = slot_array + slot * SLOT_ENTRY_SIZE;
    bytes[at..at + 2].copy_from_slice(&offset.to_be_bytes());
    bytes[at + 2..at + 4].copy_from_slice(&length.to_be_bytes());
}

pub(crate) fn fix_checksum(bytes: &mut RawPage) {
    let crc = Page::page_checksum(bytes);
    bytes[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
}

pub(crate) fn assert_roundtrip<T>(value: T)
where
    T: Serializable + PartialEq + std::fmt::Debug,
    T::Error: std::fmt::Debug,
{
    let mut buf = Cursor::new(Vec::new());
    value.serialize(&mut buf).unwrap();
    buf.set_position(0);
    let deser = T::deserialize(&mut buf).unwrap();
    assert_eq!(deser, value);
    assert_eq!(buf.get_ref().len(), value.encoded_size());
    assert_eq!(buf.position() as usize, value.encoded_size());
}

/// Fixed-width key prefix so string keys sort by their index regardless of padding.
pub(crate) fn padded_key(index: usize, total_len: usize) -> Key {
    let prefix = format!("{index:06}");
    Key::String(format!(
        "{prefix}{}",
        "x".repeat(total_len.saturating_sub(prefix.len()))
    ))
}

/// Two-column schema (integer key + string payload) and the largest payload that still validates.
pub(crate) fn leaf_schema() -> (Schema, usize) {
    let schema = Schema::try_from(vec![
        Column::integer("id").unwrap(),
        Column::string("data").unwrap(),
    ])
    .unwrap();
    let row_with = |key: i64, len: usize| Row {
        fields: vec![RowValue::Integer(key), RowValue::String("p".repeat(len))],
    };
    let max_payload = (0..MAX_LEAF_ENTRY_SIZE)
        .rev()
        .find(|&len| schema.validate_row(row_with(0, len)).is_ok())
        .expect("some payload must validate");
    (schema, max_payload)
}

pub(crate) fn higher_key(key: &Key) -> Key {
    match key {
        Key::Integer(n) if *n < i64::MAX => Key::Integer(n.wrapping_add(1)),
        Key::Integer(_) => Key::String("()".to_string()),
        Key::String(s) => Key::String(format!("{s}s")),
    }
}

pub(crate) fn assert_unchanged(before: &Page, after: &Page) {
    let before = before.as_raw_page().unwrap();
    let after = after.as_raw_page().unwrap();
    assert_eq!(before, after);
}

pub(crate) fn child(n: u32) -> PageId {
    PageId::new(TableId::new(1), n)
}

pub(crate) fn max_internal_len() -> usize {
    (0..MAX_INTERNAL_ENTRY_SIZE)
        .rev()
        .find(|&len| Page::internal_entry_size(&padded_key(0, len)) <= MAX_INTERNAL_ENTRY_SIZE)
        .unwrap()
}

pub(crate) fn fill_internal(start_id: PageId, key_sizes: Vec<u16>) -> Page {
    let max_key_len = max_internal_len();

    // build a left page and fill it all the way up with various sized keys (should have a tiny space available at the end)
    let mut left = Page::empty_page(
        start_id,
        PageBody::Internal {
            keys: Vec::new(),
            children: vec![start_id.wrapping_add(1)],
        },
    );
    for (i, size) in key_sizes.iter().cycle().enumerate() {
        let key = padded_key(i * 2, 6 + (*size as usize) % (max_key_len - 5));
        match left.internal_insert(key, start_id.wrapping_add(i as u32 + 2)) {
            Ok(()) => {}
            Err(PageError::PageFull) => break,
            Err(e) => panic!("unexpected error while filling: {e:?}"),
        }
    }
    left
}

fn make_row(schema: &Schema, key: i64, len: usize) -> ValidatedRow {
    schema
        .validate_row(Row {
            fields: vec![RowValue::Integer(key), RowValue::String("p".repeat(len))],
        })
        .unwrap()
}

pub(crate) fn fill_leaf(page_id: PageId, payload_sizes: Vec<u16>) -> Page {
    let (schema, max_payload) = leaf_schema();

    // generate an empty leaf page
    let mut page = Page::empty_leaf(page_id);
    let mut count = 0i64;

    // fill it up with various sized rows and keys 0, 10, 20, ...
    for len in payload_sizes.iter().cycle() {
        let len = *len as usize % (max_payload + 1);
        match page.leaf_insert(make_row(&schema, count * 10, len)) {
            Ok(()) => count += 1,
            Err(PageError::PageFull) => break,
            Err(e) => panic!("Unexpected error: {e:?}"),
        }
    }
    page
}

pub(crate) fn internal_with_one_child(id: u32) -> Page {
    Page::empty_page(
        child(id),
        PageBody::Internal {
            keys: Vec::new(),
            children: vec![child(id).wrapping_add(1)],
        },
    )
}

pub(crate) fn try_split(page: &mut Page, new_id: PageId) -> Option<(Key, Page)> {
    match page.split_page(new_id) {
        Ok(ss) => Some(ss),
        Err(PageError::TooSmallToSplit(_)) => None,
        Err(e) => panic!("Unexpected split error: {e:?}"),
    }
}

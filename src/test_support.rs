use crate::commontypes::{Key, PageId, TableId};
use crate::page::{MAX_INTERNAL_ENTRY_SIZE, MAX_LEAF_ENTRY_SIZE, Page, PageBody, PageError};
use crate::schema::{Column, ColumnType, Row, RowValue, Schema, ValidatedRow};
use crate::traits::Serializable;
use quickcheck::{Arbitrary, Gen};
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

#[derive(Debug, Clone)]
pub(crate) struct SchemaWithRows(pub(crate) Schema, pub(crate) Vec<Row>);

impl Arbitrary for SchemaWithRows {
    fn arbitrary(g: &mut Gen) -> Self {
        let schema = Schema::arbitrary(g);
        let n = usize::arbitrary(g) % 25;
        let rows: Vec<Row> = (0..n)
            .map(|_| valid_row_from_schema(&schema, g).into())
            .collect();
        SchemaWithRows(schema, rows)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SchemaRowPair(pub(crate) Schema, pub(crate) Row);

impl Arbitrary for SchemaRowPair {
    fn arbitrary(g: &mut Gen) -> Self {
        let schema = Schema::arbitrary(g);
        let row = valid_row_from_schema(&schema, g).into();
        SchemaRowPair(schema, row)
    }
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
    let schema = Schema::try_from(vec![Column::integer(), Column::string()]).unwrap();
    let row_with = |key: i64, len: usize| Row {
        fields: vec![RowValue::Integer(key), RowValue::String("p".repeat(len))],
    };
    let max_payload = (0..MAX_LEAF_ENTRY_SIZE)
        .rev()
        .find(|&len| schema.validate_row(row_with(0, len)).is_ok())
        .expect("some payload must validate");
    (schema, max_payload)
}

pub(crate) fn valid_row_from_schema(schema: &Schema, g: &mut Gen) -> ValidatedRow {
    loop {
        let row = Row {
            fields: schema
                .columns
                .iter()
                .map(|c| {
                    let coin_flip = bool::arbitrary(g);
                    match c.col_type {
                        _ if c.nullable && coin_flip => RowValue::Null,
                        ColumnType::Bool => RowValue::Boolean(bool::arbitrary(g)),
                        ColumnType::Float => {
                            let mut f = f64::arbitrary(g);
                            while f.is_nan() {
                                f = f64::arbitrary(g);
                            }
                            RowValue::Float(f)
                        }
                        ColumnType::Integer => RowValue::Integer(i64::arbitrary(g)),
                        ColumnType::String => RowValue::String(String::arbitrary(g)),
                    }
                })
                .collect(),
        };
        if let Ok(vr) = schema.validate_row(row) {
            break vr;
        }
    }
}

pub(crate) fn increment_key_on_row(row: &Row) -> Row {
    let mut new_row = row.clone();
    let new_key = match &row.fields[0] {
        RowValue::Integer(n) => RowValue::Integer(n.wrapping_add(1)),
        RowValue::String(s) => RowValue::String(format!("{s}1")),
        _ => unreachable!(),
    };
    new_row.fields[0] = new_key;
    new_row
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

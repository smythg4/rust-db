use quickcheck::TestResult;
use quickcheck_macros::quickcheck;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use crate::bpm::{BufferPoolManager, Replacer};
use crate::disk::FakeDisk;
use crate::types::TableId;

use crate::schema::tests::SchemaWithRows;
use crate::schema::{Key, Row};
use crate::table::Table;

#[quickcheck]
fn btree_inserts_preserved_and_ordered(SchemaWithRows(schema, rows): SchemaWithRows) -> TestResult {
    let bpm = BufferPoolManager::new(FakeDisk::default(), Replacer::default(), 128);
    let table =
        Table::create(&bpm, TableId::new(1), schema, "basic table").expect("create table failed");

    let mut model: BTreeMap<Key, Row> = BTreeMap::new();

    for row in rows {
        let key = Key::try_from(&row.fields[0]).expect("generated rows have valid keys");
        let result = table.insert(row.clone());
        match model.entry(key) {
            Entry::Occupied(e) => {
                assert!(result.is_err(), "duplicate key {:?} was accepted", e.key())
            }
            Entry::Vacant(e) => {
                result.expect("failed to insert row");
                e.insert(row);
            }
        }
    }

    let actual = table.get_all(|_| true).expect("failed to fetch rows");
    let expected: Vec<Row> = model.into_values().collect();
    assert_eq!(expected, actual);
    TestResult::passed()
}

#[quickcheck]
fn btree_deletes_cant_be_found(
    SchemaWithRows(schema, rows): SchemaWithRows,
    picks: Vec<usize>,
) -> TestResult {
    let bpm = BufferPoolManager::new(FakeDisk::default(), Replacer::default(), 128);
    let table = Table::create(&bpm, TableId::new(1), schema, "basic table")
        .expect("failed to create table");

    let mut model: BTreeMap<Key, Row> = BTreeMap::new();
    for row in rows {
        let key = Key::try_from(&row.fields[0]).expect("valid keys");
        if let Entry::Vacant(e) = model.entry(key) {
            table.insert(row.clone()).expect("failed to insert row");
            e.insert(row);
        }
    }
    let keys: Vec<Key> = model.keys().cloned().collect();
    if keys.is_empty() {
        return TestResult::discard();
    }

    for p in picks {
        let key = &keys[p % keys.len()];
        let deleted = table.delete(key).expect("delete failed");
        assert_eq!(
            deleted,
            model.remove(key),
            "delete({key:?}) returned the wrong row"
        );
        assert!(
            matches!(table.get(key), Ok(None)),
            "{key:?} still found after delete"
        );
    }

    let actual = table.get_all(|_| true).expect("failed to fetch rows");
    let expected: Vec<Row> = model.into_values().collect();
    assert_eq!(expected, actual);
    TestResult::passed()
}

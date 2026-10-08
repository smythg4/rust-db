use crate::bpm::{BufferPoolManager, ClockEvictor};
use crate::btree::{BTree, BTreeError};
use crate::commontypes::{Key, TableId};
use crate::disk::FileDisk;
use crate::page::{PAGE_SIZE, PageError};
use crate::schema::Column;
use crate::schema::{Row, RowValue, Schema};
use crate::table::{NO_FILTER, Table, TableError};
use crate::traits::{DiskManager, EvictionPolicy};

impl<'t, Dm: DiskManager, Ep: EvictionPolicy> Table<'t, Dm, Ep> {
    fn check_row_keys_order(&self, start: i64, end: i64) -> Result<(), TableError> {
        let expected_count = (end.checked_sub(start).unwrap_or_default().abs() + 1) as usize;
        let start_key = Key::Integer(start);
        let end_key = Key::Integer(end);
        let rows = BTree::new(self).get_range(&start_key, &end_key, |_| true)?;
        let keys: Vec<Key> = rows
            .iter()
            .map(|r| Key::try_from(&r.fields[0]))
            .collect::<Result<_, _>>()
            .expect("stored rows have valid keys");

        assert_eq!(keys.len(), expected_count);
        assert!(
            keys.windows(2).all(|w| w[0] < w[1]),
            "keys aren't strictly increasing"
        );
        Ok(())
    }
}

// test helper to generate a row and insert it
fn insert_row<Dm: DiskManager, Ep: EvictionPolicy>(
    table: &Table<Dm, Ep>,
    row_id: i64,
    email: &str,
    active: bool,
) -> Result<(), TableError> {
    let payload = if email == "NULL" {
        RowValue::Null
    } else {
        RowValue::String(email.into())
    };

    let row = Row::try_from(vec![
        RowValue::Integer(row_id),
        payload,
        RowValue::Boolean(active),
    ])
    .expect("this will work");
    table.insert(row)?;
    Ok(())
}

#[test]
fn table_basics() {
    let _ = env_logger::try_init();
    let path = std::env::temp_dir().join(format!(
        "rust-db-{}-{}.db",
        std::process::id(),
        "table_basics"
    ));
    let _ = std::fs::remove_file(&path); // start clean

    let disk = FileDisk::new(&path).expect("failed to open file");
    let bpm = BufferPoolManager::new(disk, ClockEvictor::default(), 128);
    let schema = Schema::try_from(vec![
        Column::integer("id").unwrap(),
        Column::nullable_string("email").unwrap(),
        Column::bool("active").unwrap(),
    ])
    .expect("failed to build schema");

    let table =
        Table::create(&bpm, TableId::new(1), schema, "Test Table").expect("failed to create table");
    let num_iters: i64 = 100_000;
    let keys_for = |k: i64| (-num_iters..=num_iters).filter(move |n| n.rem_euclid(3) == k);

    std::thread::scope(|s| {
        let writer1 = s.spawn(|| {
            for row_num in keys_for(0).rev() {
                match insert_row(
                    &table,
                    row_num,
                    &format!("user{}@aol.com", row_num.abs()),
                    row_num % 5 == 0,
                ) {
                    Ok(_) => {}
                    Err(TableError::BTree(BTreeError::Page(PageError::DuplicateKey))) => {
                        log::warn!("Duplicate key at {row_num}, skipping...")
                    }
                    Err(e) => panic!("Unexpected Btree error {e:?}"),
                }
            }
        });

        let writer2 = s.spawn(|| {
            for row_num in keys_for(1) {
                match insert_row(
                    &table,
                    row_num,
                    &format!("user{}@yahoo.com", row_num.abs()),
                    row_num % 5 == 0,
                ) {
                    Ok(_) => {}
                    Err(TableError::BTree(BTreeError::Page(PageError::DuplicateKey))) => {
                        log::warn!("Duplicate key at {row_num}, skipping...");
                    }
                    Err(e) => panic!("Unexpected Btree error {e:?}"),
                }
            }
        });

        let writer3 = s.spawn(|| {
            for row_num in keys_for(2).rev() {
                match insert_row(&table, row_num, "NULL", row_num % 5 == 0) {
                    Ok(_) => {}
                    Err(TableError::BTree(BTreeError::Page(PageError::DuplicateKey))) => {
                        log::warn!("Duplicate key at {row_num}, skipping...");
                    }
                    Err(e) => panic!("Unexpected Btree error {e:?}"),
                }
            }
        });

        let reader = s.spawn(|| {
            for i in -num_iters..=num_iters {
                if let Some(thing) = BTree::new(&table).get(&Key::Integer(i)).unwrap() {
                    assert_eq!(
                        thing.cmp_key(&Key::Integer(i)),
                        std::cmp::Ordering::Equal,
                        "keys don't match"
                    );
                }
            }
        });

        writer1.join().unwrap();
        writer2.join().unwrap();
        writer3.join().unwrap();

        table
            .check_row_keys_order(-num_iters, num_iters)
            .expect("failed to check all keys and ordering");

        reader.join().unwrap();

        let remover = s.spawn(|| {
            for i in (-num_iters..=num_iters).rev().skip(9) {
                BTree::new(&table)
                    .delete(&Key::Integer(i))
                    .expect("failed to delete a row");
            }
        });

        remover.join().unwrap();
    });

    table.print_table().expect("failed to print table");

    let rows = BTree::new(&table)
        .get_all(NO_FILTER)
        .expect("failed to fetch rows");

    table.close().expect("failed to close table");
    bpm.close().expect("failed to close bpm");

    let new_disk = FileDisk::new(&path).expect("failed to reopen file");
    let new_bpm = BufferPoolManager::new(new_disk, ClockEvictor::default(), 512);
    let new_table = Table::open(&new_bpm, TableId::new(1)).expect("failed to reopen table");

    let after_rows = BTree::new(&new_table)
        .get_all(NO_FILTER)
        .expect("failed to fetch rows on reload");

    assert_eq!(rows, after_rows, "reload rows were different");

    new_table.vacuum().expect("vacuum failed");

    assert!(
        std::fs::metadata(&path).unwrap().len() <= 3 * PAGE_SIZE as u64,
        "vacuum didn't shrink the file"
    );

    assert_eq!(BTree::new(&new_table).get_all(NO_FILTER).unwrap(), rows);
    new_table.debug_check_space_accounting("post-vacuum reopen");

    new_table.close().expect("failed to close new table");
    new_bpm.close().expect("failed to close new bpm");

    let _ = std::fs::remove_file(&path); // clean up the temp file
    println!("Done!");
}

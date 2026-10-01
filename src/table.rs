use crate::bpm::{BpmError, BufferPoolManager, PageWriteGuard};
use crate::btree::{BTree, BTreeError};
use crate::commontypes::{PageId, TableId};
use crate::page::{Page, PageBody, PageError};
use crate::schema::{Row, Schema, SchemaError};
use crate::traits::{DiskManager, EvictionPolicy};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TableError {
    #[error(transparent)]
    Bpm(#[from] BpmError),
    #[error(transparent)]
    Page(#[from] PageError),
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error(transparent)]
    BTree(#[from] BTreeError),
    #[error("Table already exists: {0}")]
    AlreadyExists(TableId),
}

pub struct Table<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    pub(crate) bpm: &'a BufferPoolManager<Dm, Ep>,
    meta_id: PageId,
    #[allow(dead_code)]
    // only tests right now. maybe package this into always reading from the meta page? though caching is probably fine since it should never change
    schema: Schema,
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> Table<'a, Dm, Ep> {
    /// The only way to get a `Table`: reads and validates the meta page.
    pub fn open(bpm: &'a BufferPoolManager<Dm, Ep>, table_id: TableId) -> Result<Self, TableError> {
        let guard = bpm.fetch_read(PageId::new(table_id, 0))?;
        let meta_id = guard.page_id();
        let schema = guard.meta_get_schema()?.clone();
        Ok(Self {
            bpm,
            meta_id,
            schema,
        })
    }

    /// Writes a fresh meta page plus an empty leaf root
    pub fn create(
        bpm: &'a BufferPoolManager<Dm, Ep>,
        table_id: TableId,
        schema: Schema,
    ) -> Result<Self, TableError> {
        use crate::page::PageBody;
        let meta_id = PageId::new(table_id, 0);
        println!("Opening the file...");
        match bpm.fetch_read(meta_id) {
            Ok(_) => return Err(TableError::AlreadyExists(table_id)),
            Err(BpmError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {} // file isn't populated yet
            Err(BpmError::IoError(e)) if e.kind() == std::io::ErrorKind::NotFound => {} // clear path to make the table
            Err(e) => return Err(e.into()),
        }
        println!("File opened! Making new pages...");
        let root_id = PageId::new(table_id, 1);
        let meta_page = Page::empty_page(
            meta_id,
            PageBody::Meta {
                root_id,
                page_count: 2,
                free_list_head: None,
                schema: schema.clone(),
            },
        );
        let leaf_root = Page::empty_leaf(root_id);

        bpm.new_page(leaf_root)?;
        bpm.new_page(meta_page)?;
        println!("Meta and root pages created. Flushing to disk...");
        bpm.flush_all()?;
        Ok(Self {
            bpm,
            meta_id,
            schema,
        })
    }

    pub fn allocate(&self) -> Result<PageWriteGuard<'_, Dm, Ep>, TableError> {
        let mut meta_guard = self.bpm.fetch_write(self.meta_id)?;

        if let Some(free_list_head) = meta_guard.meta_get_free_list_head()? {
            let mut free_guard = self.bpm.fetch_write(free_list_head)?;
            meta_guard.free_list_pop(&free_guard)?;
            *free_guard = Page::empty_page(free_guard.page_id(), PageBody::Free { next: None });
            Ok(free_guard)
        } else {
            let new_free_id = meta_guard.meta_bump_page_count()?;
            Ok(self
                .bpm
                .new_page(Page::empty_page(new_free_id, PageBody::Free { next: None }))?)
        }
    }

    pub fn free(&self, mut page: PageWriteGuard<'a, Dm, Ep>) -> Result<(), TableError> {
        let mut meta_guard = self.bpm.fetch_write(self.meta_id)?;
        meta_guard.free_list_push(&mut page)?;
        Ok(())
    }

    pub fn root_id(&self) -> Result<PageId, TableError> {
        let id = self.bpm.fetch_read(self.meta_id)?.meta_get_root_id()?;
        Ok(id)
    }

    pub fn set_root_id(&self, id: PageId) -> Result<(), TableError> {
        self.bpm.fetch_write(self.meta_id)?.meta_set_root_id(id)?;
        Ok(())
    }

    pub fn insert(&self, row: Row) -> Result<(), TableError> {
        let row = self.schema.validate_row(row)?;
        BTree::new(self).insert(row)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bpm::Frame;
    use crate::commontypes::{FrameId, Key};
    use crate::disk::FileDisk;
    use crate::page::PageError::DuplicateKey;
    use crate::schema::{Row, RowValue};
    use crate::test_support::leaf_schema;

    #[derive(Default)]
    struct Replacer {
        hand: usize,
    }

    impl EvictionPolicy for Replacer {
        /// TODO: This is just a placeholder until I figure this out...
        fn find_victim(&mut self, frames: &[Frame]) -> Option<FrameId> {
            for _ in 0..frames.len() {
                let id = self.hand;
                self.hand = (self.hand + 1) % frames.len();
                if !frames[id].is_pinned() {
                    return Some(FrameId::new(id));
                }
            }
            None // everything pinned → NoFreeFrames
        }
    }

    impl<'a> Table<'a, FileDisk, Replacer> {
        // helper to generate a row and insert it
        fn insert_row(&self, row_id: i64, content: &str) -> Result<(), TableError> {
            let payload = if content == "NULL" {
                RowValue::Null
            } else {
                RowValue::String(content.into())
            };

            let row =
                Row::try_from(vec![RowValue::Integer(row_id), payload]).expect("this will work");
            self.insert(row)?;
            Ok(())
        }

        fn print_table(&self) -> Result<(), TableError> {
            let rows = BTree::new(self).get_all().expect("failed to fetch rows");
            let header = line(self.schema.columns.iter().map(|c| {
                format!(
                    "{} ({:?}{})",
                    c.name,
                    c.col_type,
                    if c.nullable { "*" } else { "" }
                )
            }));

            let rule = "-".repeat(header.chars().count());

            let print_rows = |rows: &[Row]| {
                for row in rows {
                    println!("{}", line(row.fields.iter().map(|v| v.to_string())));
                }
            };

            println!("{rule}\n{header}\n{rule}");
            if rows.len() <= 10 {
                print_rows(&rows);
            } else {
                print_rows(&rows[..5]);
                println!(
                    "{}",
                    line(self.schema.columns.iter().map(|_| "...".to_string()))
                );
                print_rows(&rows[rows.len() - 5..]);
            }
            println!("{rule}\n({} rows)", rows.len());

            Ok(())
        }

        fn check_row_keys_order(&self, start: i64, end: i64) -> Result<(), TableError> {
            let expected_count = (end.checked_sub(start).unwrap_or_default().abs() + 1) as usize;
            let start_key = Key::Integer(start);
            let end_key = Key::Integer(end);
            let rows = BTree::new(self).get_range(&start_key, &end_key)?;
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

    const COL_WIDTH: usize = 30;

    /// Pads to COL_WIDTH, or cuts with "…" if too long (counts chars, not bytes).
    fn cell(s: &str) -> String {
        if s.chars().count() > COL_WIDTH {
            let cut: String = s.chars().take(COL_WIDTH - 1).collect();
            format!("{cut}…")
        } else {
            format!("{s:<COL_WIDTH$}")
        }
    }

    fn line(cells: impl IntoIterator<Item = String>) -> String {
        let cells: Vec<String> = cells.into_iter().map(|c| cell(&c)).collect();
        format!("| {} |", cells.join(" | "))
    }

    #[test]
    fn table_basics() {
        env_logger::init();
        let path = std::env::temp_dir().join(format!(
            "rust-db-{}-{}.db",
            std::process::id(),
            "table_basics"
        ));
        let _ = std::fs::remove_file(&path); // start clean

        let disk = FileDisk::new(&path).expect("failed to open file");
        let bpm = BufferPoolManager::new(disk, Replacer::default(), 10);
        let mut schema = leaf_schema().0;
        schema.columns[1].nullable = true;
        let table = Table::create(&bpm, TableId::new(1), schema).expect("failed to create table");
        let num_iters: i64 = 100_000;
        let keys_for = |k: i64| (-num_iters..=num_iters).filter(move |n| n.rem_euclid(3) == k);

        std::thread::scope(|s| {
            let writer1 = s.spawn(|| {
                for row_num in keys_for(0).rev() {
                    match table.insert_row(row_num, &format!("user{}@aol.com", row_num.abs())) {
                        Ok(_) => {}
                        Err(TableError::BTree(BTreeError::Page(DuplicateKey))) => {
                            eprintln!("Duplicate key at {row_num}, skipping...")
                        }
                        Err(e) => panic!("Unexpected Btree error {e:?}"),
                    }
                }
            });

            let writer2 = s.spawn(|| {
                for row_num in keys_for(1) {
                    match table.insert_row(row_num, &format!("user{}@yahoo.com", row_num.abs())) {
                        Ok(_) => {}
                        Err(TableError::BTree(BTreeError::Page(DuplicateKey))) => {
                            eprintln!("Duplicate key at {row_num}, skipping...");
                        }
                        Err(e) => panic!("Unexpected Btree error {e:?}"),
                    }
                }
            });

            let writer3 = s.spawn(|| {
                for row_num in keys_for(2).rev() {
                    match table.insert_row(row_num, "NULL") {
                        Ok(_) => {}
                        Err(TableError::BTree(BTreeError::Page(DuplicateKey))) => {
                            eprintln!("Duplicate key at {row_num}, skipping...");
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
                for i in (num_iters - 13)..=(num_iters - 3) {
                    BTree::new(&table)
                        .delete(&Key::Integer(i))
                        .expect("failed to delete a row");
                }
            });

            remover.join().unwrap();
        });

        table.print_table().expect("failed to print table");

        let _ = std::fs::remove_file(&path); // clean up the temp file
        println!("Done!");
    }
}

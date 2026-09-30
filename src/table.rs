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
        match bpm.fetch_read(meta_id) {
            Ok(_) => return Err(TableError::AlreadyExists(table_id)),
            Err(BpmError::IoError(e)) if e.kind() == std::io::ErrorKind::NotFound => {} // clear path to make the table
            Err(e) => return Err(e.into()),
        }
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
    use crate::page::RawPage;
    use crate::schema::{Row, RowValue};
    use crate::test_support::leaf_schema;
    use std::collections::HashMap;
    use std::sync::Mutex;

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

    #[derive(Default)]
    struct FakeDisk {
        stuff: Mutex<HashMap<PageId, RawPage>>,
    }

    impl DiskManager for FakeDisk {
        fn read_page(&self, id: PageId, buf: &mut RawPage) -> std::io::Result<()> {
            let pages = self.stuff.lock().unwrap();
            let page = pages
                .get(&id)
                .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
            buf.copy_from_slice(page);
            Ok(())
        }
        fn write_page(&self, id: PageId, buf: &RawPage) -> std::io::Result<()> {
            self.stuff.lock().unwrap().insert(id, *buf);
            Ok(())
        }
        fn sync(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> Table<'a, FakeDisk, Replacer> {
        // helper to generate a row and insert it
        fn insert_row(&self, row_id: i64, content: &str) -> Result<(), TableError> {
            let row = Row::try_from(vec![
                RowValue::Integer(row_id),
                RowValue::String(content.into()),
            ])
            .expect("this will work");
            self.insert(row)?;
            Ok(())
        }

        #[allow(dead_code)]
        fn print_pages(&self) -> Result<(), TableError> {
            let num_pages = self.bpm.fetch_read(self.meta_id)?.meta_get_page_count()?;
            let table_id = self.bpm.fetch_read(self.meta_id)?.page_id().get_table_id();
            let page_id = |num| PageId::new(table_id, num);

            for i in 0..num_pages as u32 {
                let guard = self.bpm.fetch_read(page_id(i))?;
                println!("{:?}", guard);
            }

            Ok(())
        }

        fn print_and_check_row_keys_order(&self, start: i64, end: i64) -> Result<(), TableError> {
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

            for row in rows {
                println!("{:?}, ", row)
            }
            Ok(())
        }
    }

    #[test]
    fn table_basics() {
        //env_logger::init();
        let bpm = BufferPoolManager::new(FakeDisk::default(), Replacer::default(), 6);
        let table =
            Table::create(&bpm, TableId::new(1), leaf_schema().0).expect("failed to create table");
        let num_iters = 10000;

        std::thread::scope(|s| {
            let h = s.spawn(|| {
                for row_num in (-num_iters..=num_iters).rev() {
                    if table
                        .insert_row(row_num, &format!("stuff in row {row_num}"))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            let h2 = s.spawn(|| {
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
            h.join().unwrap();
            table
                .print_and_check_row_keys_order(-5, 5)
                .expect("failed to print keys");
            h2.join().unwrap();
        });

        println!("Done!");
    }
}

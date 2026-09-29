use crate::bpm::{BpmError, BufferPoolManager, PageWriteGuard};
use crate::commontypes::{PageId, TableId};
use crate::page::{Page, PageError};
use crate::schema::{Schema, SchemaError};
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
    #[error("Table already exists: {0}")]
    AlreadyExists(TableId),
}

pub struct Table<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    bpm: &'a BufferPoolManager<Dm, Ep>,
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

    pub fn allocate(
        &self,
        init: impl FnOnce(PageId) -> Page,
    ) -> Result<PageWriteGuard<'_, Dm, Ep>, TableError> {
        let mut meta_guard = self.bpm.fetch_write(self.meta_id)?;
        if let Some(free_list_head) = meta_guard.meta_get_free_list_head()? {
            let mut free_guard = self.bpm.fetch_write(free_list_head)?;
            meta_guard.free_list_pop(&free_guard)?;
            *free_guard = init(free_list_head);
            Ok(free_guard)
        } else {
            let new_free_id = meta_guard.meta_bump_page_count()?;
            Ok(self.bpm.new_page(init(new_free_id))?)
        }
    }

    pub fn free(&self, mut page: PageWriteGuard<'a, Dm, Ep>) -> Result<(), TableError> {
        let mut meta_guard = self.bpm.fetch_write(self.meta_id)?;
        meta_guard.free_list_push(&mut page)?;
        Ok(())
    }

    pub fn root_id(&self) -> Result<PageId, BpmError> {
        let id = self.bpm.fetch_read(self.meta_id)?.meta_get_root_id()?;
        Ok(id)
    }

    pub fn set_root_id(&self, id: PageId) -> Result<(), TableError> {
        self.bpm.fetch_write(self.meta_id)?.meta_set_root_id(id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commontypes::FrameId;
    use crate::page::RawPage;
    use crate::schema::{Row, RowValue};
    use crate::test_support::leaf_schema;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct Replacer;

    impl EvictionPolicy for Replacer {
        /// TODO: This is just a placeholder until I figure this out...
        fn find_victim(&self) -> Option<FrameId> {
            Some(FrameId::new(0))
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
        fn insert_row(
            &self,
            page_id: PageId,
            row_id: i64,
            content: &str,
        ) -> Result<(), TableError> {
            let row = Row::try_from(vec![
                RowValue::Integer(row_id),
                RowValue::String(content.into()),
            ])
            .expect("this will work");
            let vr = self.schema.validate_row(row)?;
            let mut page = self.bpm.fetch_write(page_id).expect("fetch write failed");
            page.leaf_insert(vr)?;
            Ok(())
        }

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
    }

    #[test]
    fn table_basics() {
        let bpm = BufferPoolManager::new(FakeDisk::default(), Replacer, 2);
        let table = Table::create(&bpm, TableId::new(999), leaf_schema().0)
            .expect("failed to create table");
        let root_id = table.root_id().expect("failed to get root id");

        for row_num in -100..100 {
            if table
                .insert_row(root_id, row_num, &format!("stuff in row {row_num}"))
                .is_err()
            {
                break;
            }
        }

        table.print_pages().expect("failed to print pages");
        println!("Done!");
    }
}

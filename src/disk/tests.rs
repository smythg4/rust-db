use crate::commontypes::PageId;
use crate::page::{Page, RawPage};
use crate::traits::DiskManager;

use std::collections::HashMap;
use std::sync::Mutex;

#[allow(dead_code)] // used for tests only
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

    fn swap_file(&self, pages: Vec<Page>) -> Result<(), std::io::Error> {
        let mut new_hash = HashMap::new();
        for page in pages {
            new_hash.insert(
                page.page_id(),
                page.as_raw_page().expect("failed to serialize in swap"),
            );
        }
        *self.stuff.lock().unwrap() = new_hash;
        Ok(())
    }
}

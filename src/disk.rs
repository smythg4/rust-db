use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::commontypes::PageId;
use crate::page::{PAGE_SIZE, RawPage};
use crate::traits::DiskManager;

pub struct FileDisk {
    file: Arc<RwLock<File>>,
}

impl FileDisk {
    // TODO: Add support for one file per table
    pub fn new(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref();
        // this will result in a fresh file each time
        let f = OpenOptions::new()
            .read(true)
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Arc::new(RwLock::new(f)),
        })
    }
}

impl DiskManager for FileDisk {
    fn read_page(&self, id: PageId, buf: &mut RawPage) -> std::io::Result<()> {
        let file_offset = id.get_page_num() as u64 * PAGE_SIZE as u64;
        let guard = self.file.read().unwrap();
        guard.read_exact_at(buf, file_offset)?;
        Ok(())
    }

    fn write_page(&self, id: PageId, buf: &RawPage) -> std::io::Result<()> {
        let file_offset = id.get_page_num() as u64 * PAGE_SIZE as u64;
        let guard = self.file.write().unwrap();
        guard.write_all_at(buf, file_offset)?;
        Ok(())
    }

    fn sync(&self) -> std::io::Result<()> {
        let guard = self.file.write().unwrap();
        let size = guard.metadata().unwrap().size() as f64 / (1024 * 1024) as f64;
        println!("File Size: {size:.2}MB");
        guard.sync_all()
    }
}

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

#[allow(dead_code)]
#[cfg(test)]
#[derive(Default)]
struct FakeDisk {
    stuff: Mutex<HashMap<PageId, RawPage>>,
}
#[cfg(test)]
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

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::page::{PAGE_SIZE, Page, RawPage};
use crate::traits::{DiskManager, Serializable};
use crate::types::PageId;

pub struct FileDisk {
    file: Arc<RwLock<File>>,
    path: PathBuf,
}

impl FileDisk {
    // TODO: Add support for one file per table
    pub fn new(path: impl AsRef<Path>) -> Result<Self, std::io::Error> {
        let path = path.as_ref();
        let f = OpenOptions::new()
            .read(true)
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)?;
        Ok(Self {
            file: Arc::new(RwLock::new(f)),
            path: path.into(),
        })
    }
}

impl DiskManager for FileDisk {
    fn read_page(&self, id: PageId, buf: &mut RawPage) -> std::io::Result<()> {
        let file_offset = id.get_page_num() as u64 * PAGE_SIZE as u64;
        let guard = self.file.read().unwrap();
        if file_offset >= guard.metadata()?.len() {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound)); // the page was never written
        }
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
        guard.sync_all()
    }

    fn swap_file(&self, pages: Vec<Page>) -> Result<(), std::io::Error> {
        let temp_path = self.path.with_extension("vac");
        let mut temp_file = std::fs::File::create(&temp_path)?;
        for page in pages {
            temp_file.seek(SeekFrom::Start(
                page.page_id().get_page_num() as u64 * PAGE_SIZE as u64,
            ))?;
            page.serialize(&mut temp_file).expect("write failed");
        }
        temp_file.sync_all()?;
        std::fs::rename(&temp_path, &self.path)?;
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?; // make the rename itself durable
        }
        let mut guard = self.file.write().unwrap();
        *guard = OpenOptions::new().read(true).write(true).open(&self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::FakeDisk;

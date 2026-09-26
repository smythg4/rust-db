use crate::page::PAGE_SIZE;
use std::path::Path;
use std::file::OpenOptions;

pub const BUFFER_SIZE: usize = 512;


// Dummy structs to hold me over until I actually make them
struct Wal;
struct EvictionPolicy;


pub struct BufferPoolManagerBuilder {
    has_wal: Option<Wal>,
    file_path: Option<Path>,
}

impl BufferPoolManagerBuilder {
    pub fn new() -> Self {
        Self {
            wal: None,
            file_path: None,
        }
    }

    pub fn with_wal(mut self, wal: Wal) -> Self {
        self.wal = Some(wal);
        self
    }

    pub fn with_file_backing<P: impl AsRef<Path>>(mut self, filepath: P) -> Self {
        let filepath = filepath.as_ref();
        self.file_path = Some(filepath);
        self
    }

    pub fn build(self) -> BufferPoolManager {
        let fp = self.file_path.unwrap();
        let persistant_layer = File::open(fp).unwrap();
        let eviction_policy = ClockEvictor;
        let wal = self.wal.unwrap_or_default();
        const EMPTY: RwLock<Option<Box<PageFrame>>> = RwLock::New(None);
        BufferPoolManager {
            persistant_layer,
            eviction_policy,
            wal,
            frames: [EMPTY; BUFFER_SIZE],
        }
    }
}

pub(crate) struct BufferPoolManager<Dm: DiskManager> {
    persistant_layer: Dm,
    eviction_policy: EvictionPolicy,
    wal: Option<Wal>,
    frames: [RwLock<Option<Box<PageFrame>>>; BUFFER_SIZE],
}
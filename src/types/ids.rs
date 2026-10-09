use crate::traits::Serializable;
use crate::types::PAGE_ID_SIZE;
use std::io::{Read, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PageId(TableId, u32);
impl PageId {
    pub const fn new(table_id: TableId, page_num: u32) -> Self {
        Self(table_id, page_num)
    }
    pub const fn get_table_id(self) -> TableId {
        self.0
    }
    pub const fn get_page_num(self) -> u32 {
        self.1
    }
    pub fn wrapping_add(&self, n: u32) -> Self {
        PageId(self.0, self.1.wrapping_add(n))
    }
}

impl std::fmt::Display for PageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0, self.1) // TableId:page_num, using TableId's own Display
    }
}

impl Serializable for PageId {
    type Error = std::io::Error;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        self.get_table_id().serialize(w)?;
        w.write_all(&self.get_page_num().to_be_bytes())?;
        Ok(())
    }
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let table_id = TableId::deserialize(r)?;

        let mut page_buf = [0u8; 4];
        r.read_exact(&mut page_buf)?;
        let page_num = u32::from_be_bytes(page_buf);

        Ok(PageId::new(table_id, page_num))
    }
    fn encoded_size(&self) -> usize {
        PAGE_ID_SIZE
    }
}

// Generates the basic tuple types
macro_rules! id_type {
    ($name:ident, $inner:ty) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name($inner);

        impl $name {
            pub const fn new(id: $inner) -> Self {
                Self(id)
            }
            pub const fn get(self) -> $inner {
                self.0
            }
        }
        impl From<$inner> for $name {
            fn from(id: $inner) -> Self {
                Self(id)
            }
        }
        impl From<$name> for $inner {
            fn from(id: $name) -> Self {
                id.0
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

macro_rules! id_type_serializable {
    ($name:ident, $inner:ty) => {
        id_type!($name, $inner);

        impl Serializable for $name {
            type Error = std::io::Error;
            fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
                w.write_all(&self.0.to_be_bytes())?;
                Ok(())
            }
            fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
                let mut buf = [0u8; std::mem::size_of::<$inner>()];
                r.read_exact(&mut buf)?;
                Ok(Self(<$inner>::from_be_bytes(buf)))
            }
            fn encoded_size(&self) -> usize {
                std::mem::size_of::<$inner>()
            }
        }
    };
}

id_type_serializable!(TableId, u32);
id_type!(FrameId, usize); // stays non-serializable
id_type_serializable!(SlotIndex, u16);
id_type_serializable!(TransactionId, u64);

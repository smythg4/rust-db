use crate::schema::{RowValue, RowValueError};
use crate::traits::Serializable;
use integer_encoding::*;
use std::io::{Read, Write};
use thiserror::Error;

pub const SLOT_ENTRY_SIZE: usize = 4; // two u16s
pub const PAGE_ID_SIZE: usize = 8; // two u32s

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RecordId(PageId, SlotIndex);

impl RecordId {
    pub const fn new(page_id: PageId, slot_index: SlotIndex) -> Self {
        Self(page_id, slot_index)
    }
    pub const fn get_page_id(self) -> PageId {
        self.0
    }
    pub const fn get_slot_index(self) -> SlotIndex {
        self.1
    }
}

impl std::fmt::Display for RecordId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0, self.1) // PageId:SlotIndex, composes once PageId has Display
    }
}

#[derive(Error, Debug)]
pub enum LsnError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("LSNs can't be 0")]
    ZeroLsn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lsn(u64);
impl Lsn {
    pub const fn new(id: u64) -> Result<Self, LsnError> {
        if id == 0 {
            return Err(LsnError::ZeroLsn);
        }
        Ok(Lsn(id))
    }
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<Lsn> for u64 {
    fn from(value: Lsn) -> Self {
        value.0
    }
}

impl std::convert::TryFrom<u64> for Lsn {
    type Error = LsnError;
    fn try_from(id: u64) -> Result<Self, Self::Error> {
        Self::new(id)
    }
}

impl std::fmt::Display for Lsn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Serializable for Lsn {
    type Error = LsnError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut buf = [0u8; 8];
        r.read_exact(&mut buf)?;
        Lsn::new(u64::from_be_bytes(buf))
    }

    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        w.write_all(&self.0.to_be_bytes())?;
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        size_of::<u64>()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotEntry {
    pub offset: u16,
    pub length: u16,
}

impl SlotEntry {
    pub const fn new(offset: u16, length: u16) -> Self {
        Self { offset, length }
    }

    pub fn range(&self) -> std::ops::Range<usize> {
        self.offset as usize..(self.offset as usize + self.length as usize)
    }
}

impl Serializable for SlotEntry {
    type Error = std::io::Error;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        w.write_all(&self.offset.to_be_bytes())?;
        w.write_all(&self.length.to_be_bytes())?;
        Ok(())
    }
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut offset_buf = [0u8; 2];
        r.read_exact(&mut offset_buf)?;
        let mut length_buf = [0u8; 2];
        r.read_exact(&mut length_buf)?;
        Ok(Self::new(
            u16::from_be_bytes(offset_buf),
            u16::from_be_bytes(length_buf),
        ))
    }
    fn encoded_size(&self) -> usize {
        SLOT_ENTRY_SIZE
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
                1 + std::mem::size_of::<$inner>()
            }
        }
    };
}

id_type_serializable!(TableId, u32);
id_type!(FrameId, usize); // stays non-serializable
id_type_serializable!(SlotIndex, u16);
id_type_serializable!(TransactionId, u64);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Integer(i64),
    String(String),
}

#[derive(Error, Debug)]
pub enum KeyError {
    #[error("Float fields cannot be used as keys")]
    NotOrderable,
    #[error("Key cannot be null")]
    NullKey,
    #[error(transparent)]
    RowValue(#[from] RowValueError),
    #[error("Seriously? A boolean as a key?")]
    BoolKey,
}

impl TryFrom<&RowValue> for Key {
    type Error = KeyError;
    fn try_from(value: &RowValue) -> Result<Self, Self::Error> {
        match value {
            RowValue::Integer(n) => Ok(Self::Integer(*n)),
            RowValue::String(s) => Ok(Self::String(s.clone())),
            RowValue::Null => Err(KeyError::NullKey),
            RowValue::Float(_) => Err(KeyError::NotOrderable),
            RowValue::Boolean(_) => Err(KeyError::BoolKey),
        }
    }
}

impl From<Key> for RowValue {
    fn from(key: Key) -> Self {
        match key {
            Key::Integer(n) => RowValue::Integer(n),
            Key::String(s) => RowValue::String(s),
        }
    }
}

impl Serializable for Key {
    type Error = KeyError;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        let field: RowValue = self.clone().into();
        field.serialize(w)?;
        Ok(())
    }

    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let field = &RowValue::deserialize(r)?;
        field.try_into()
    }

    fn encoded_size(&self) -> usize {
        match self {
            Self::Integer(_) => 1 + size_of::<i64>(),
            Self::String(s) => {
                let length = s.len();
                let varlen = length.encode_var_vec().len();
                1 + varlen + length
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::{Arbitrary, Gen, TestResult};
    use quickcheck_macros::quickcheck;
    use std::io::Cursor;

    impl Arbitrary for Key {
        fn arbitrary(g: &mut Gen) -> Self {
            let num = g.choose(&[0, 1]).unwrap();
            match num {
                0 => Key::Integer(i64::arbitrary(g)),
                1 => Key::String(String::arbitrary(g)),
                _ => unreachable!(),
            }
        }
    }

    #[quickcheck]
    fn key_encoded_sizes(key: Key) -> TestResult {
        let mut bytes = Cursor::new(Vec::new());
        key.serialize(&mut bytes).unwrap();

        assert_eq!(key.encoded_size(), bytes.into_inner().len());
        TestResult::passed()
    }

    #[test]
    fn page_id_size_constant_is_accurate() {
        let mut buffer = Vec::with_capacity(PAGE_ID_SIZE);
        let tid = TableId::new(0);
        let pid = PageId::new(tid, 0);
        pid.serialize(&mut buffer).unwrap();
        assert_eq!(PAGE_ID_SIZE, buffer.len());
    }

    #[test]
    fn slot_entry_size_constant_is_accurate() {
        let mut buffer = Vec::with_capacity(SLOT_ENTRY_SIZE);
        let se = SlotEntry::new(0, 0);
        se.serialize(&mut buffer).unwrap();
        assert_eq!(SLOT_ENTRY_SIZE, buffer.len());
    }

    #[test]
    fn zero_lsns_trigger_errors() {
        let result = Lsn::new(0);
        assert!(matches!(result, Err(LsnError::ZeroLsn)))
    }

    #[test]
    fn null_and_float_keys_trigger_errors() {
        let null_result = Key::try_from(&RowValue::Null);
        let float_result = Key::try_from(&RowValue::Float(10.0));

        assert!(matches!(null_result, Err(KeyError::NullKey)));
        assert!(matches!(float_result, Err(KeyError::NotOrderable)));
    }
}

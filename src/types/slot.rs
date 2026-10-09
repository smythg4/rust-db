use crate::traits::Serializable;
use crate::types::SLOT_ENTRY_SIZE;
use std::io::{Read, Write};

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

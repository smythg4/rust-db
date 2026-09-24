use std::io::{Read, Write};
pub trait Serializable
where
    Self: Sized,
{
    type Error;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error>;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error>;
    fn encoded_size(&self) -> usize;
}

/// Handles Option types by pre-pending entries with a '0' u8 for `None` or a
/// '1' u8 for `Some`.
impl<T: Serializable> Serializable for Option<T>
where
    T::Error: From<std::io::Error>,
{
    type Error = T::Error;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        match self {
            Some(v) => {
                w.write_all(&[1u8])?;
                v.serialize(w)
            }
            None => {
                w.write_all(&[0u8])?;
                Ok(())
            }
        }
    }

    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut tag = [0u8; 1];
        r.read_exact(&mut tag)?;
        match tag[0] {
            0 => Ok(None),
            _ => Ok(Some(T::deserialize(r)?)),
        }
    }

    fn encoded_size(&self) -> usize {
        match self {
            Some(v) => v.encoded_size() + 1,
            None => 1,
        }
    }
}

/*
TODO: Pick this up later so we can have an implementation that works directly on
raw bytes instead of pulling everything into memory.

 pub trait Page: Serializable {
    type Record;
    /// Returns the number of bytes free in the Page
    fn free_space(&self) -> usize;

    /// Returns a reference to an underlying record
    fn get_record(&self, slot: SlotIndex) -> Option<Self::Record>;

    /// Inserts a record into the Page
    fn insert_record(&mut self, record: Self::Record) -> Result<SlotIndex, PageError>;

    /// Deletes a record from the Page
    fn delete_record(&mut self, slot: SlotIndex) -> Result<(), PageError>;
 }
 */

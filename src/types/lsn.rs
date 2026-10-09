use crate::traits::Serializable;
use std::io::{Read, Write};
use thiserror::Error;

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

#[derive(Debug, Clone, PartialEq)]
pub struct PageLsn(pub Option<Lsn>);

impl Serializable for PageLsn {
    type Error = LsnError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut buf = [0u8; 8];
        r.read_exact(&mut buf)?;
        let val = u64::from_be_bytes(buf);
        Ok(if val == 0 {
            PageLsn(None)
        } else {
            PageLsn(Some(Lsn::new(val).unwrap()))
        })
    }

    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        match self.0 {
            Some(lsn) => w.write_all(&lsn.0.to_be_bytes()),
            None => w.write_all(&0u64.to_be_bytes()),
        }?;
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        size_of::<u64>()
    }
}

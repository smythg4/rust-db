use crate::schema::{RowValue, RowValueError};
use crate::traits::Serializable;
use integer_encoding::*;
use std::io::{Read, Write};
use thiserror::Error;

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
                let varlen = length.required_space();
                1 + varlen + length
            }
        }
    }
}

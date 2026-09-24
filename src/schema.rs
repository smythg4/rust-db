use crate::traits::Serializable;
use integer_encoding::*;
use std::io::{Read, Write};
use std::string::FromUtf8Error;
use thiserror::Error;

const NULL_FLAG: u8 = 0;
const INT_FLAG: u8 = 1;
const FLOAT_FLAG: u8 = 2;
const STRING_FLAG: u8 = 3;

const MAX_FIELD_LEN: usize = 4096 / 2;
const MAX_NUM_FIELDS: usize = 255;

#[derive(Error, Debug)]
pub enum RowValueError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error(transparent)]
    Utf8Error(#[from] FromUtf8Error),
    #[error("Unknown schema field tag: {0}")]
    UnknownTag(u8),
    #[error("Field length: {0}. Fields cannot be longer than {MAX_FIELD_LEN}")]
    FieldTooLong(usize),
    #[error("Too many fields: {0}. Only {MAX_NUM_FIELDS} allowed")]
    TooManyFields(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowValue {
    Integer(i64),
    Float(f64),
    String(String),
    Null,
}

impl RowValue {
    pub fn get_tag(&self) -> u8 {
        match self {
            Self::Integer(_) => INT_FLAG,
            Self::Float(_) => FLOAT_FLAG,
            Self::String(_) => STRING_FLAG,
            Self::Null => NULL_FLAG,
        }
    }
}

impl Serializable for RowValue {
    type Error = RowValueError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, RowValueError> {
        let mut tag_buf = [0u8; 1];
        let mut eight_buf = [0u8; 8];
        r.read_exact(&mut tag_buf)?;
        let tag = tag_buf[0];
        match tag {
            INT_FLAG => {
                r.read_exact(&mut eight_buf)?;
                Ok(Self::Integer(i64::from_be_bytes(eight_buf)))
            }
            FLOAT_FLAG => {
                r.read_exact(&mut eight_buf)?;
                Ok(Self::Float(f64::from_be_bytes(eight_buf)))
            }
            STRING_FLAG => {
                let len = r.read_varint()?;
                if len > MAX_FIELD_LEN {
                    return Err(RowValueError::FieldTooLong(len));
                }
                let mut s = vec![0u8; len];
                r.read_exact(&mut s)?;
                Ok(Self::String(String::from_utf8(s)?))
            }
            NULL_FLAG => Ok(Self::Null),
            _ => Err(RowValueError::UnknownTag(tag)),
        }
    }

    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), RowValueError> {
        match self {
            RowValue::Integer(n) => {
                w.write_all(&INT_FLAG.to_be_bytes())?;
                w.write_all(&n.to_be_bytes())?;
            }
            Self::Float(f) => {
                w.write_all(&FLOAT_FLAG.to_be_bytes())?;
                w.write_all(&f.to_be_bytes())?;
            }
            RowValue::String(s) => {
                w.write_all(&STRING_FLAG.to_be_bytes())?;
                let length = s.len();
                if length > MAX_FIELD_LEN {
                    return Err(RowValueError::FieldTooLong(length));
                }
                let varlen = length.encode_var_vec();
                w.write_all(&varlen)?;
                w.write_all(s.as_bytes())?;
            }
            RowValue::Null => w.write_all(&NULL_FLAG.to_be_bytes())?,
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub(crate) fields: Vec<RowValue>,
}

impl Serializable for Row {
    type Error = RowValueError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut num_fields_buf = [0u8; 1];
        r.read_exact(&mut num_fields_buf)?;
        let num_fields = num_fields_buf[0] as usize;

        if num_fields > MAX_NUM_FIELDS {
            return Err(RowValueError::TooManyFields(num_fields));
        }

        let mut fields = Vec::with_capacity(num_fields);
        for _ in 0..num_fields {
            let field = RowValue::deserialize(r)?;
            fields.push(field);
        }

        Ok(Self { fields })
    }

    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        if self.fields.len() > MAX_NUM_FIELDS {
            return Err(RowValueError::TooManyFields(self.fields.len()));
        }
        let num_fields = self.fields.len() as u8;
        w.write_all(&num_fields.to_be_bytes())?;

        for f in &self.fields {
            f.serialize(w)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::TestResult;
    use quickcheck_macros::quickcheck;
    use std::io::{Cursor, Seek};

    #[test]
    fn roundtrip_basic() {
        let mut fields = Vec::new();
        for i in 0..10 {
            fields.push(RowValue::Integer(i));
        }
        for i in 0..10 {
            fields.push(RowValue::Float(i as f64 / 100.0));
        }
        for i in 0..10 {
            fields.push(RowValue::String(format!("String {i}")));
        }
        for _ in 0..10 {
            fields.push(RowValue::Null);
        }
        let row = Row { fields };
        let mut bytes = Cursor::new(Vec::new());
        row.serialize(&mut bytes).unwrap();

        bytes.seek(std::io::SeekFrom::Start(0)).unwrap();
        let deser = Row::deserialize(&mut bytes).unwrap();

        assert_eq!(row, deser);
    }

    #[test]
    fn too_many_row_entries_triggers_error() {
        let mut fields = Vec::new();
        for i in 0..MAX_NUM_FIELDS + 1 {
            fields.push(RowValue::Integer(i as i64));
        }
        let row = Row { fields };
        let mut bytes = Cursor::new(Vec::new());
        let ser_result = row.serialize(&mut bytes);

        assert!(ser_result.is_err());
        assert!(matches!(
            ser_result.unwrap_err(),
            RowValueError::TooManyFields(256)
        ));
    }

    #[test]
    fn too_long_field_triggers_error() {
        let row = Row {
            fields: vec![RowValue::String("a".repeat(MAX_FIELD_LEN + 1))],
        };
        let mut bytes = Cursor::new(Vec::new());
        let ser_result = row.serialize(&mut bytes);

        assert!(ser_result.is_err());
        assert!(matches!(
            ser_result.unwrap_err(),
            RowValueError::FieldTooLong(2049)
        ));
    }

    #[quickcheck]
    fn invalid_tags_trigger_error(tag: u8) -> TestResult {
        if [STRING_FLAG, INT_FLAG, FLOAT_FLAG, NULL_FLAG].contains(&tag) {
            return TestResult::discard();
        }

        let mut bytes = vec![0u8; 9];
        bytes[0] = tag;

        let deser_res = RowValue::deserialize(&mut Cursor::new(bytes));

        assert!(deser_res.is_err());
        match deser_res {
            Err(RowValueError::UnknownTag(t)) if t == tag => TestResult::passed(),
            _ => TestResult::failed(),
        }
    }
}

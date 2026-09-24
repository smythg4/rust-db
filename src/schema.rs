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

    pub fn column_type(&self) -> Option<ColumnType> {
        match self {
            Self::Float(_) => Some(ColumnType::Float),
            Self::Integer(_) => Some(ColumnType::Integer),
            Self::String(_) => Some(ColumnType::String),
            Self::Null => None,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Float,
    String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    col_type: ColumnType,
    nullable: bool,
}

#[derive(Error, Debug)]
pub enum SchemaError {
    #[error("Type in Row does not conform to type in schema column {0}")]
    TypeMismatch(usize),
    #[error("Row doesn't have the right number of columns")]
    ColumnCountMismatch,
    #[error("Null value in non-nullable column {0}")]
    NullValueInNonNullCol(usize),
}

pub struct ValidatedRow(Row);

impl From<ValidatedRow> for Row {
    fn from(value: ValidatedRow) -> Self {
        value.0
    }
}
pub struct Schema {
    columns: Vec<Column>,
}

impl Schema {
    pub fn validate_row(&self, row: Row) -> Result<ValidatedRow, SchemaError> {
        let cols = &self.columns;
        let values = &row.fields;
        if cols.len() != values.len() {
            return Err(SchemaError::ColumnCountMismatch);
        }

        for (i, (col, value)) in cols.iter().zip(values.iter()).enumerate() {
            match value.column_type() {
                Some(c) if c == col.col_type => {}
                None if col.nullable => {}
                None if !col.nullable => return Err(SchemaError::NullValueInNonNullCol(i)),
                _ => return Err(SchemaError::TypeMismatch(i)),
            }
        }

        Ok(ValidatedRow(row))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quickcheck::{Arbitrary, Gen, TestResult};
    use quickcheck_macros::quickcheck;
    use std::io::{Cursor, Seek};

    impl Arbitrary for RowValue {
        fn arbitrary(g: &mut Gen) -> Self {
            let num = g.choose(&[0, 1, 2, 3]).unwrap();
            match num {
                0 => RowValue::Integer(i64::arbitrary(g)),
                1 => {
                    let mut f = f64::arbitrary(g);
                    while f.is_nan() {
                        f = f64::arbitrary(g);
                    }
                    RowValue::Float(f)
                }
                2 => RowValue::String(String::arbitrary(g)),
                3 => RowValue::Null,
                _ => unreachable!(),
            }
        }
    }

    #[quickcheck]
    fn roundtrip_basic(fields: Vec<RowValue>) -> TestResult {
        let row = Row { fields };
        let mut bytes = Cursor::new(Vec::new());
        row.serialize(&mut bytes).unwrap();

        bytes.seek(std::io::SeekFrom::Start(0)).unwrap();
        let deser = Row::deserialize(&mut bytes).unwrap();

        assert_eq!(row, deser);
        TestResult::passed()
    }

    #[test]
    fn too_many_row_entries_triggers_error() {
        let mut fields = Vec::new();
        for _ in 0..MAX_NUM_FIELDS + 1 {
            fields.push(RowValue::Null);
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

    #[test]
    fn valid_rows_pass_validation() {
        let schema = Schema {
            columns: vec![
                Column {
                    col_type: ColumnType::Integer,
                    nullable: false,
                },
                Column {
                    col_type: ColumnType::String,
                    nullable: true,
                },
                Column {
                    col_type: ColumnType::Float,
                    nullable: true,
                },
            ],
        };

        let all_present = Row {
            fields: vec![
                RowValue::Integer(1),
                RowValue::String("hello".to_string()),
                RowValue::Float(1.5),
            ],
        };
        assert!(schema.validate_row(all_present).is_ok());

        // NULL is fine in nullable columns
        let with_nulls = Row {
            fields: vec![RowValue::Integer(2), RowValue::Null, RowValue::Null],
        };
        assert!(schema.validate_row(with_nulls).is_ok());
    }

    #[test]
    fn invalid_rows_fail_validation() {
        let schema = Schema {
            columns: vec![
                Column {
                    col_type: ColumnType::Integer,
                    nullable: false,
                },
                Column {
                    col_type: ColumnType::String,
                    nullable: true,
                },
            ],
        };

        let wrong_type = Row {
            fields: vec![RowValue::Integer(1), RowValue::Float(1.5)],
        };
        assert!(matches!(
            schema.validate_row(wrong_type),
            Err(SchemaError::TypeMismatch(1))
        ));

        let null_in_non_null = Row {
            fields: vec![RowValue::Null, RowValue::String("x".to_string())],
        };
        assert!(matches!(
            schema.validate_row(null_in_non_null),
            Err(SchemaError::NullValueInNonNullCol(0))
        ));

        let too_short = Row {
            fields: vec![RowValue::Integer(1)],
        };
        assert!(matches!(
            schema.validate_row(too_short),
            Err(SchemaError::ColumnCountMismatch)
        ));
    }
}

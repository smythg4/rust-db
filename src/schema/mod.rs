#[cfg(test)]
pub(crate) mod tests;

use crate::commontypes::Key;
use crate::page::{MAX_INTERNAL_ENTRY_SIZE, MAX_LEAF_ENTRY_SIZE, Page};
use crate::traits::Serializable;
use integer_encoding::*;
use std::io::{Read, Write};
use std::string::FromUtf8Error;
use thiserror::Error;

const NULL_FLAG: u8 = 0;
const INT_FLAG: u8 = 1;
const FLOAT_FLAG: u8 = 2;
const STRING_FLAG: u8 = 3;
const BOOL_FLAG: u8 = 4;

const MAX_FIELD_LEN: usize = MAX_LEAF_ENTRY_SIZE - 1 - 2; // max entry minus 1 byte tag and 2 byte varint
pub(crate) const MAX_NUM_FIELDS: usize = 255;

#[derive(Error, Debug)]
pub enum RowValueError {
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("Unknown schema field tag: {0}")]
    UnknownTag(u8),
    #[error("Field length: {0}. Fields cannot be longer than {MAX_FIELD_LEN}")]
    FieldTooLong(usize),
    #[error("Too many fields: {0}. Only {MAX_NUM_FIELDS} allowed")]
    TooManyFields(usize),
    #[error("Booleans must come from bytes holding '0' or '1', got {0}")]
    InvalidBool(u8),
    #[error(transparent)]
    Utf8Error(#[from] FromUtf8Error),
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowValue {
    Integer(i64),
    Float(f64),
    String(String),
    Boolean(bool),
    Null,
}

impl std::fmt::Display for RowValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Integer(n) => write!(f, "{n}"),
            Self::Float(n) => write!(f, "{n}"),
            Self::Boolean(b) => write!(f, "{b}"),
            Self::Null => write!(f, "NULL"),
            Self::String(s) => write!(f, "'{s}'"),
        }
    }
}

impl RowValue {
    pub fn column_type(&self) -> Option<ColumnType> {
        match self {
            Self::Float(_) => Some(ColumnType::Float),
            Self::Integer(_) => Some(ColumnType::Integer),
            Self::String(_) => Some(ColumnType::String),
            Self::Boolean(_) => Some(ColumnType::Bool),
            Self::Null => None,
        }
    }
}

impl Serializable for RowValue {
    type Error = RowValueError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, RowValueError> {
        let mut one_buf = [0u8; 1];
        let mut eight_buf = [0u8; 8];
        r.read_exact(&mut one_buf)?;
        let tag = one_buf[0];
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
            BOOL_FLAG => {
                r.read_exact(&mut one_buf)?;
                match one_buf[0] {
                    0 => Ok(Self::Boolean(false)),
                    1 => Ok(Self::Boolean(true)),
                    _ => Err(RowValueError::InvalidBool(one_buf[0])),
                }
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
                if s.len() > MAX_FIELD_LEN {
                    return Err(RowValueError::FieldTooLong(s.len()));
                }
                w.write_all(&STRING_FLAG.to_be_bytes())?;
                w.write_all(&s.len().encode_var_vec())?;
                w.write_all(s.as_bytes())?;
            }
            RowValue::Boolean(b) => {
                w.write_all(&BOOL_FLAG.to_be_bytes())?;
                match b {
                    true => w.write_all(&1u8.to_be_bytes()),
                    false => w.write_all(&0u8.to_be_bytes()),
                }?;
            }
            RowValue::Null => w.write_all(&NULL_FLAG.to_be_bytes())?,
        }
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        match self {
            Self::Float(_) => 1 + size_of::<f64>(),
            Self::Integer(_) => 1 + size_of::<i64>(),
            Self::Boolean(_) => 1 + size_of::<u8>(),
            Self::String(s) => {
                let length = s.len();
                let varlen = length.required_space();
                1 + varlen + length
            }
            Self::Null => 1,
        }
    }
}

#[derive(Error, Debug)]
pub enum RowError {
    #[error("Cannot construct Row from empty list")]
    EmptyRow,
    #[error("First entry must be a valid primary key")]
    InvalidFirstEntry,
}
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub(crate) fields: Vec<RowValue>,
}

impl Row {
    /// Compares this row's primary key (its first field) against `key`.
    ///
    /// The result is how `self` orders relative to `key`, which is the
    /// orientation `binary_search_by` expects, so no `.reverse()` is needed.
    ///
    /// Panics if the first field isn't a valid key. Stored rows passed schema
    /// validation, so reaching that arm means the page is corrupt.
    pub fn cmp_key(&self, key: &Key) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self.fields.first(), key) {
            (Some(RowValue::Integer(a)), Key::Integer(b)) => a.cmp(b),
            (Some(RowValue::String(a)), Key::String(b)) => a.as_str().cmp(b.as_str()),
            // Mirror Key's derived Ord, where Integer sorts before String.
            (Some(RowValue::Integer(_)), Key::String(_)) => Ordering::Less,
            (Some(RowValue::String(_)), Key::Integer(_)) => Ordering::Greater,
            (other, _) => panic!("row has invalid primary key {other:?}; page is corrupt"),
        }
    }
}

impl TryFrom<Vec<RowValue>> for Row {
    type Error = RowError;
    fn try_from(fields: Vec<RowValue>) -> Result<Self, Self::Error> {
        if fields.is_empty() {
            Err(RowError::EmptyRow)
        } else if let Some(key_row) = fields.first()
            && Key::try_from(key_row).is_ok()
        {
            Ok(Row { fields })
        } else {
            Err(RowError::InvalidFirstEntry)
        }
    }
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

    fn encoded_size(&self) -> usize {
        1 + self.fields.iter().map(|f| f.encoded_size()).sum::<usize>()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Integer,
    Float,
    String,
    Bool,
}

impl ColumnType {
    pub(crate) fn is_valid_key(&self) -> bool {
        match self {
            Self::Float => false,
            Self::Bool => false,
            Self::Integer | Self::String => true,
        }
    }

    /// Converts raw &str into a `RowValue` based on underlying `Column`
    /// TODO: Add support for strings surrounded by single quotes
    pub fn parse_value(self, text: &str) -> Result<RowValue, SchemaError> {
        let text = text.trim();
        if text.eq_ignore_ascii_case("null") {
            return Ok(RowValue::Null);
        }
        //let bad = || ParseValueError { text: text.to_string(), expected: self };
        Ok(match self {
            ColumnType::Integer => RowValue::Integer(text.parse()?),
            ColumnType::Float => {
                let f: f64 = text.parse()?;
                if f.is_nan() {
                    return Err(SchemaError::NanFloat);
                } // NaN breaks ordering and equality
                RowValue::Float(f)
            }
            ColumnType::Bool => match text.to_ascii_lowercase().as_str() {
                "true" | "t" | "1" => RowValue::Boolean(true),
                "false" | "f" | "0" => RowValue::Boolean(false),
                _ => return Err(SchemaError::ParseBool),
            },
            ColumnType::String => RowValue::String(text.to_string()),
        })
    }
}

impl Serializable for ColumnType {
    type Error = SchemaError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut buf = [0u8; 1];
        r.read_exact(&mut buf)?;
        Ok(match buf[0] {
            INT_FLAG => ColumnType::Integer,
            FLOAT_FLAG => ColumnType::Float,
            STRING_FLAG => ColumnType::String,
            BOOL_FLAG => ColumnType::Bool,
            _ => return Err(SchemaError::TagError(buf[0])),
        })
    }
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        let tag = match self {
            Self::Integer => INT_FLAG,
            Self::Float => FLOAT_FLAG,
            Self::String => STRING_FLAG,
            Self::Bool => BOOL_FLAG,
        };
        w.write_all(&[tag])?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        1
    }
}

/// A value stored in a `Schema` representing the defined column type
/// and whether or not the field is nullable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub(crate) name: String,
    pub(crate) col_type: ColumnType,
    pub(crate) nullable: bool,
}

impl std::fmt::Display for Column {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({:?}{})",
            self.name,
            self.col_type,
            if self.nullable { "*" } else { "" }
        )
    }
}

impl Serializable for Column {
    type Error = SchemaError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let col_type = ColumnType::deserialize(r)?;
        let mut buf_one = [0u8; 1];
        r.read_exact(&mut buf_one)?;
        let nullable = match buf_one[0] {
            0 => false,
            1 => true,
            _ => return Err(SchemaError::InvalidBool(buf_one[0])),
        };
        let name_len = r.read_varint()?;
        if name_len > MAX_FIELD_LEN {
            return Err(SchemaError::FieldTooLong(name_len));
        }
        let mut name_buf = vec![0u8; name_len];
        r.read_exact(&mut name_buf)?;
        let name = String::from_utf8(name_buf)?;
        Ok(Self {
            name,
            col_type,
            nullable,
        })
    }
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        self.col_type.serialize(w)?;
        w.write_all(&[self.nullable as u8])?;
        let str_len = self.name.len();
        w.write_all(&str_len.encode_var_vec())?;
        w.write_all(self.name.as_bytes())?;
        Ok(())
    }
    fn encoded_size(&self) -> usize {
        1 + 1 + self.name.len().required_space() + self.name.len()
    }
}

macro_rules! column_constructors {
    ($($variant:ident => $non_null:ident, $nullable:ident);* $(;)?) => {
        impl Column {
            $(
                pub fn $non_null(name: impl Into<String>) -> Result<Self, SchemaError> {
                    let name = name.into();
                    if name.is_empty() {
                        return Err(SchemaError::EmptyColumnName);
                    }
                    Ok(Self { name, col_type: ColumnType::$variant, nullable: false })
                }
                pub fn $nullable(name: impl Into<String>) -> Result<Self, SchemaError> {
                    let name = name.into();
                    if name.is_empty() {
                        return Err(SchemaError::EmptyColumnName);
                    }
                    Ok(Self { name, col_type: ColumnType::$variant, nullable: true })
                }
            )*
        }
    };
}

column_constructors! {
Integer => integer, nullable_integer;
Float => float, nullable_float;
String => string, nullable_string;
     Bool => bool, nullable_bool;
}

#[derive(Error, Debug)]
pub enum SchemaError {
    #[error("Type in Row does not conform to type in schema column {0}")]
    TypeMismatch(usize),
    #[error("Row doesn't have the right number of columns")]
    ColumnCountMismatch,
    #[error("Null value in non-nullable column {0}")]
    NullValueInNonNullCol(usize),
    #[error("Primary keys cannot be nullable")]
    NullablePrimaryKey,
    #[error("Primary keys must be impl Ord")]
    NonOrdPrimaryKey,
    #[error("Cannot build Schema from empty column slice")]
    EmptyColumns,
    #[error("Too many columns")]
    TooManyColumns,
    #[error("Row required size: {0}. Rows cannot use more than {MAX_LEAF_ENTRY_SIZE}")]
    RowTooLong(usize),
    #[error("Key required size: {0}. Keys cannot use more than {MAX_INTERNAL_ENTRY_SIZE}")]
    KeyTooLong(usize),
    #[error("Field length: {0}. Fields cannot be longer than {MAX_FIELD_LEN}")]
    FieldTooLong(usize),
    #[error("Invalid key value in first row")]
    InvalidKey,
    #[error("Column names can't be empty strings")]
    EmptyColumnName,
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error("Unknown byte tag {0}")]
    TagError(u8),
    #[error("Booleans must come from bytes holding '0' or '1', got {0}")]
    InvalidBool(u8),
    #[error(transparent)]
    Utf8Error(#[from] FromUtf8Error),
    #[error("Column not found")]
    ColumnNotFound,
    #[error("Can't compare NaN values")]
    NanFloat,
    #[error("Failed to parse bool value")]
    ParseBool,
    #[error(transparent)]
    ParseInt(#[from] std::num::ParseIntError),
    #[error(transparent)]
    ParseFloat(#[from] std::num::ParseFloatError),
}

/// ValidatedRow is the only type accepted for `insert` operations on the B+Tree
/// This ensures that Schema validation has been accomplished before trying to push
/// bad bytes into the database.
#[derive(Debug, Clone)]
pub struct ValidatedRow(Row);

impl ValidatedRow {
    pub fn primary_key(&self) -> Key {
        match &self.0.fields[0] {
            RowValue::Integer(n) => Key::Integer(*n),
            RowValue::String(s) => Key::String(s.clone()),
            _ => unreachable!("shouldn't be able to get another option from a Validated Row"),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_row(row: Row) -> Self {
        ValidatedRow(row)
    }
}

impl AsRef<Row> for ValidatedRow {
    fn as_ref(&self) -> &Row {
        &self.0
    }
}

impl From<ValidatedRow> for Row {
    fn from(value: ValidatedRow) -> Self {
        value.0
    }
}

/// Stores the list of columns for a Table. Columns include a ColumnType (e.g. Integer, String,
/// Float) as well as an `nullable` flag indicated whether or not the field is nullable.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    pub(crate) columns: Vec<Column>,
}

impl TryFrom<Vec<Column>> for Schema {
    type Error = SchemaError;
    fn try_from(columns: Vec<Column>) -> Result<Self, Self::Error> {
        if columns.is_empty() {
            return Err(SchemaError::EmptyColumns);
        }
        if !columns[0].col_type.is_valid_key() {
            return Err(SchemaError::NonOrdPrimaryKey);
        }
        if columns[0].nullable {
            return Err(SchemaError::NullablePrimaryKey);
        }
        if columns.len() > MAX_NUM_FIELDS {
            return Err(SchemaError::TooManyColumns);
        }
        Ok(Self { columns })
    }
}

impl Schema {
    /// Takes an unvalidated Row and ensures it conforms to the `Schema`, errors
    /// will indicate what was wrong and in which column it occurred. `ValidatedRow`
    /// is the only type accepted for `insert` operations.
    pub fn validate_row(&self, row: Row) -> Result<ValidatedRow, SchemaError> {
        let cols = &self.columns;
        let values = &row.fields;

        if cols.len() != values.len() {
            return Err(SchemaError::ColumnCountMismatch);
        }
        if Page::leaf_entry_size(&row) > MAX_LEAF_ENTRY_SIZE {
            return Err(SchemaError::RowTooLong(Page::leaf_entry_size(&row)));
        }
        let key = match Key::try_from(&row.fields[0]) {
            Ok(k) => k,
            Err(_) => return Err(SchemaError::InvalidKey),
        };
        if Page::internal_entry_size(&key) > MAX_INTERNAL_ENTRY_SIZE {
            return Err(SchemaError::KeyTooLong(Page::internal_entry_size(&key)));
        }

        for (i, (col, value)) in cols.iter().zip(values.iter()).enumerate() {
            match value.column_type() {
                Some(c) if c == col.col_type => {
                    if let RowValue::String(s) = value
                        && s.len() > MAX_FIELD_LEN
                    {
                        return Err(SchemaError::FieldTooLong(s.len()));
                    }
                }
                None if col.nullable => {}
                None => return Err(SchemaError::NullValueInNonNullCol(i)),
                _ => return Err(SchemaError::TypeMismatch(i)),
            }
        }

        Ok(ValidatedRow(row))
    }

    /// Returns a callback to filter a `Row` based on column value. Right now it just handles
    /// equal to. E.g. `col_name: "name", value "bob"` returns true if a `Row`'s column for `"name"`
    /// holds the value `"bob"`
    /// TODO: Extend this to handle other comparison operations (e.g. <, >, etc). Will need to implement
    /// a comparison scheme for `RowValue`
    pub(crate) fn filter_fn(
        &self,
        col_name: &str,
        value: &str,
    ) -> Result<impl Fn(&Row) -> bool, SchemaError> {
        let i = self
            .columns
            .iter()
            .position(|c| c.name == col_name)
            .ok_or(SchemaError::ColumnNotFound)?;

        let value = self.columns[i].col_type.parse_value(value)?;

        let res = move |row: &Row| row.fields.get(i).is_some_and(|rv| &value == rv);
        Ok(res)
    }
}

impl Serializable for Schema {
    type Error = SchemaError;
    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let num_cols: usize = r.read_varint()?;
        if num_cols > MAX_NUM_FIELDS {
            return Err(SchemaError::TooManyColumns);
        }
        let mut columns = Vec::with_capacity(num_cols);
        for _ in 0..num_cols {
            columns.push(Column::deserialize(r)?);
        }
        Self::try_from(columns)
    }

    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        let num_cols = self.columns.len();
        w.write_all(&num_cols.encode_var_vec())?;
        for col in &self.columns {
            col.serialize(w)?;
        }
        Ok(())
    }

    fn encoded_size(&self) -> usize {
        self.columns.len().required_space()
            + self.columns.iter().map(|c| c.encoded_size()).sum::<usize>()
    }
}

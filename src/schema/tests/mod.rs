pub(crate) mod generators;

pub(crate) use generators::{SchemaRowPair, SchemaWithRows};

use crate::commontypes::{Key, PAGE_ID_SIZE, SLOT_ENTRY_SIZE};
use crate::schema::{
    BOOL_FLAG, Column, ColumnType, FLOAT_FLAG, INT_FLAG, MAX_FIELD_LEN, MAX_INTERNAL_ENTRY_SIZE,
    MAX_LEAF_ENTRY_SIZE, MAX_NUM_FIELDS, NULL_FLAG, Row, RowError, RowValue, RowValueError,
    STRING_FLAG, Schema, SchemaError,
};
use crate::test_support::*;
use crate::traits::Serializable;

use quickcheck::{Arbitrary, Gen, TestResult};
use quickcheck_macros::quickcheck;
use std::collections::HashSet;
use std::io::Cursor;
use std::mem::discriminant;

#[quickcheck]
fn primary_key_returns_key(SchemaRowPair(schema, row): SchemaRowPair) -> TestResult {
    match schema.validate_row(row) {
        Ok(validated_row) => {
            let first_entry = &validated_row.0.fields[0];
            assert!(Key::try_from(first_entry).is_ok());
            TestResult::passed()
        }
        Err(_) => TestResult::failed(),
    }
}

#[quickcheck]
fn encoded_sizes(value: RowValue) -> TestResult {
    assert_roundtrip(value);
    TestResult::passed()
}

#[quickcheck]
fn row_roundtrip(row: Row) -> TestResult {
    assert_roundtrip(row);
    TestResult::passed()
}

#[test]
fn too_big_row_triggers_error_on_validation() {
    let schema = Schema::try_from(vec![
        Column::string("string column").unwrap(),
        Column::nullable_string("nullable string column").unwrap(),
    ])
    .unwrap();
    let row = Row::try_from(vec![
        RowValue::String("a".repeat(MAX_LEAF_ENTRY_SIZE)),
        RowValue::Null,
    ])
    .unwrap();
    let result = schema.validate_row(row);

    assert_matches!(result, Err(SchemaError::RowTooLong(_)));
}

#[test]
fn too_big_key_triggers_error_on_validation() {
    // this should be the largest value that is valid
    let test_boundary = MAX_INTERNAL_ENTRY_SIZE - PAGE_ID_SIZE - SLOT_ENTRY_SIZE - 1 - 2;

    let schema = Schema::try_from(vec![
        Column::string("string column").unwrap(),
        Column::nullable_string("nullable string column").unwrap(),
    ])
    .unwrap();
    let row = Row::try_from(vec![
        RowValue::String("a".repeat(test_boundary + 1)),
        RowValue::Null,
    ])
    .unwrap();
    let result = schema.validate_row(row);

    assert_matches!(result, Err(SchemaError::KeyTooLong(_)));

    let row = Row::try_from(vec![
        RowValue::String("a".repeat(test_boundary)),
        RowValue::Null,
    ])
    .unwrap();
    let result = schema.validate_row(row);

    assert!(result.is_ok());
}

#[test]
fn too_many_row_entries_triggers_error() {
    let mut fields = Vec::new();
    for _ in 0..MAX_NUM_FIELDS + 1 {
        fields.push(RowValue::Null);
    }
    let row = Row { fields };
    let mut bytes = Vec::new();
    let ser_result = row.serialize(&mut bytes);

    assert!(ser_result.is_err());
    assert_matches!(ser_result.unwrap_err(), RowValueError::TooManyFields(256));
}

#[test]
fn string_rows_boundary_value_holds() {
    // exactly MAX_FIELD_LEN bytes round-trips
    let rv = RowValue::String("a".repeat(MAX_FIELD_LEN));
    assert_roundtrip(rv.clone());

    // one more → FieldTooLong, and nothing written
    let rv = RowValue::String("a".repeat(MAX_FIELD_LEN + 1));
    let mut buf = Vec::new();

    let err = rv.serialize(&mut buf).unwrap_err();
    assert_matches!(err, RowValueError::FieldTooLong(n) if n == MAX_FIELD_LEN + 1);
    assert!(buf.is_empty(), "rejected field wrote {} bytes", buf.len());
}

#[test]
fn row_generation_errors_work() {
    // Null first entries will be rejected
    let mut fields = vec![
        RowValue::Null,
        RowValue::Integer(5),
        RowValue::Boolean(true),
    ];
    assert_matches!(
        Row::try_from(fields.clone()),
        Err(RowError::InvalidFirstEntry)
    );

    // Float first entries will be rejected
    fields.remove(0);
    fields.insert(0, RowValue::Float(0.0));
    assert_matches!(
        Row::try_from(fields.clone()),
        Err(RowError::InvalidFirstEntry)
    );

    // Bool first entries will be rejected
    fields.remove(0);
    fields.insert(0, RowValue::Boolean(true));
    assert_matches!(Row::try_from(fields), Err(RowError::InvalidFirstEntry));

    // Empty vectors will be rejected
    assert_matches!(Row::try_from(Vec::new()), Err(RowError::EmptyRow));
}

#[test]
fn too_long_field_triggers_error() {
    let row = Row {
        fields: vec![RowValue::String("a".repeat(MAX_FIELD_LEN + 1))],
    };
    let mut bytes = Vec::new();
    let ser_result = row.serialize(&mut bytes);

    assert!(ser_result.is_err());
    assert_matches!(ser_result.unwrap_err(), RowValueError::FieldTooLong(_));
}

#[quickcheck]
fn invalid_tags_trigger_error(tag: u8) -> TestResult {
    if [STRING_FLAG, INT_FLAG, FLOAT_FLAG, BOOL_FLAG, NULL_FLAG].contains(&tag) {
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
            Column::integer("dummy").unwrap(),
            Column::nullable_string("dummy").unwrap(),
            Column::nullable_float("dummy").unwrap(),
            Column::bool("dummy").unwrap(),
        ],
    };

    let all_present = Row {
        fields: vec![
            RowValue::Integer(1),
            RowValue::String("hello".to_string()),
            RowValue::Float(1.5),
            RowValue::Boolean(true),
        ],
    };
    assert!(schema.validate_row(all_present).is_ok());

    // NULL is fine in nullable columns
    let with_nulls = Row {
        fields: vec![
            RowValue::Integer(2),
            RowValue::Null,
            RowValue::Null,
            RowValue::Boolean(true),
        ],
    };
    assert!(schema.validate_row(with_nulls).is_ok());
}

#[test]
fn invalid_rows_fail_validation() {
    let schema = Schema {
        columns: vec![
            Column::integer("dummy").unwrap(),
            Column::nullable_string("dummy").unwrap(),
            Column::bool("dummy").unwrap(),
        ],
    };

    let wrong_type = Row {
        fields: vec![
            RowValue::Integer(1),
            RowValue::Float(1.5),
            RowValue::Boolean(false),
        ],
    };
    assert_matches!(
        schema.validate_row(wrong_type),
        Err(SchemaError::TypeMismatch(1))
    );

    let null_key = Row {
        fields: vec![
            RowValue::Null,
            RowValue::String("x".to_string()),
            RowValue::Boolean(true),
        ],
    };
    assert_matches!(schema.validate_row(null_key), Err(SchemaError::InvalidKey));

    let null_in_non_null = Row {
        fields: vec![
            RowValue::Integer(10),
            RowValue::String("x".to_string()),
            RowValue::Null,
        ],
    };
    assert_matches!(
        schema.validate_row(null_in_non_null),
        Err(SchemaError::NullValueInNonNullCol(2))
    );

    let too_short = Row {
        fields: vec![RowValue::Integer(1)],
    };
    assert_matches!(
        schema.validate_row(too_short),
        Err(SchemaError::ColumnCountMismatch)
    );
}

#[test]
fn schemas_cant_have_nullable_primary_keys() {
    let schema_result = Schema::try_from(vec![
        Column::nullable_integer("dummy").unwrap(),
        Column::string("dummy").unwrap(),
        Column::nullable_float("dummy").unwrap(),
    ]);
    assert_matches!(schema_result, Err(SchemaError::NullablePrimaryKey));
}

#[test]
fn schemas_cant_have_invalidkey_primary_keys() {
    // Floats can't be primary keys
    let schema_result = Schema::try_from(vec![
        Column::float("dummy").unwrap(),
        Column::string("dummy").unwrap(),
        Column::nullable_float("dummy").unwrap(),
    ]);
    assert_matches!(schema_result, Err(SchemaError::NonOrdPrimaryKey));

    // Bools can't be primary keys
    let schema_result = Schema::try_from(vec![
        Column::bool("dummy").unwrap(),
        Column::string("dummy").unwrap(),
        Column::nullable_float("dummy").unwrap(),
    ]);
    assert_matches!(schema_result, Err(SchemaError::NonOrdPrimaryKey));
}

#[test]
fn schemas_fail_with_too_many_columns() {
    let cols: Vec<Column> = (0..MAX_NUM_FIELDS + 1)
        .map(|_| Column::integer("dummy").unwrap())
        .collect();
    let schema_result = Schema::try_from(cols);
    assert_matches!(schema_result, Err(SchemaError::TooManyColumns));
}

#[test]
fn schemas_fail_with_no_columns() {
    let schema_result = Schema::try_from(vec![]);
    assert_matches!(schema_result, Err(SchemaError::EmptyColumns));
}

#[quickcheck]
fn cmp_key_matches_key_ord(row: Row, key: Key) -> bool {
    let row_key = Key::try_from(&row.fields[0]).unwrap();
    row.cmp_key(&key) == row_key.cmp(&key)
}

#[quickcheck]
fn cmp_key_equal_to_own_key(row: Row) -> bool {
    let row_key = Key::try_from(&row.fields[0]).unwrap();
    row.cmp_key(&row_key) == std::cmp::Ordering::Equal
}

#[test]
fn generator_produces_every_row_value_variant() {
    let mut g = Gen::new(100);
    let seen: HashSet<_> = (0..1_000)
        .map(|_| discriminant(&RowValue::arbitrary(&mut g)))
        .collect();

    let all = [
        RowValue::Null,
        RowValue::Boolean(false),
        RowValue::Float(0.0),
        RowValue::Integer(0),
        RowValue::String("s".into()),
    ];
    for ty in all {
        assert!(
            seen.contains(&discriminant(&ty)),
            "generator never produced {ty:?}"
        );
    }
}

#[test]
fn generator_produces_every_column_type() {
    let mut g = Gen::new(100);
    let seen: HashSet<_> = (0..1_000)
        .map(|_| discriminant(&ColumnType::arbitrary(&mut g)))
        .collect();

    for ty in [
        ColumnType::Integer,
        ColumnType::Float,
        ColumnType::String,
        ColumnType::Bool,
    ] {
        assert!(
            seen.contains(&discriminant(&ty)),
            "generator never produced {ty:?}"
        );
    }
}

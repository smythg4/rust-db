use crate::page::*;
use crate::schema::*;
use quickcheck::{Arbitrary, Gen};

/// A length in `0..=max`, scaled by quickcheck's size parameter (`QUICKCHECK_GENERATOR_SIZE`,
/// default 100) so every generator grows and shrinks together.
pub(crate) fn gen_len(g: &mut Gen, max: usize) -> usize {
    usize::arbitrary(g) % (max.min(g.size()) + 1)
}

#[derive(Debug, Clone)]
pub(crate) struct SchemaWithRows(pub(crate) Schema, pub(crate) Vec<Row>);

impl Arbitrary for SchemaWithRows {
    fn arbitrary(g: &mut Gen) -> Self {
        let schema = Schema::arbitrary(g);
        let rows: Vec<Row> = (0..gen_len(g, MAX_NUM_FIELDS - 1) % 25)
            .map(|_| valid_row_from_schema(&schema, g).into())
            .collect();
        SchemaWithRows(schema, rows)
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        let SchemaWithRows(schema, rows) = self.clone();

        let (s1, r1) = (schema.clone(), rows.clone());
        let fewer_rows = (0..r1.len()).map(move |i| {
            let mut r = r1.clone();
            r.remove(i);
            SchemaWithRows(s1.clone(), r)
        });

        let fewer_columns = (1..schema.columns.len()).rev().map(move |i| {
            let mut s = schema.clone();
            let mut r = rows.clone();
            s.columns.remove(i); // column 0 is the key: never removed
            for row in &mut r {
                row.fields.remove(i);
            }
            SchemaWithRows(s, r)
        });

        Box::new(fewer_rows.chain(fewer_columns))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SchemaRowPair(pub(crate) Schema, pub(crate) Row);

impl Arbitrary for SchemaRowPair {
    fn arbitrary(g: &mut Gen) -> Self {
        let schema = Schema::arbitrary(g);
        let row = valid_row_from_schema(&schema, g).into();
        SchemaRowPair(schema, row)
    }
    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        let SchemaRowPair(schema, row) = self.clone();
        Box::new((1..schema.columns.len()).rev().map(move |i| {
            let mut s = schema.clone();
            let mut r = row.clone();
            s.columns.remove(i); // column 0 is the key: never removed
            r.fields.remove(i);
            SchemaRowPair(s, r)
        }))
    }
}

impl Arbitrary for Page {
    fn arbitrary(g: &mut Gen) -> Self {
        if bool::arbitrary(g) {
            arbitrary_leaf(g)
        } else {
            arbitrary_internal(g)
        }
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        if self.is_leaf() {
            Box::new(LeafPage(self.clone()).shrink().map(|LeafPage(p)| p))
        } else {
            Box::new(InternalPage(self.clone()).shrink().map(|InternalPage(p)| p))
        }
    }
}

/// A leaf built through the real API: random rows inserted until a generated target
/// count is reached or the page fills up. Every generated leaf is one `leaf_insert` can
/// actually produce: sorted, unique keys, within size limits, from empty to full.
fn arbitrary_leaf(g: &mut Gen) -> Page {
    let mut page = Page::empty_leaf(PageId::arbitrary(g));
    page.last_update = PageLsn(Option::<Lsn>::arbitrary(g));
    page.set_next(Option::<PageId>::arbitrary(g)).unwrap();
    page.set_prev(Option::<PageId>::arbitrary(g)).unwrap();
    for _ in 0..gen_len(g, MAX_LEAF_ITEMS) {
        match page.leaf_insert(ValidatedRow::from_row(Row::arbitrary(g))) {
            Ok(()) | Err(PageError::DuplicateKey) => {}
            Err(PageError::PageFull) => break,
            Err(e) => panic!("unexpected error building a leaf: {e:?}"),
        }
    }
    page
}

/// An internal page built through the real API: random key/child pairs inserted until a
/// generated target count is reached or the page fills up. Every generated page is one
/// `internal_insert` can actually produce: sorted, unique keys, within size limits,
/// from empty to full.
fn arbitrary_internal(g: &mut Gen) -> Page {
    let mut page = Page::empty_page(
        PageId::arbitrary(g),
        PageBody::Internal {
            keys: Vec::new(),
            children: vec![PageId::arbitrary(g)],
        },
    );
    page.last_update = PageLsn(Option::<Lsn>::arbitrary(g));
    let all_int_keys = bool::arbitrary(g);
    for _ in 0..gen_len(g, MAX_INTERNAL_ITEMS) {
        let key = if all_int_keys {
            Key::Integer(i64::arbitrary(g))
        } else {
            Key::String(String::arbitrary(g))
        };
        let child_id = PageId::arbitrary(g);
        match page.internal_insert(key, child_id) {
            Ok(()) | Err(PageError::DuplicateKey) => {}
            Err(PageError::PageFull) => break,
            Err(e) => panic!("unexpected error building an internal page: {e:?}"),
        }
    }
    page
}

#[derive(Debug, Clone)]
pub(crate) struct LeafPage(pub(crate) Page);

#[derive(Debug, Clone)]
pub(crate) struct InternalPage(pub(crate) Page);

impl Arbitrary for LeafPage {
    fn arbitrary(g: &mut Gen) -> Self {
        LeafPage(arbitrary_leaf(g))
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        let page = self.0.clone();
        let keys: Vec<Key> = page
            .records()
            .unwrap()
            .map(|r| Key::try_from(&r.fields[0]).unwrap())
            .collect();

        Box::new(keys.into_iter().map(move |key| {
            let mut smaller = page.clone();
            smaller.leaf_remove(&key).unwrap();
            smaller.check_invariants().unwrap();
            LeafPage(smaller)
        }))
    }
}

impl Arbitrary for InternalPage {
    fn arbitrary(g: &mut Gen) -> Self {
        InternalPage(arbitrary_internal(g))
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        let page = self.0.clone();
        let keys: Vec<Key> = page.keys().unwrap().cloned().collect();

        Box::new(keys.into_iter().map(move |key| {
            let mut smaller = page.clone();
            smaller.internal_remove(&key).unwrap();
            smaller.check_invariants().unwrap();
            InternalPage(smaller)
        }))
    }
}

fn non_nan_f64(g: &mut Gen) -> f64 {
    let mut f = f64::arbitrary(g);
    while f.is_nan() {
        f = f64::arbitrary(g);
    }
    f
}

impl Arbitrary for ColumnType {
    fn arbitrary(g: &mut Gen) -> Self {
        g.choose(&[
            ColumnType::String,
            ColumnType::Bool,
            ColumnType::Float,
            ColumnType::Integer,
        ])
        .cloned()
        .unwrap()
    }
}

impl Arbitrary for Column {
    fn arbitrary(g: &mut Gen) -> Self {
        let mut name = String::arbitrary(g);
        while name.is_empty() {
            name = String::arbitrary(g);
        }
        Column {
            name,
            col_type: ColumnType::arbitrary(g),
            nullable: bool::arbitrary(g),
        }
    }
}

impl Arbitrary for Schema {
    fn arbitrary(g: &mut Gen) -> Self {
        let mut first_entry = Column::arbitrary(g);
        while !first_entry.col_type.is_valid_key() || first_entry.nullable {
            first_entry = Column::arbitrary(g);
        }
        Schema {
            columns: [first_entry]
                .into_iter()
                .chain((0..gen_len(g, MAX_NUM_FIELDS - 1) % 25).map(|_| Column::arbitrary(g)))
                .collect(),
        }
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        let columns = self.columns.clone();
        Box::new((1..columns.len()).rev().map(move |i| {
            let mut smaller = columns.clone();
            smaller.remove(i); // index 0 is the key column: never removed
            Schema { columns: smaller }
        }))
    }
}

impl Arbitrary for RowValue {
    fn arbitrary(g: &mut Gen) -> Self {
        // RowValue: a table of generator functions
        let gens: &[fn(&mut Gen) -> RowValue] = &[
            |g| RowValue::Integer(i64::arbitrary(g)),
            |g| RowValue::Float(non_nan_f64(g)),
            |g| RowValue::String(String::arbitrary(g)),
            |g| RowValue::Boolean(bool::arbitrary(g)),
            |_| RowValue::Null,
        ];
        g.choose(gens).unwrap()(g)
    }
}

/// TODO: Figure out how to cap g.size() in Arbitrary instead of these contrived
/// caps I put in the implementation
impl Arbitrary for Row {
    /// Builds a row within the real limits by construction: the first, then extra
    /// fields until either the generated count or the row's byte budget runs out.
    fn arbitrary(g: &mut Gen) -> Self {
        let key = loop {
            let k = Key::arbitrary(g);
            if Page::internal_entry_size(&k) <= MAX_INTERNAL_ENTRY_SIZE {
                break k;
            }
        };
        let mut row = Row {
            fields: vec![key.into()],
        };
        for _ in 0..gen_len(g, MAX_NUM_FIELDS - 1) {
            row.fields.push(RowValue::arbitrary(g));
            if Page::leaf_entry_size(&row) > MAX_LEAF_ENTRY_SIZE {
                row.fields.pop();
                break;
            }
        }
        row
    }

    fn shrink(&self) -> Box<dyn Iterator<Item = Self>> {
        let fields = self.fields.clone();
        Box::new((1..fields.len()).rev().map(move |i| {
            let mut smaller = fields.clone();
            smaller.remove(i); // index 0 is the key column: never removed
            Row { fields: smaller }
        }))
    }
}

pub(crate) fn valid_row_from_schema(schema: &Schema, g: &mut Gen) -> ValidatedRow {
    loop {
        let row = Row {
            fields: schema
                .columns
                .iter()
                .map(|c| {
                    let coin_flip = bool::arbitrary(g);
                    match c.col_type {
                        _ if c.nullable && coin_flip => RowValue::Null,
                        ColumnType::Bool => RowValue::Boolean(bool::arbitrary(g)),
                        ColumnType::Float => {
                            let mut f = f64::arbitrary(g);
                            while f.is_nan() {
                                f = f64::arbitrary(g);
                            }
                            RowValue::Float(f)
                        }
                        ColumnType::Integer => RowValue::Integer(i64::arbitrary(g)),
                        ColumnType::String => RowValue::String(String::arbitrary(g)),
                    }
                })
                .collect(),
        };
        if let Ok(vr) = schema.validate_row(row) {
            break vr;
        }
    }
}

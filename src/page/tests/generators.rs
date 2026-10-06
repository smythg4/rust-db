use crate::page::*;
use crate::schema::*;
use crate::test_support::gen_len;
use quickcheck::{Arbitrary, Gen};

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

use crate::types::{Lsn, PageId, PageLsn, SlotEntry, SlotIndex, TableId};
use quickcheck::{Arbitrary, Gen};

impl Arbitrary for PageId {
    fn arbitrary(g: &mut Gen) -> Self {
        PageId::new(TableId::arbitrary(g), u32::arbitrary(g))
    }
}

impl Arbitrary for TableId {
    fn arbitrary(g: &mut Gen) -> Self {
        TableId::new(u32::arbitrary(g))
    }
}

impl Arbitrary for SlotIndex {
    fn arbitrary(g: &mut Gen) -> Self {
        SlotIndex::new(u16::arbitrary(g))
    }
}

impl Arbitrary for Lsn {
    fn arbitrary(g: &mut Gen) -> Self {
        let mut lsn = u64::arbitrary(g);
        while lsn == 0 {
            lsn = u64::arbitrary(g);
        }
        Lsn::new(lsn).unwrap()
    }
}

impl Arbitrary for PageLsn {
    fn arbitrary(g: &mut Gen) -> Self {
        PageLsn(Option::<Lsn>::arbitrary(g))
    }
}

impl Arbitrary for SlotEntry {
    fn arbitrary(g: &mut Gen) -> Self {
        SlotEntry {
            offset: u16::arbitrary(g),
            length: u16::arbitrary(g),
        }
    }
}

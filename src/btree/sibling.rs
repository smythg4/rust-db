use crate::page::{ChildIndex, Page};
use crate::schema::Key;
use crate::types::PageId;

pub(crate) enum Side {
    Left,
    Right,
}

pub(crate) struct Sibling {
    pub(crate) id: PageId,
    pub(crate) separator: Key,
    pub(crate) side: Side,
}

pub(crate) fn pick_sibling(parent: &Page, idx: ChildIndex) -> Option<Sibling> {
    if let Some(id) = parent.child_at(idx.right_sibling()) {
        Some(Sibling {
            id,
            separator: parent
                .key_at(idx.right_separator())
                .expect("children.len() == keys.len() + 1")
                .clone(),
            side: Side::Right,
        })
    } else {
        Some(Sibling {
            id: parent.child_at(idx.left_sibling()?)?,
            separator: parent
                .key_at(idx.left_separator()?)
                .expect("children.len() == keys.len() + 1")
                .clone(),
            side: Side::Left,
        })
    }
}

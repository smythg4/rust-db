use crate::page::*;

impl Page {
    /// Checks every structural rule a `Page` must satisfy, whether it came from disk or from
    /// an operation in this module:
    ///
    /// - leaf: every row's first field is a valid key, and keys are strictly increasing
    /// - internal: `children.len() == keys.len() + 1`, and keys are strictly increasing
    /// - the page fits in `PAGE_SIZE` bytes
    ///
    /// Returns the first violation found. `deserialize` maps it to `PageError::Corrupt`;
    /// page operations call `debug_check_invariants` to catch bugs in this module.
    pub(crate) fn check_invariants(&self) -> Result<(), CorruptionKind> {
        if self.free_space().is_none() {
            return Err(CorruptionKind::ExceedsCapacity);
        }
        match &self.body {
            PageBody::Leaf { records, .. } => {
                let mut prev: Option<Key> = None;
                for (i, record) in records.iter().enumerate() {
                    if Self::leaf_entry_size(record) > MAX_LEAF_ENTRY_SIZE {
                        return Err(CorruptionKind::RowTooLarge { slot: i });
                    }
                    let first = record.fields.first().ok_or(CorruptionKind::MissingKey)?;
                    let key = Key::try_from(first)
                        .map_err(|_| CorruptionKind::InvalidKey(first.clone()))?;
                    if Self::internal_entry_size(&key) > MAX_INTERNAL_ENTRY_SIZE {
                        return Err(CorruptionKind::KeyTooLarge { slot: i });
                    }
                    if prev.as_ref().is_some_and(|p| p >= &key) {
                        return Err(CorruptionKind::UnsortedKeys { at: i });
                    }

                    prev = Some(key);
                }
            }
            PageBody::Internal { keys, children } => {
                if children.len() != keys.len() + 1 {
                    return Err(CorruptionKind::ChildCountMismatch {
                        keys: keys.len(),
                        children: children.len(),
                    });
                }
                if let Some(slot) = keys
                    .iter()
                    .position(|k| Self::internal_entry_size(k) > MAX_INTERNAL_ENTRY_SIZE)
                {
                    return Err(CorruptionKind::KeyTooLarge { slot });
                }
                if let Some(i) = keys.windows(2).position(|w| w[0] >= w[1]) {
                    return Err(CorruptionKind::UnsortedKeys { at: i + 1 });
                }
            }
            PageBody::Meta {
                page_count,
                free_list_head,
                table_name,
                ..
            } => {
                if let Some(flh) = free_list_head
                    && flh.get_page_num() >= *page_count
                {
                    return Err(CorruptionKind::PageNumOutOfRange(
                        flh.get_page_num() as usize
                    ));
                }
                if table_name.len() > u16::MAX as usize {
                    return Err(CorruptionKind::TableNameTooLong(table_name.len()));
                }
            }
            PageBody::Free { .. } => {} // nothing to check within the page. BPM needs to make sure it's in the right table
        }
        Ok(())
    }

    /// Panics (debug builds only) if `check_invariants` fails. Call at the end of every
    /// operation that changes a page; `op` names the operation in the panic message.
    #[inline]
    pub(crate) fn debug_check_invariants(&self, op: &str) {
        #[cfg(debug_assertions)]
        if let Err(kind) = self.check_invariants() {
            panic!("page {} violates {kind:?} after {op}", self.page_id);
        }
        #[cfg(not(debug_assertions))]
        let _ = op;
    }
}

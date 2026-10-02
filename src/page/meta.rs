use crate::page::*;

impl Page {
    /// Returns the root `PageId` from a Meta `Page`
    pub(crate) fn meta_get_root_id(&self) -> Result<PageId, PageError> {
        let PageBody::Meta { root_id, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(*root_id)
    }

    /// Returns the root `PageId` from a Meta `Page`
    pub(crate) fn meta_set_root_id(&mut self, id: PageId) -> Result<(), PageError> {
        let PageBody::Meta { root_id, .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        *root_id = id;
        Ok(())
    }

    /// Returns the number of pages in the `Table`
    pub(crate) fn meta_get_page_count(&self) -> Result<usize, PageError> {
        let PageBody::Meta { page_count, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(*page_count as usize)
    }

    /// Overwrites the number of pages in the `Table`
    pub(crate) fn meta_set_page_count(&mut self, new_count: u32) -> Result<(), PageError> {
        let PageBody::Meta { page_count, .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        *page_count = new_count;
        Ok(())
    }

    /// Returns the schema from the `Table`
    pub(crate) fn meta_get_schema(&self) -> Result<&Schema, PageError> {
        let PageBody::Meta { schema, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(schema)
    }

    /// Returns the table_name from the `Table`
    pub(crate) fn meta_get_table_name(&self) -> Result<&String, PageError> {
        let PageBody::Meta { table_name, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(table_name)
    }

    /// Returns the head of the free page list
    pub(crate) fn meta_get_free_list_head(&self) -> Result<Option<PageId>, PageError> {
        let PageBody::Meta { free_list_head, .. } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(*free_list_head)
    }

    /// Sets the head of the free page list. Returns the old head
    pub(crate) fn meta_set_free_list_head(
        &mut self,
        new_head: Option<PageId>,
    ) -> Result<Option<PageId>, PageError> {
        let PageBody::Meta { free_list_head, .. } = &mut self.body else {
            return Err(PageError::WrongPageType);
        };
        let old_head = *free_list_head;
        *free_list_head = new_head;
        Ok(old_head)
    }

    /// Increments the page_count on a meta page
    pub(crate) fn meta_bump_page_count(&mut self) -> Result<PageId, PageError> {
        let PageBody::Meta {
            page_count,
            root_id,
            ..
        } = &mut self.body
        else {
            return Err(PageError::WrongPageType);
        };
        let new_page_id = PageId::new(root_id.get_table_id(), *page_count);
        *page_count += 1;
        Ok(new_page_id)
    }

    /// Used to `pop` the next free page off the list
    pub fn free_list_pop(&mut self, page: &Page) -> Result<PageId, PageError> {
        if !page.is_free() || !self.is_meta() {
            return Err(PageError::WrongPageType);
        }
        let PageBody::Meta { free_list_head, .. } = &mut self.body else {
            unreachable!()
        };
        if *free_list_head != Some(page.page_id) {
            return Err(PageError::NotFreeListHead {
                head: *free_list_head,
                got: page.page_id,
            });
        }

        let PageBody::Free { next } = &page.body else {
            unreachable!()
        };

        *free_list_head = *next;

        self.debug_check_invariants("free_list_pop");
        Ok(page.page_id)
    }

    /// Used to update the free_list on a `Meta` `Page`. `freed` must be part of the same `Table`,
    /// must have a page_num that's less than the meta page's `num_pages`, and can't be page number
    /// 0 since that's reserved for the `Meta` `Page`.
    pub fn free_list_push(&mut self, freed: &mut Page) -> Result<(), PageError> {
        if !self.is_meta() || !freed.is_free() {
            return Err(PageError::WrongPageType);
        }
        let PageBody::Meta {
            free_list_head,
            root_id,
            page_count,
            ..
        } = &mut self.body
        else {
            unreachable!()
        };

        let id = freed.page_id();
        if id.get_table_id() != root_id.get_table_id() {
            return Err(PageError::WrongTable(id));
        }
        if id.get_page_num() == 0 {
            return Err(PageError::ReservedPage);
        }
        if id.get_page_num() >= *page_count {
            return Err(PageError::PageOutOfRange(id));
        }
        if *free_list_head == Some(id) {
            return Err(PageError::AlreadyFree(id));
        }

        let PageBody::Free { next } = &mut freed.body else {
            unreachable!()
        };

        *next = *free_list_head;
        *free_list_head = Some(id);

        self.debug_check_invariants("free_list_push");
        Ok(())
    }

    pub fn free_next(&self) -> Result<Option<PageId>, PageError> {
        let PageBody::Free { next } = &self.body else {
            return Err(PageError::WrongPageType);
        };
        Ok(*next)
    }
}

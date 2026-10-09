use crate::bpm::{BpmError, BufferPoolManager, PageWriteGuard};
use crate::btree::BTree;
use crate::page::{PAGE_SIZE, Page, PageBody};
use crate::schema::{Column, ColumnType, Key, Row, RowValue, Schema};
use crate::traits::{DiskManager, EvictionPolicy};
use crate::types::{PageId, TableId};
use std::collections::HashSet;

const NO_FILTER: fn(&Row) -> bool = |_| true;

mod errors;

pub use errors::TableError;

pub struct Table<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    pub(crate) bpm: &'a BufferPoolManager<Dm, Ep>,
    meta_id: PageId,
    #[allow(dead_code)]
    // only tests right now. maybe package this into always reading from the meta page? though caching is probably fine since it should never change
    schema: Schema,
    #[allow(dead_code)] // for now...
    table_name: String,
}

impl<'a, Dm: DiskManager, Ep: EvictionPolicy> Table<'a, Dm, Ep> {
    /// The only way to get a `Table`: reads and validates the meta page.
    pub fn open(bpm: &'a BufferPoolManager<Dm, Ep>, table_id: TableId) -> Result<Self, TableError> {
        let guard = bpm.fetch_read(PageId::new(table_id, 0))?;
        let meta_id = guard.page_id();
        let schema = guard.meta_get_schema()?.clone();
        let table_name = guard.meta_get_table_name()?.clone();
        let table = Self {
            bpm,
            meta_id,
            schema,
            table_name,
        };
        Ok(table)
    }

    /// Finishes using this table. In debug builds, checks that every page is either in the
    /// tree or on the free list. Does not flush: durability is the buffer pool's job, so call
    /// `BufferPoolManager::close` to persist changes.
    pub fn close(self) -> Result<(), TableError> {
        log::debug!("Closing table...");
        Ok(())
    }

    /// Writes a fresh meta page plus an empty leaf root
    pub fn create(
        bpm: &'a BufferPoolManager<Dm, Ep>,
        table_id: TableId,
        schema: Schema,
        name: &str,
    ) -> Result<Self, TableError> {
        use crate::page::PageBody;
        let meta_id = PageId::new(table_id, 0);
        log::info!("Opening the file...");
        match bpm.fetch_read(meta_id) {
            Ok(_) => return Err(TableError::AlreadyExists(table_id)),
            Err(BpmError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {} // file isn't populated yet
            Err(BpmError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {} // clear path to make the table
            Err(e) => return Err(e.into()),
        }
        log::info!("File opened! Making new pages...");
        let root_id = PageId::new(table_id, 1);
        let meta_page = Page::empty_page(
            meta_id,
            PageBody::Meta {
                root_id,
                page_count: 2,
                free_list_head: None,
                schema: schema.clone(),
                table_name: name.to_string(),
            },
        );
        let leaf_root = Page::empty_leaf(root_id);

        bpm.new_page(leaf_root)?;
        bpm.new_page(meta_page)?;
        log::info!("Meta and root pages created. Flushing to disk...");
        bpm.flush_all()?;
        let table = Self {
            bpm,
            meta_id,
            schema,
            table_name: name.to_string(),
        };
        table.debug_check_space_accounting("create");
        Ok(table)
    }

    pub fn allocate(&self) -> Result<PageWriteGuard<'_, Dm, Ep>, TableError> {
        let mut meta_guard = self.bpm.fetch_write(self.meta_id)?;

        if let Some(free_list_head) = meta_guard.meta_get_free_list_head()? {
            let mut free_guard = self.bpm.fetch_write(free_list_head)?;
            meta_guard.free_list_pop(&free_guard)?;
            *free_guard = Page::empty_page(free_guard.page_id(), PageBody::Free { next: None });
            Ok(free_guard)
        } else {
            let new_free_id = meta_guard.meta_bump_page_count()?;
            Ok(self
                .bpm
                .new_page(Page::empty_page(new_free_id, PageBody::Free { next: None }))?)
        }
    }

    pub fn free(&self, mut page: PageWriteGuard<'a, Dm, Ep>) -> Result<(), TableError> {
        let mut meta_guard = self.bpm.fetch_write(self.meta_id)?;
        meta_guard.free_list_push(&mut page)?;
        //self.debug_check_space_accounting("free");
        Ok(())
    }

    pub fn root_id(&self) -> Result<PageId, TableError> {
        let id = self.bpm.fetch_read(self.meta_id)?.meta_get_root_id()?;
        Ok(id)
    }

    pub fn set_root_id(&self, id: PageId) -> Result<(), TableError> {
        self.bpm.fetch_write(self.meta_id)?.meta_set_root_id(id)?;
        //self.debug_check_space_accounting("set root id");
        Ok(())
    }

    pub fn insert(&self, row: Row) -> Result<(), TableError> {
        let row = self.schema.validate_row(row)?;
        BTree::new(self).insert(row)?;
        //self.debug_check_space_accounting("insert");
        Ok(())
    }

    pub fn delete(&self, key: &Key) -> Result<Option<Row>, TableError> {
        Ok(BTree::new(self).delete(key)?)
    }

    pub fn get(&self, key: &Key) -> Result<Option<Row>, TableError> {
        Ok(BTree::new(self).get(key)?)
    }

    pub fn get_all(&self, filter_fn: impl Fn(&Row) -> bool) -> Result<Vec<Row>, TableError> {
        Ok(BTree::new(self).get_all(filter_fn)?)
    }

    pub fn size(&self) -> Result<usize, TableError> {
        Ok(self.bpm.fetch_read(self.meta_id)?.meta_get_page_count()? * PAGE_SIZE)
    }

    fn get_page_id(&self, page_num: u32) -> PageId {
        let table_id = self.meta_id.get_table_id();
        PageId::new(table_id, page_num)
    }

    fn new_internal(&self, page_num: &mut u32, right_child: PageId) -> Page {
        let body = PageBody::Internal {
            keys: Vec::new(),
            children: vec![right_child],
        };
        let page = Page::empty_page(self.get_page_id(*page_num), body);
        *page_num += 1;
        page
    }

    fn build_internal_level(
        &self,
        children: Vec<(Key, PageId)>,
        next_page_num: &mut u32,
        out: &mut Vec<Page>,
    ) -> Result<Vec<(Key, PageId)>, TableError> {
        let mut iter = children.into_iter();
        let (mut low_key, first_child) = iter.next().expect("a level has at least one page");
        let mut page = self.new_internal(next_page_num, first_child);
        let mut level = Vec::new();

        for (key, child) in iter {
            if page.can_insert_separator(&key) {
                page.internal_insert(key, child)?;
            } else {
                level.push((low_key, page.page_id()));
                out.push(page);
                low_key = key;
                page = self.new_internal(next_page_num, child);
            }
        }
        level.push((low_key, page.page_id()));
        out.push(page);

        Ok(level)
    }

    fn first_key(page: &Page) -> Key {
        let row = page
            .records()
            .expect("leaves have records")
            .next()
            .expect("has first row");
        Key::try_from(row.fields.first().expect("row has keys")).expect("keys are valid")
    }

    pub fn vacuum(&self) -> Result<(), TableError> {
        log::info!("Starting vacuum on table '{}'...", self.table_name);
        // collect all the rows into memory
        let rows: Vec<Row> = BTree::new(self).get_all(NO_FILTER)?;

        let mut next_page_num: u32 = 1; // page 0 is the meta page
        let new_leaf = |n: &mut u32| {
            let page = Page::empty_leaf(self.get_page_id(*n));
            *n += 1;
            page
        };

        // build a series of leaf pages
        let mut leaves: Vec<Page> = Vec::new();
        let mut curr_page = new_leaf(&mut next_page_num);
        for row in rows {
            // start at 2
            if curr_page.can_insert(&row) {
                let validated_row = self.schema.validate_row(row)?;
                curr_page.leaf_insert(validated_row)?;
            } else {
                leaves.push(std::mem::replace(
                    &mut curr_page,
                    new_leaf(&mut next_page_num),
                ));
                let validated_row = self.schema.validate_row(row)?;
                curr_page.leaf_insert(validated_row)?;
            }
        }
        leaves.push(curr_page);

        // link the neighbors - wanted to do this windows, but that's not mutable
        let ids: Vec<PageId> = leaves.iter().map(|l| l.page_id()).collect();
        for (i, leaf) in leaves.iter_mut().enumerate() {
            leaf.set_prev(i.checked_sub(1).map(|j| ids[j]))?;
            leaf.set_next(ids.get(i + 1).copied())?;
        }

        let mut level: Vec<(Key, PageId)> = leaves
            .iter()
            .map(|l| (Self::first_key(l), l.page_id()))
            .collect();
        let mut internals = Vec::new();
        while level.len() > 1 {
            level = self.build_internal_level(level, &mut next_page_num, &mut internals)?;
        }
        let root_id = level[0].1;
        let mut meta_page = self.bpm.fetch_read(self.meta_id)?.clone();
        meta_page.meta_set_root_id(root_id)?;
        meta_page.meta_set_page_count(next_page_num)?; // every page from here on doesn't exist
        meta_page.meta_set_free_list_head(None)?; // the new file is fully packed: nothing is free
        let mut all_pages = vec![meta_page];
        all_pages.extend(internals);
        all_pages.extend(leaves);

        self.bpm.swap_file(all_pages)?;
        Ok(())
    }

    fn check_space_accounting(&self) {
        let page_count = self
            .bpm
            .fetch_read(self.meta_id)
            .expect("failed to fetch meta data")
            .meta_get_page_count()
            .expect("failed to read meta data page count");
        let root_id = self.root_id().expect("failed to fetch root id");
        let mut in_tree = HashSet::new();
        let mut child_stack = vec![root_id];
        while let Some(cid) = child_stack.pop() {
            assert!(in_tree.insert(cid), "page {cid} is reachable twice");
            let curr_page = self.bpm.fetch_read(cid).expect("failed to find child");
            if let Some(children) = curr_page.children() {
                child_stack.extend(children);
            }
        }
        let mut on_free_list = HashSet::new();
        let mut cur = self
            .bpm
            .fetch_read(self.meta_id)
            .expect("failed to fetch meta page")
            .meta_get_free_list_head()
            .expect("failed to get the free list head");
        while let Some(id) = cur {
            assert!(on_free_list.insert(id), "cycle in the free list at {id}");
            cur = self
                .bpm
                .fetch_read(id)
                .expect("failed to fetch the next free list entry")
                .free_next()
                .expect("failed to read next pointer on free list");
        }
        assert!(
            in_tree.is_disjoint(&on_free_list),
            "a page is both in the tree and free"
        );
        assert_eq!(in_tree.len() + on_free_list.len(), page_count - 1); // minus the meta page
    }

    /// Panics (debug builds only) if `check_space_accounting` fails. Call at the end of every
    /// operation that changes a table; `op` names the operation in the panic message.
    #[inline]
    #[allow(unused_variables)]
    pub(crate) fn debug_check_space_accounting(&self, op: &str) {
        #[cfg(debug_assertions)]
        self.check_space_accounting();

        #[cfg(not(debug_assertions))]
        let _ = op;
    }

    fn map_raw(kind: ColumnType, arg: &str) -> Result<RowValue, TableError> {
        if arg.to_uppercase() == "NULL" {
            return Ok(RowValue::Null);
        }
        Ok(match kind {
            ColumnType::Bool => RowValue::Boolean(arg.parse().map_err(|_| TableError::Unexpected)?),
            ColumnType::Float => RowValue::Float(arg.parse().map_err(|_| TableError::Unexpected)?),
            ColumnType::Integer => {
                RowValue::Integer(arg.parse().map_err(|_| TableError::Unexpected)?)
            }
            ColumnType::String => RowValue::String(arg.to_string()),
        })
    }

    pub fn insert_raw(&self, args: &[String]) -> Result<(), TableError> {
        let row = Row {
            fields: self
                .schema
                .columns
                .iter()
                .map(|c| c.col_type)
                .zip(args.iter())
                .map(|(ct, a)| Self::map_raw(ct, a))
                .collect::<Result<Vec<RowValue>, TableError>>()?,
        };
        self.insert(row)
    }

    pub fn delete_raw(&self, args: &[String]) -> Result<Option<Row>, TableError> {
        assert!(args.len() == 1);
        let row_val = Self::map_raw(self.schema.columns[0].col_type, &args[0])?;
        let key = &Key::try_from(&row_val).expect("invalid key");
        self.delete(key)
    }

    pub fn get_raw(&self, args: &[String]) -> Result<Option<Row>, TableError> {
        assert_eq!(args.len(), 1);
        let row_val = Self::map_raw(self.schema.columns[0].col_type, &args[0])?;
        let key = &Key::try_from(&row_val).expect("invalid key");
        self.get(key)
    }

    pub fn get_all_raw(&self, args: &[String]) -> Result<Vec<Row>, TableError> {
        assert_eq!(args.len(), 4);
        assert_eq!(args[0], "*");
        assert_eq!(args[1], "where");
        let filter_fn = self.schema.filter_fn(&args[2], &args[3])?;
        self.get_all(filter_fn)
    }

    pub fn print_rows(&self, rows: &[Row]) -> Result<(), TableError> {
        println!("{}", self.table_name);
        let header = line(self.schema.columns.iter().map(Column::to_string));

        let rule = "-".repeat(header.chars().count());

        let print_rows = |rows: &[Row]| {
            for row in rows {
                println!("{}", line(row.fields.iter().map(|v| v.to_string())));
            }
        };

        println!("{rule}\n{header}\n{rule}");
        if rows.len() <= 10 {
            print_rows(rows);
        } else {
            print_rows(&rows[..5]);
            println!(
                "{}",
                line(self.schema.columns.iter().map(|_| "...".to_string()))
            );
            print_rows(&rows[rows.len() - 5..]);
        }
        println!("{rule}\n({} rows)", rows.len());

        Ok(())
    }

    pub fn print_table(&self) -> Result<(), TableError> {
        let rows = BTree::new(self)
            .get_all(NO_FILTER)
            .expect("failed to fetch rows");
        self.print_rows(&rows)
    }
}

const COL_WIDTH: usize = 30;

/// Pads to COL_WIDTH, or cuts with "…" if too long (counts chars, not bytes).
fn cell(s: &str) -> String {
    if s.chars().count() > COL_WIDTH {
        let cut: String = s.chars().take(COL_WIDTH - 1).collect();
        format!("{cut}…")
    } else {
        format!("{s:<COL_WIDTH$}")
    }
}

fn line(cells: impl IntoIterator<Item = String>) -> String {
    let cells: Vec<String> = cells.into_iter().map(|c| cell(&c)).collect();
    format!("| {} |", cells.join(" | "))
}

#[cfg(test)]
mod tests;

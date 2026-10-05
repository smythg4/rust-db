use crate::commontypes::{Key, PageId, PageLsn, SlotEntry};
use crate::page::{
    CHECKSUM_OFFSET, FREE_TAG, INTERNAL_TAG, LEAF_TAG, MAX_INTERNAL_ITEMS, MAX_LEAF_ITEMS,
    META_TAG, PAGE_ID_SIZE, PAGE_SIZE, SLOT_ENTRY_SIZE,
};
use crate::page::{CorruptionKind, Page, PageBody, PageError, RawPage};
use crate::schema::{Row, Schema};
use crate::traits::Serializable;
use crc32_light::Crc32Stream;
use integer_encoding::{VarInt, VarIntReader};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};

impl Serializable for Page {
    type Error = PageError;
    fn serialize<W: Write>(&self, w: &mut W) -> Result<(), Self::Error> {
        let raw = self.as_raw_page()?;
        w.write_all(&raw)?;
        Ok(())
    }

    fn deserialize<R: Read>(r: &mut R) -> Result<Self, Self::Error> {
        let mut buf = [0u8; PAGE_SIZE];
        r.read_exact(&mut buf)?;

        // read the checksum
        let crc_buf = &buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4];
        let checksum = u32::from_be_bytes(crc_buf.try_into().expect("always 4 byte slice"));

        let computed_checksum = Self::page_checksum(&buf);
        if computed_checksum != checksum {
            return Err(PageError::Corrupt {
                page_id: None,
                kind: CorruptionKind::CheckSumMismatch,
            });
        }

        let mut cursor = Cursor::new(&buf[..]);

        let mut buf_one = [0u8; 1];
        cursor.read_exact(&mut buf_one)?;
        let tag = buf_one[0];

        let page_id = PageId::deserialize(&mut cursor)?;
        let last_update = PageLsn::deserialize(&mut cursor)?;

        // helper closure for error mapping
        let corrupt = |kind| PageError::Corrupt {
            page_id: Some(page_id),
            kind,
        };

        // skip the crc32 portion
        cursor.set_position(CHECKSUM_OFFSET as u64 + 4);

        let (next_page, prev_page) = if tag == LEAF_TAG {
            let np = Option::<PageId>::deserialize(&mut cursor)
                .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
            let pp = Option::<PageId>::deserialize(&mut cursor)
                .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
            (np, pp)
        } else {
            (None, None)
        };

        let mut buf_two = [0u8; 2];
        cursor.read_exact(&mut buf_two)?;
        let num_items = u16::from_be_bytes(buf_two) as usize;

        let max_items = match tag {
            META_TAG => 0,
            FREE_TAG => 0,
            INTERNAL_TAG => MAX_INTERNAL_ITEMS,
            LEAF_TAG => MAX_LEAF_ITEMS,
            _ => {
                return Err(PageError::Corrupt {
                    page_id: None,
                    kind: CorruptionKind::InvalidTag(tag),
                });
            }
        };
        if num_items > max_items {
            return Err(corrupt(CorruptionKind::TooManyItems(num_items)));
        }

        let body = match tag {
            LEAF_TAG => {
                let ranges = Self::read_slot_ranges(&mut cursor, num_items).map_err(corrupt)?;
                let records = ranges
                    .into_iter()
                    .enumerate()
                    .map(|(slot, range)| {
                        Self::decode_slot(&buf[range], slot, |slot| CorruptionKind::CorruptRow {
                            slot,
                        })
                        .map_err(corrupt)
                    })
                    .collect::<Result<Vec<Row>, PageError>>()?;

                PageBody::Leaf {
                    next: next_page,
                    prev: prev_page,
                    records,
                }
            }
            INTERNAL_TAG => {
                let mut children = Vec::with_capacity(num_items + 1);
                for _ in 0..num_items + 1 {
                    children.push(
                        PageId::deserialize(&mut cursor)
                            .expect("slot array fits: num_items is bounded"),
                    );
                }
                let ranges = Self::read_slot_ranges(&mut cursor, num_items).map_err(corrupt)?;
                let keys = ranges
                    .into_iter()
                    .enumerate()
                    .map(|(slot, range)| {
                        Self::decode_slot(&buf[range], slot, |slot| CorruptionKind::CorruptKey {
                            slot,
                        })
                        .map_err(corrupt)
                    })
                    .collect::<Result<Vec<Key>, PageError>>()?;
                PageBody::Internal { keys, children }
            }
            FREE_TAG => {
                let next = Option::<PageId>::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
                PageBody::Free { next }
            }
            META_TAG => {
                let root_id = PageId::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
                let mut buf_four = [0u8; 4];
                cursor.read_exact(&mut buf_four)?;
                let page_count = u32::from_be_bytes(buf_four);
                let free_list_head = Option::<PageId>::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::InvalidPointerTag))?;
                let schema = Schema::deserialize(&mut cursor)
                    .map_err(|_| corrupt(CorruptionKind::BadSchema))?;
                let name_len = cursor.read_varint()?;
                let mut name_buffer = vec![0u8; name_len];
                cursor.read_exact(&mut name_buffer)?;
                let table_name = String::from_utf8(name_buffer)?;

                PageBody::Meta {
                    root_id,
                    page_count,
                    free_list_head,
                    schema,
                    table_name,
                }
            }
            _ => unreachable!(),
        };

        let page = Page {
            page_id,
            last_update,
            body,
        };

        // ensure all invariants are upheld
        page.check_invariants().map_err(corrupt)?;

        Ok(page)
    }

    fn encoded_size(&self) -> usize {
        PAGE_SIZE
    }
}

impl Page {
    /// Writes `Page` header data to writer.
    pub(crate) fn write_header<W: Write>(&self, writer: &mut W) -> Result<(), PageError> {
        // Write the header information: Type Tag, LSN, a CRC32 checksum, then if
        // a Leaf node, write the Next and Prev pages.
        writer.write_all(&[self.page_type_tag()])?;
        self.page_id.serialize(writer)?;
        self.last_update.serialize(writer)?;
        // place holder for checksum
        writer.write_all(&0u32.to_be_bytes())?;
        match self.body {
            PageBody::Leaf { next, prev, .. } => {
                next.serialize(writer)?;
                prev.serialize(writer)?;
            }
            PageBody::Internal { .. } | PageBody::Free { .. } | PageBody::Meta { .. } => {}
        };

        // note the number of items stored on the page
        let num_items = self.num_items() as u16; // records.len() for Leaf, keys.len() for Internal
        writer.write_all(&num_items.to_be_bytes())?;
        Ok(())
    }

    /// Writes the body of a page to the writer
    /// Returns `slot_offset` and `data_offset` (used for tests to confirm that `free_space` is working properly) for the `Page`
    /// `data_offset` - `slot_offset` is the range of unused space on the `Page`
    pub(crate) fn write_body<W: Write + Seek>(
        &self,
        writer: &mut W,
        header_end: usize,
    ) -> Result<(usize, usize), PageError> {
        let mut data_offset = PAGE_SIZE;
        match &self.body {
            PageBody::Leaf { records, .. } => {
                // Track the positions
                let mut slot_offset = header_end;

                for record in records {
                    // serialize the row into a 'scratch' buffer to measure the length
                    let mut buffer = Vec::new();
                    record.serialize(&mut buffer)?;
                    let len = buffer.len();

                    // shift the offset back, and write the Row at the proper offset
                    data_offset -= len;
                    writer.seek(SeekFrom::Start(data_offset as u64))?;
                    writer.write_all(&buffer)?;

                    // build a SlotEntry to point to the newly written data and write
                    // it at the proper offset
                    let slot_entry = SlotEntry::new(data_offset as u16, len as u16);
                    writer.seek(SeekFrom::Start(slot_offset as u64))?;
                    slot_entry.serialize(writer)?;

                    // advance the running slot_offset
                    slot_offset += SLOT_ENTRY_SIZE;
                }
                Ok((slot_offset, data_offset))
            }
            PageBody::Internal { keys, children } => {
                // children are fixed-width and inserted in order right after the header
                let mut children_pos = header_end;
                for child in children {
                    writer.seek(SeekFrom::Start(children_pos as u64))?;
                    child.serialize(writer)?;
                    children_pos += PAGE_ID_SIZE;
                }

                // Keys: variable-width, same slot+data-area pattern as Leaf's rows.
                let mut slot_offset = children_pos;
                for key in keys {
                    // write key into a scratch buffer and measure size
                    let mut buffer = Vec::new();
                    key.serialize(&mut buffer)?;
                    let length = buffer.len();

                    // adjust data_offset and write the data
                    data_offset -= length;
                    writer.seek(SeekFrom::Start(data_offset as u64))?;
                    writer.write_all(&buffer)?;

                    // write the accompanying SlotEntry
                    let slot_entry = SlotEntry::new(data_offset as u16, length as u16);
                    writer.seek(SeekFrom::Start(slot_offset as u64))?;
                    slot_entry.serialize(writer)?;
                    slot_offset += SLOT_ENTRY_SIZE;
                }
                Ok((slot_offset, data_offset))
            }
            PageBody::Free { next } => {
                // Free pages are just the next pointer, simple enough!
                next.serialize(writer)?;
                Ok((0, 0))
            }
            PageBody::Meta {
                root_id,
                page_count,
                free_list_head,
                schema,
                table_name,
            } => {
                // write the data straight out in order
                root_id.serialize(writer)?;
                writer.write_all(&page_count.to_be_bytes())?;
                free_list_head.serialize(writer)?;
                schema.serialize(writer)?;
                writer.write_all(&table_name.len().encode_var_vec())?;
                writer.write_all(table_name.as_bytes())?;
                Ok((0, 0))
            }
        }
    }

    /// Returns the `Page` represented as raw bytes (`RawPage = [0u8; PAGE_SIZE]`).
    /// Will error if write to internal `Cursor` fails or if the `Page` would overflow
    /// a `RawPage`.
    pub fn as_raw_page(&self) -> Result<RawPage, PageError> {
        self.check_invariants().map_err(|kind| PageError::Corrupt {
            page_id: Some(self.page_id),
            kind,
        })?;
        let mut cursor = Cursor::new([0u8; PAGE_SIZE]);

        self.write_header(&mut cursor)?;

        // Header complete, mark the position.
        let header_end = cursor.position() as usize;

        // write the body (result isn't important here)
        let _ = self.write_body(&mut cursor, header_end)?;

        let mut buf = cursor.into_inner();
        let crc = Self::page_checksum(&buf);
        buf[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());

        Ok(buf)
    }

    pub(crate) fn page_checksum(raw_page: &RawPage) -> u32 {
        let mut crc_stream = Crc32Stream::new();
        crc_stream.update(&raw_page[..CHECKSUM_OFFSET]);
        crc_stream.update(&raw_page[CHECKSUM_OFFSET + 4..]);
        crc_stream.finalize()
    }
}

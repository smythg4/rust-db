## rust-db
A learning project focused on learning more about databases.

### Current Status
Wrapping up phase 1 with an easy to manipulate page structure. Right now the design leads me to read a 4KB chunk of memory, parse it into an in-memory version, perform all `Page` operations on that (`insert`, `split_page`, etc), then it can be serialized back into a `RawPage` of bytes to be flushed back to disk. This will cost a full 4KB read and parse any time a we page something into and out of the `BufferPoolManager`. It's a reasonable cost to pay for now to ensure correctness.

Once done with phase 1, I need to decide if I want the `BufferPoolManager` to be `async` from the get-go. I'm thinking yes because I'd hate to have retrofit everything back after building up a `BTree`. This is where `Go` would be a little nicer...

#### Testing
Recent work has been primarily focused on good property based tests with the `quickcheck` crate. I was able to implement `Arbitrary` for all my base types and put operations through the wringer.
  - I'm particularly proud of the ability to generate an arbitrary valid `Page`, serialize it, mutate random bytes within, then reserialize to make sure it never panics, but returns a proper error instead.
  - All basic types are tested with a roundtrip serialization check as well as a check of `.encoded_size()` against the actual encoding
  length, instilling confidence in all calls to `.encoded_size()`.

```
QUICKCHECK_TESTS=10000 cargo test
```

### Immediate To-Do
#### New code with no tests yet
- [x] `leaf_get`: key present → that row; absent → `Ok(None)`; empty leaf → `Ok(None)`; internal page →
`NotLeaf`.
Add present and absent checks to `insertion_order_on_leaves` after every step
- [x] `leaf_records_from` vs `BTreeMap::range(start..)`: start below all keys, equal to a key (included),
between keys, above all keys, empty page; internal page → `NotLeaf`
- [x] `leaf_records_from`: iterator still usable after the key is dropped (guards `use<'a>`)
- [x] `internal_replace_key`: borrow between children of a `new_root` parent, replace the separator →`find_child` routes every key to the page that holds it
- [ ] `internal_replace_key` rejections (page unchanged): new ≤ left neighbor, new ≥ right neighbor,
old key missing (`MissingKey` returns `new`), larger key on a full page (`PageFull`), leaf page
(`NotInternal`)
- [x] `internal_insert`: oversized key → `KeyTooLong`

#### Borrow rejections
- [ ] internal borrows, both directions: donor with 0 keys → `EmptyBorrow`; donor with 1 key → succeeds
- [ ] internal borrows: `self`'s edge key on the wrong side of the separator → `KeysOutOfOrder`
- [ ] internal borrows: donor's edge key on the wrong side of the separator → `KeysOutOfOrder`
- [ ] internal borrows: separator too big for `self` → `PageFull`
- [ ] leaf borrows, both directions: wrong neighbor → `PointerMismatch` (check expected/actual order)
- [ ] leaf borrows: overlapping ranges incl. an equal key → `KeysOutOfOrder`
- [x] leaf borrow from left: destination full → `PageFull`; donor with 1 row → `EmptyBorrow`
- [x] conservation test for `internal_borrow_from_left` (the right-hand version exists)

#### Merges
- [ ] underfull guarantee: two leaves just under `LEAF_UNDERFULL_BYTES` merge successfully
- [ ] underfull guarantee: two internal pages just under `INTERNAL_UNDERFULL_BYTES` + a max-size separator merge successfully
- [x] internal merge with an empty side (0 keys, 1 child) → 1 key, 2 children

#### Accessors and small functions
- [x] `set_lsn`: smaller → `StaleLsnUpdate` (unchanged), equal and larger accepted; LSN survives a round trip
- [ ] `page_id()` after construction, split (new page), and round trip
- [ ] `can_insert_separator`: exact fit → true, one byte over → false, leaf → false
- [ ] `is_underfull`: exactly at each threshold, both page types
- [ ] `next`/`set_next`/`prev`/`set_prev` on an internal page → `NotLeaf`, page unchanged
- [ ] `split_page`: 0 or 1 rows, or fewer than 3 keys → `TooSmallToSplit`

#### Routing and indexes
- [ ] `find_child_index`: index matches the linear-scan reference and `child_at(index)` equals the returned ID
- [ ] `child_at` / `key_at`: out of range → `None`; on a leaf → `None`
- [ ] `ChildIndex` navigation: index 0 has no left sibling/separator; for every child, keys routed to it lie between `key_at(left_separator)` and `key_at(right_separator)`

#### Invariants and size limits
- [ ] `check_invariants` returns each kind: `RowTooLarge`, `KeyTooLarge` (leaf and internal), `ChildCountMismatch`, `UnsortedKeys { at }` (check `at`), `ExceedsCapacity` — build pages from `empty_page`
- [ ] `as_raw_page` refuses a page that fails `check_invariants`
- [ ] property: `as_raw_page` succeeds ⇒ `deserialize` returns the same page
- [ ] string of exactly `MAX_FIELD_LEN` bytes round-trips; one more → `FieldTooLong` with nothing written
- [ ] crafted string length prefix over `MAX_FIELD_LEN` → `FieldTooLong` on read
- [ ] `validate_row`: string field at the limit accepted, one byte over → `FieldTooLong`
- [ ] `Row::deserialize`: field count over `MAX_NUM_FIELDS` → `TooManyFields`

#### Corruption kinds without a targeted test
- [ ] `InvalidTag`, `InvalidPointerTag`, `CorruptRow`, `MissingKey`, `InvalidKey`, `UnsortedKeys` (via slot swap), `ExceedsCapacity`, `RowTooLarge` / `KeyTooLarge` from `deserialize`

#### Generators and helpers
- [x] every `RowValue` and `ColumnType` variant is generated
- [x] `higher_key(k) > k`
- [ ] `MAX_*_ITEMS` never too low (smallest distinct entries never exceed it)
- [ ] every shrink candidate passes `check_invariants`

#### Durability
- [ ] Torn write: old/new page spliced at any offset decodes to old, new, or `Corrupt` — never a third page

#### Before tests can cover them
- [ ] `Meta` / `Free`: finish or remove — `todo!()` / `unreachable!()` in `free_space`, `write_header`,
`write_body`, `check_invariants`, `split_page`, `entries_size`; then add round-trip and corruption tests


### Phase 0 — Types
- When following the [cstack database tutorial](https://github.com/smythg4/cstack_db), raw `u32` and `usize` abounded. This time, I opted to create custom types for things like `PageId`, `SlotIndex`, `SlotEntry`, `Key`, `Row`, `RowValue`, and `ValidatedRow` for example.
- Compile time checks will prevent users from using the wrong type of arguments (e.g. `PageId` when `SlotIndex` is required).
- One very nice touch is the concept of `ValidatedRow`. A `Table` can hold a `Schema`. When performing an `insert` operation, the `Page` object requires that argument is a `ValidatedRow`. `Schema` includes a method called `validate_row(row: Row) -> Result<ValidatedRow, SchemaError>`, which ensures that any `Row` inserted into a `Page` conforms to the `Schema`'s rules to prevent the input of junk data.
- A lot of work is being done by a `Serializable` trait. Any type that's going to or from a raw byte format is required to implement this trait. This allows smooth composition for something like `Page` to `serialize` or `deserialize` component parts to/from anything that implements `Write`/`Read`.
- Current shortcomings:
  - There's lots of cloning and allocations going on (e.g. `ValidatedRow` -> `Key` clones the underlying `String` if that's its type).
  - I may swap out `Key::String(String)` for `Key::String(&str)`, but I'm somewhat dreading the injection of a million lifetimes. Perhaps `Key::String(Cow<str>)` or `Key::String(Arc<str>)` will be the right call.

### Phase 1 — Storage Layout
- In-table data is represented as a `RowValue`, which currently supports `Integer(i64)`, `String(String)`, `Boolean(bool)`, `Float(f64)`, and `Null`.
- `Schemas` hold `Columns` that are made up of `ColumnType` and a `nullable` flag. Primary Keys are always stored in the first element of the underlying `Vec`. Primary Keys can only be non-nullable `String` or `Integer` right now and a new `Schema` will be rejected if the first entry doesn't meet these requirements.
- The fundamental unit of storage `Page` holds core metadata like `page_id: PageId` and `Lsn` (not currently used, but will be important for WAL implementation), as well as a `PageBody` that is either a `Leaf` or `Internal`.
  - `Internal` page bodies hold a list of keys and child `PageId`s. There should always be 1 more child than keys. This is enforced through `debug_assert!`s for operations on `Page`s and `PageError::Corrupt { kind }` for deserialization.
  - `Leaf` page bodies hold a list of `Rows` and sibling pointers (`next: Option<PageId>`, `prev: Option<PageId>`) to allow quicker sequential scans.
```
#[derive(Debug, PartialEq, Clone)]
pub struct Page {
    page_id: PageId,
    last_update: PageLsn,
    body: PageBody,
}

#[derive(Debug, PartialEq, Clone)]
pub enum PageBody {
    Leaf {
        records: Vec<Row>,
        next: Option<PageId>,
        prev: Option<PageId>,
    },
    Internal {
        keys: Vec<Key>,
        children: Vec<PageId>,
    },
}
```
- All numerical encoding is in Big Endian order.
- Byte manipulation lives in exactly one place: the serialize/deserialize pair. Everything above that boundary works with typed data, not raw `&mut [u8]` — this is what makes the offset/node-type-confusion bug class from cstack_db structurally impossible here.
- One major shortcoming at this juncture is the need to read in the full 4KB page off disk and deserialize into this in-memory representation any time I go through the BPM.
  - It's commented out right now, but my plan is to define a trait for `Page` that I can implement for a pure, raw-byte page representation and swap my current implementation out for something that's closer to 'zero-copy'.
  - I think I'll be able to keep my tests for the new `Page` implementation if I implement `Arbitrary` for the new type that makes a `NaivePage` (the current type), converts it to raw bytes, then reads those in.
    - As I think about this more, I'm probably going to want the raw bytes version of `Page` to use tombstone markers since removal isn't nearly as easy as with a `Vec`. When I generate an arbitrary `NaivePage` insert some random sentinel entries that we convert to tombstones for the new `Page` layout.

#### Page layout (4096 bytes)

  | Header | Slot array → | Free space | ← Row / key data |
  |:---:|:---:|:---:|:---:|
  | fixed fields | grows toward the end | shrinks from both sides | grows toward the front |

  All integers are big-endian. `Option<PageId>` fields are a 1-byte tag (`0` = `None`, `1` = `Some`) followed by the 8-byte `PageId` only when `Some`.
  Checksums are computed over the page with the checksum field excluded.

  #### Leaf header (≤ 41 bytes)

  | Offset | Field | Size (bytes) | Notes |
  |---:|---|---:|---|
  | 0 | `tag` | 1 | `1` = leaf |
  | 1 | `page_id` | 8 | `TableId` (u32) + page number (u32) |
  | 9 | `lsn` | 8 | `0` = no LSN yet |
  | 17 | `checksum` | 4 | `u32` |
  | 21 | `next` | 1 + 8 | `Option<PageId>` |
  | 30 | `prev` | 1 + 8 | `Option<PageId>` |
  | 39 | `num_items` | 2 | number of records |

  #### Internal header (≤ 23 bytes)

  | Offset | Field | Size (bytes) | Notes |
  |---:|---|---:|---|
  | 0 | `tag` | 1 | `2` = internal |
  | 1 | `page_id` | 8 | `TableId` (u32) + page number (u32) |
  | 9 | `lsn` | 8 | `0` = no LSN yet |
  | 17 | `checksum` | 4 | `u32` |
  | 21 | `num_items` | 2 | number of keys |
  | 23 | `children` | 8 × (`num_items` + 1) | child `PageId`s, followed by the slot array |

  Offsets assume every `Option` is `Some`. Each `None` moves the fields after it 8 bytes earlier.

  #### Slot entry (4 bytes)

  | Field | Size (bytes) | Notes |
  |---|---:|---|
  | `offset` | 2 | byte offset from the start of the page |
  | `length` | 2 | length of the encoded row or key |
  
### Phase 2 — BufferPoolManager
- Reads pages off disk, deserializes into a proper `Page` struct, serializes
  back to bytes only at the swap boundary (eviction or shutdown).
- Need to design all BPM methods to be `&self` to enable concurrency in later stages
- `RwLock`-guarded pages (`RwLock<Option<Box<PageFrame>>>` per frame).
  - `RwLock` chosen deliberately for real cross-thread concurrency, not by
    default — `RefCell` gets the same `&self`-based win far cheaper if this
    stays single-threaded.
- Pin counting via RAII guard (increment on fetch, decrement on `Drop`) — a
  bookkeeping mechanism that informs the eviction policy if it's safe to evict.
- Clock eviction policy: reference bit per frame, set on access, cleared as
  the clock hand sweeps past without evicting. Only `pin_count == 0` frames
  are eligible.
    - This will be a separate type stored by the BPM. Consider defining a trait `EvictionPolicy` to allow easy swap outs and experimentation with different policies.
- Dirty bit set on write-guard drop
- One access path only (`fetch_page`, always faults in on a miss) — no
  separate forcing/non-forcing variants. Half of cstack_db's session-restart
  bugs were exactly a call site using the read-only accessor that returned
  `None` on a cache miss instead of loading from disk.
- `DiskManager` layer will contain methods like `read_page(id: PageId)` and `write_page(raw: &RawPage)`.
  - File listing could be a map with `TableId` -> `Path` and `PageId` -> `offset`.
- `ReadGuard`s will need to implement `Deref` and `WriteGuard`s will need to implement `Deref` and `DerefMut` for `Page` so I can use them with `Page` operations.

```
struct BufferPoolManager<Dm: DiskManager> {
    persistant_layer: Dm,
    eviction_policy: EvictionPolicy,
    wal: Option<Wal>,
    frames: [RwLock<Option<Box<PageFrame>>>; BUFFER_SIZE],
    ...
}
```

### Phase 3 — BTree over the BPM
- `BTree` struct holding a reference to the BPM (similar to `Table` and `Pager` from cstack).
```
pub struct BTree {
  id: TableId,
  bpm: Arc<BufferPoolManager>,
  root_page: PageId,
  schema: Schema,
  ...
}
```
- `PageId` of `0` should be a metadata page containing all the core information.
- Delete, including underflow handling (merge with / borrow from a sibling).
  Conspicuously absent from cstack_db — this is the mirror image of
  split-on-insert, and at least as fiddly as the key-promotion bookkeeping in
  `internal_node_split_and_insert` was, just in reverse.
- Free-list / space map for page allocation, replacing cstack_db's
  `get_unused_page_num` (which only ever grows). Needed once delete exists so
  freed pages are reusable.
- A minimal catalog/system table to durably store user-defined schema
  definitions themselves, since schemas are no longer fixed at compile time.
- Implement a `vacuum` method that performs:
  - Complete sequential scan, gathering all records in one place.
  - Builds full leaf `Page`s out of the collection and connects sibling pointers
  - Bottom up construction of internal `Page`s until reaching the root
  - Update `self.root_page`

### Phase 3.5 - REPL / Dumb Queries
- Now the project is ready to interact with, implement a simple REPL and allow some basic "stored procedures" like `INSERT <Row>`, `SELECT <Key>`, `DELETE <Key>`, `UPDATE <Key> <Row>`.
- Maybe I'll start with a default dummy `Schema` to avoid all the `Table` declarations with the REPL.

### Phase 4 — Concurrency
- `RwLock`-guarded pages (built in Phase 2) used for real.
- Latch crabbing (lock coupling) for B+Tree traversal: hold the parent's
  latch, acquire the child's, release the parent once the child is confirmed
  safe (won't split/merge). Standard technique for concurrent tree traversal
  without one thread pinning the root latch for an entire operation.

### Phase 5 — WAL / ARIES
Hardest item on the list — sequence deliberately rather than attempting full
ARIES in one pass:
1. Redo-only logging + crash recovery first (log before touching the page,
   replay on restart).
2. Undo + CLRs (compensation log records) for transaction abort, once
   redo-only recovery is solid.
3. Invariant to never violate: the log record for a change is durable
   *before* the corresponding page write hits disk. Every page carries the
   LSN of its last modifying record.
- WAL records are generated at the BTree call site (logical redo/undo info —
  "Inserted key K into page P" — lives there, not in the buffer
  pool). `WriteGuard::drop` does **not** push to the WAL; it only marks the
  frame dirty. WAL-before-data is enforced at the *other* end: in the BPM's
  flush/eviction path, before writing a dirty page, force the log manager to
  durably flush up through that page's stamped LSN.
- Build a crash-test harness as a real deliverable (kill the process
  mid-transaction, restart, assert recovery produces a consistent state) —
  the only way to know ARIES is actually correct rather than "looks right".
- Will need to devise a protocol for logging splits and merges and the multiple page modifications that result.

### Phase 6 — TransactionManager
- Basic `TransactionManager`: begin/commit/abort, transaction IDs, hooked
  into WAL. A single global lock serializing all transactions is a
  reasonable v1 concurrency model — get commit/abort/WAL integration correct
  before attempting real isolation levels (2PL/MVCC).
- Add `BEGIN`, `ABORT`, and `COMMIT` to the dumb REPL.

### Phase 7 - QueryEngine
- Basic `QueryEngine`: thin dispatch layer once everything below it works,
  similar in spirit to `vm.rs` from cstack's tutorial.
- Add support for range selections
- Maybe support `JOIN`? That's gonna be fun.

### Phase 8 - Consensus
- RAFT or VSR, whichever I find easier to implement
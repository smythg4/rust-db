## rust-db
A learning project focused on learning more about databases.

### Current Status
Wrapping up phase 1 with an easy to manipulate page structure. Right now the design leads me to read a 4KB chunk of memory, parse it into an in-memory version, perform all `Page` operations on that (`insert`, `split_page`, etc), then it can be serialized back into a `RawPage` of bytes to be flushed back to disk. This will cost a full 4KB read and parse to perform any operation, instead of working directly on the bytes, but has made writing tests far easier so I have the confidence to move on to phase 2.

#### Testing
Recent work has been primarily focused on good property based tests with the `quickcheck` crate. I was able to implement `Arbitrary` for all my base types and put operations through the wringer.
  - I'm particularly proud of the ability to generate an arbitrary valid `Page`, serialize it, mutate random bytes within, then reserialize to make sure it never panics, but returns a proper error instead.
  - All basic types are tested with a roundtrip serialization check as well as a check of `.encoded_size()` against the actual encoding
  length, instilling confidence in all calls to `.encoded_size()`.

```
QUICKCHECK_TESTS=10000 cargo test
```

### Immediate To-Do
  - [x] Write `insert` for internal pages - will make current tests much nicer.
  - [x] `Option` deserialize: return an `InvalidData` io error instead of panicking on bad tags
  - [x] Replace `expect`s in `Page::deserialize` with `PageError::Corrupt`; validate invariants on read (valid key
  in field 0, strictly sorted keys, children = keys + 1)
  - [x] Corrupt-input tests: random/mutated page bytes never panic; truncated values return `Err`
  - [x] Cap total row size (vs. usable leaf space) and key size (vs. internal node space) in `validate_row`; unify
  raw vs. encoded length limits
  - [x] Split leaves by bytes, not count, so a post-split retry always fits
  - [x] Split tests: discard only `TooSmallToSplit`; strict leaf sortedness `debug_assert`
  - [x] Generators: integer keys in internal nodes, random `None` pointers (loosen `free_space_works`
  accordingly), fuller pages
  - [x] Round-trip + size tests for `PageLsn`, `Lsn`, `PageId`, `SlotEntry`, `Option<T>`
  - [x] Rewrite `page_insert_returns_page_full_when_full`; tidy `validate_row`, `Row::try_from`,
  `PageLsn::deserialize`
  - [ ] `find_child` tests: boundaries + split-then-route
  - [ ] Handle primative type errors as `PageError::Corrupt` in `Page::deserialize` where appropriate.
  - [x] Add a `merge_page` method for use when deletions reduce page size to half full
  - [x] Write test cases for `merge_page`
    - [x] Test for `PageFull` errors
    - [x] Test for `InvalidMerge` errors (duplicate keys, unsorted keys, bad pointers, page type mismatch)
    - [x] Test for original page preservation after merge failure
    - [x] Test for `Page` split, then remerge. Should always succeed and be byte-for-byte of original. Ensure the new page id is provided as the "Freed page"
  - [ ] Add `internal_remove` method to remove a key from an internal page
  - [ ] Add `leaf_remove` to remove a record from a leaf page
  - [ ] Move common test helpers into a test_support.rs module. Build some more helpers to remove redundancy inside tests.

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
  - `Internal` page bodies hold a list of keys and child `PageId`s. There should always be 1 more child than keys. This is enforced through `debug_assert!`s for operations on `Page`s and `PageError::CorruptData` for deserialization.
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
- One major shortcoming at this juncture is the need to read in the full 4KB page off disk and deserialize into this in-memory representation for any page modifications.
  - It's commented out right now, but my plan is to define a trait for `Page` that I can implement for a pure, raw-byte page representation and swap my current implementation out for something that's closer to 'zero-copy'.
  - I think I'll be able to keep my tests for the new `Page` implementation if I implement `Arbitrary` for the new type that makes a naive `Page` (the current type), converts it to raw bytes, then reads those in.

#### Page layout (4096 bytes)

  | Header | Slot array → | Free space | ← Row / key data |
  |:---:|:---:|:---:|:---:|
  | fixed fields | grows toward the end | shrinks from both sides | grows toward the front |

  All integers are big-endian. `Option<PageId>` fields are a 1-byte tag (`0` = `None`, `1` = `Some`) followed by
  the 8-byte `PageId` only when `Some`.

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
## rust-db
A project focused on learning more about databases.

### Current Status
I added a few `Page` tests and rolled out a super basic REPL for interactive inserts and deletes. Run it with `cargo run --bin repl -- data/test.db --pool-size 100`. CLI arguments are a filepath (required) and pool-size (optional) which defaults to 64.
```
cargo run --bin repl -- data/test.db --pool-size 4
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.10s
     Running `target/debug/repl data/test.db --pool-size 4`
File Size: 0.01MB
rust-db > .table
Users
----------------------------------------------------------------------------------------------------
| id (Integer)                   | email (String*)                | active (Bool)                  |
----------------------------------------------------------------------------------------------------
----------------------------------------------------------------------------------------------------
(0 rows)
rust-db > insert 1 name@email.com true
Insert successful
rust-db > insert 5 name@email.com false
Insert successful
rust-db > insert 1 null false
Error on insert: Attempt to insert a duplicate key
rust-db > insert 2 null true
Insert successful
rust-db > .table
Users
----------------------------------------------------------------------------------------------------
| id (Integer)                   | email (String*)                | active (Bool)                  |
----------------------------------------------------------------------------------------------------
| 1                              | 'name@email.com'               | true                           |
| 2                              | NULL                           | true                           |
| 5                              | 'name@email.com'               | false                          |
----------------------------------------------------------------------------------------------------
(3 rows)
rust-db > delete 5
Removed: Row { fields: [Integer(5), String("name@email.com"), Boolean(false)] }
rust-db > insert 3 null null
Error on insert: Null value in non-nullable column 2
rust-db > .help
   .help   : show this message
   .table  : print the table (first and last 5 rows)
   .quit   : save and exit
   insert  : insert <id> <email|null> <true|false>
   delete  : delete <id>
   select  : select <id>
rust-db > .quit
Rows: 2
File Size: 0.01MB
File Size: 0.01MB
```

Next steps include:
- Abstract out trait layers so `Page` can eventually be swapped out.
- Write many, many, many more tests to hammer `BufferPoolManager`, `FileDisk`, `Table`, and `BTree`.

#### Testing
The `Page` layer is tested extensively, but I still need to spend some time testing everything else. I'm using `quickcheck` and property based tests to ensure a rock solid foundation.

```
QUICKCHECK_TESTS=10000 cargo test
```

### Immediate To-Do

#### Merges
- [ ] underfull guarantee: two leaves just under `LEAF_UNDERFULL_BYTES` merge successfully
- [ ] underfull guarantee: two internal pages just under `INTERNAL_UNDERFULL_BYTES` + a max-size separator merge successfully

#### Accessors and small functions
- [ ] `can_insert_separator`: exact fit → true, one byte over → false, leaf → false
- [ ] `is_underfull`: exactly at each threshold, all 4 page types
- [x] `split_page`: 0 or 1 rows, or fewer than 3 keys → `TooSmallToSplit`

#### Routing and indexes
- [x] `child_at` / `key_at`: out of range → `None`; on a leaf → `None`
- [ ] `ChildIndex` navigation: index 0 has no left sibling/separator; for every child, keys routed to it lie between `key_at(left_separator)` and `key_at(right_separator)`

#### Invariants and size limits
- [ ] `check_invariants` returns: `RowTooLarge`, `KeyTooLarge`.
- [ ] crafted string length prefix over `MAX_FIELD_LEN` → `FieldTooLong` on read
- [ ] `validate_row`: largest payload from `leaf_schema()` helper at the limit accepted, one byte over → `FieldTooLong`

#### Corruption kinds without a targeted test
- [ ] `InvalidTag`, `InvalidPointerTag`, `CorruptRow`, `ExceedsCapacity`, `RowTooLarge` / `KeyTooLarge`, `ExceedsCapacity` from `deserialize`

#### Generators and helpers
- [ ] `MAX_*_ITEMS` never too low (smallest distinct entries never exceed it)

#### `Meta`/`Free` Tests
- [ ] `free_list_push` and `free_list_pop` tests


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
- The fundamental unit of storage `Page` holds core metadata like `page_id: PageId` and `Lsn` (not currently used, but will be important for WAL implementation), as well as a `PageBody` that is either a `Leaf`, `Internal`, `Meta`, or `Free`.
  - `Internal` page bodies hold a list of keys and child `PageId`s. There should always be 1 more child than keys. This is enforced through `debug_assert!`s for operations on `Page`s and `PageError::Corrupt { kind }` for deserialization.
  - `Leaf` page bodies hold a list of `Rows` and sibling pointers (`next: Option<PageId>`, `prev: Option<PageId>`) to allow quicker sequential scans.
  - `Meta` page bodies contain all the `Table` metadata including `root_page_id`, `num_pages`, the table `Schema`, and a `free_list_head` pointer.
  - `Free` page bodies only contain a single value `next: Option<PageId>` and act as entries in a linked list of a `Page`s that have been freed through merge operations.
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
    ...
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
- Is in charge of handing out `Frames` that hold in-memory representations of the `Page`s on disk. It requires both a `DiskManager` and an `EvictionPolicy` on creation.
  - `DiskManager` represents the underlying persistent layer. By making it a trait I am able to keep a quick-and-dirty in-memory version that's just a `HashMap<FrameId, RawPage>` for testing, as well as a `FaultyDiskManager` (planned) to simulate torn writes and other failures. Right now I have an actual `FileDisk` that pushes to/from the filesystem.
  - `EvictionPolicy` is how the `BufferPoolManager` will decide to purge a `Page` from its cache and back to the `DiskManager`. I implemented a simple round-robin version as well as a `ClockEvictor` that is an efficient subsitute for an LRU cache.
    - The `EvictionPolicy` rotates through the `Frames` and finds one that isn't currently 'pinned' (has an open read or write latch to it). It should prefer to find one that isn't 'dirty' (writes not yet pushed to disk) to avoid having to flush it to disk.
- An occupied `Frame` holds a `RwLock` guarding a cached `Page`. This is used for synchronization across multiple threads trying to access the same data.
  - It also holds an atomic counter of how many read or write guards are out there (`pin_count`). This is all controlled using RAII guards. A `Frame` can't be evicted from the pool until the `pin_count` is `0` (otherwise you're leaving a reader with access to whatever `Page` you loaded in its place).
  - The `referenced` `AtomicBool` is used by the `ClockEvictor` to estimate recency of access. It's set to `true` when a `Frame` is pinned, and set to `false` when the `ClockEvictor` is searching for a victim `Frame`.
  - The `dirty` flag indicates if this `Frame` has pending writes that haven't been flushed from the cache. This is set to `true` whenever `.deref_mut()` is invoked on a `WriteGuard`, not when a `Page` is fetched for write access.
- `frames` size is determined at initialization of the `BufferPoolManager`. A higher `pool_size` will reduce the number of times you're needed to shuffle in and out of memory. Remember that my design right now triggers a deserialization everytime you pull something from the `DiskManager` and a serialization everytime you push something to it.
- The internal state is currently protected by a `Mutex`. This includes:
  - `page_table`, which is just a mapping from a `PageId` to the current index in the `frames` array.
  - `free_frames` is the list of unoccupied `Frame`s. This is initialized at start up and drains until everything has been assigned. After that eviction is required to fetch another `Page` from the `DiskManager`.
  - `eviction_policy` was described above. This is inside the lock to make sure it's handing out a valid victim for eviction.
  - `wal` is currently unused, but in a later phase I will wire in a Write Ahead Log.

```
pub struct Frame {
    latch: RwLock<Option<Page>>, // guards the page contents
    pin_count: AtomicU32,
    dirty: AtomicBool,
    referenced: AtomicBool,
}

struct BpmState<Ep: EvictionPolicy> {
    page_table: HashMap<PageId, FrameId>,
    free_frames: Vec<FrameId>,
    eviction_policy: Ep,
    wal: Option<Wal>,
}

pub struct BufferPoolManager<Dm: DiskManager, Ep: EvictionPolicy> {
    frames: Box<[Frame]>,
    persistant_layer: Dm,
    big_dumb_lock: Arc<Mutex<BpmState<Ep>>>,
}
```

### Phase 2.5 - Table
- `Table` holds a reference to a `BufferPoolManager` and stores the metadata `PageId`.
- It's the primary conduit for database interaction with methods like `create`, `allocate`, `free`, `insert`, `delete`.
- TODO: Describe all the core methods.
```
pub struct Table<'a, Dm: DiskManager, Ep: EvictionPolicy> {
    pub(crate) bpm: &'a BufferPoolManager<Dm, Ep>,
    meta_id: PageId,
    schema: Schema,
    table_name: String,
}
```

### Phase 3 — BTree over the BPM
- `BTree` struct holding a reference to the BPM (similar to `Table` and `Pager` from cstack).
```
pub struct BTree<'t, Dm: DiskManager, Ep: EvictionPolicy> {
    table: &'t Table<'t, Dm, Ep>,
}
```
- Right now this is just a wrapper around a `Table` struct that holds a `BufferPoolManager` and a `Schema`.
- Root `PageId` are always read from or written to the underlying metadata `Page` through `ReadGuard`s or `WriteGuard`s, so there shouldn't ever be a race condition resulting in an invalid root `PageId` and subsequent invalid tree traversal.
- Implement a `vacuum` method that performs (this should probably go on `Table`):
  - Complete sequential scan, gathering all records in one place.
  - Builds full leaf `Page`s out of the collection and connects sibling pointers
  - Bottom up construction of internal `Page`s until reaching the root

### Phase 3.5 - REPL / Dumb Queries
- Now the project is ready to interact with, implement a simple REPL and allow some basic "stored procedures" like `INSERT <Row>`, `SELECT <Key>`, `DELETE <Key>`, `UPDATE <Key> <Row>`.
- Maybe I'll start with a default dummy `Schema` to avoid all the `Table` declarations with the REPL.

### Phase 4 — Concurrency
- Tree descents all follow a similar pattern: 
```
        let mut curr_page = self.get_root_write()?;

        // a stack of parents traversed on the way down to finding the leaf page
        // for insertion. Holding `WriteGuards` along the way.
        let mut ancestors = Vec::new();

        while curr_page.is_internal()
            && let Some(ch) = curr_page.find_child(&key)
        {
            ancestors.push(curr_page);
            curr_page = self.table.bpm.fetch_write(ch)?;
            ...
```
- Grabbing a write latch at the root, then following it down until hitting a leaf, pushing guards onto an `ancestors` stack along the way.
  - Read only operations like `get` don't require an `ancestor` stack.
- Once an operation is safe (e.g. a `Page` can fit a max-sized entry, or if a `Page` below it split or merged with its neighbor this one can definitely handle the `Key` deletion or insertion), we clear the the ancestors `Vec`, releasing all the latches above and allowing another thread to grab the root.
  - This is a pessimistic approach to latching with an 'early' release on `ancestor` latches.
  - Alternatively I could try an optimistic approach where I only take read guards all the way down and if the leaf is safe to `insert` or `delete`, only take that write guard. Otherwise, trace back to the highest write guard I'd need to perform the operation.

### Phase 4.5 - Bloom Filters
- A quick way to test if an element is in the `Table` or not. Avoiding unecessary latch crabbing and speeding things up in cases where we know an entry isn't in the tree.

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
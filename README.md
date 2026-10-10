## rust-db
A learning project about databases: B+Tree Storage Engine, Buffer Pool Manager, Concurrent Operations.

Still to come: Write Ahead Logs, Transactions, Query Engine, and eventually Consensus.

### Current Status
I spent some time restructuring the project and tidying up the REPL output. I added a couple basic `BTree` tests, but I still have a lot of tests to write before moving on to the `Wal`.

Run simple REPL with `cargo run --bin repl -- data/test.db --pool-size 100`. CLI arguments are a filepath (required) and pool-size (optional) which defaults to 64.

![rust-db REPL demo](docs/demo.gif)

Next steps include:
- Abstract out trait layers so `Page` can eventually be swapped out.
- Write many, many, many more tests to hammer `BufferPoolManager`, `FileDisk`, `Table`, and `BTree`.

#### Testing
The `Page` layer is tested extensively, but I still need to spend some time testing everything else. I'm using `quickcheck` and property based tests to ensure a rock solid foundation.

```
QUICKCHECK_TESTS=10000 cargo test
```

### Immediate To-Do
[To Do List](TODO.md)

### Phase 0 — Types (Done)
- When following the [cstack database tutorial](https://github.com/smythg4/cstack_db), raw `u32` and `usize` abounded. This time, I opted to create custom types for things like `PageId`, `SlotIndex`, `SlotEntry`, `Key`, `Row`, `RowValue`, and `ValidatedRow` for example.
- Compile time checks will prevent users from using the wrong type of arguments (e.g. `PageId` when `SlotIndex` is required).
- One very nice touch is the concept of `ValidatedRow`. A `Table` can hold a `Schema`. When performing an `insert` operation, the `Page` object requires that argument is a `ValidatedRow`. `Schema` includes a method called `validate_row(row: Row) -> Result<ValidatedRow, SchemaError>`, which ensures that any `Row` inserted into a `Page` conforms to the `Schema`'s rules to prevent the input of junk data.
- A lot of work is being done by a `Serializable` trait. Any type that's going to or from a raw byte format is required to implement this trait. This allows smooth composition for something like `Page` to `serialize` or `deserialize` component parts to/from anything that implements `Write`/`Read`.
- Current shortcomings:
  - There's lots of cloning and allocations going on (e.g. `ValidatedRow` -> `Key` clones the underlying `String` if that's its type).
  - I may swap out `Key::String(String)` for `Key::String(&str)`, but I'm somewhat dreading the injection of a million lifetimes. Perhaps `Key::String(Cow<str>)` or `Key::String(Arc<str>)` will be the right call.

### Phase 1 — Storage Layout (Done for Now)
- In-table data is represented as a `RowValue`, which currently supports `Integer(i64)`, `String(String)`, `Boolean(bool)`, `Float(f64)`, and `Null`.
- `Schemas` hold `Columns` that are made up of `ColumnType` and a `nullable` flag. Primary Keys are always stored in the first element of the underlying `Vec`. Primary Keys can only be non-nullable `String` or `Integer` right now and a new `Schema` will be rejected if the first entry doesn't meet these requirements.
- The fundamental unit of storage `Page` holds core metadata like `page_id: PageId` and `Lsn` (not currently used, but will be important for WAL implementation), as well as a `PageBody` that is either a `Leaf`, `Internal`, `Meta`, or `Free`.
  - `Internal` page bodies hold a list of keys and child `PageId`s. There should always be 1 more child than keys. This is enforced through `debug_assert!`s for operations on `Page`s and `PageError::Corrupt { kind }` for deserialization.
  - `Leaf` page bodies hold a list of `Rows` and sibling pointers (`next: Option<PageId>`, `prev: Option<PageId>`) to allow quicker sequential scans.
  - `Meta` page bodies contain all the `Table` metadata including `root_page_id`, `num_pages`, the table `Schema`, and a `free_list_head` pointer.
  - `Free` page bodies only contain a single value `next: Option<PageId>` and act as entries in a linked list of `Page`s that have been freed through merge operations.
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
  
### Phase 2 — BufferPoolManager (Done for Now, Tests Pending)
- Is in charge of handing out `Frames` that hold in-memory representations of the `Page`s on disk. It requires both a `DiskManager` and an `EvictionPolicy` on creation.
  - `DiskManager` represents the underlying persistent layer. By making it a trait I am able to keep a quick-and-dirty in-memory version that's just a `HashMap<PageId, RawPage>` for testing, as well as a `FaultyDiskManager` (planned) to simulate torn writes and other failures. Right now I have an actual `FileDisk` that pushes to/from the filesystem.
  - `EvictionPolicy` is how the `BufferPoolManager` will decide to purge a `Page` from its cache and back to the `DiskManager`. I implemented a simple round-robin version as well as a `ClockEvictor` that is an efficient substitute for an LRU cache.
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

### Phase 2.5 - Table (Done for Now, Tests Pending)
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

### Phase 3 — BTree over the BPM (Done for Now, Tests Pending)
- `BTree` struct holding a reference to the BPM (similar to `Table` and `Pager` from cstack).
```
pub struct BTree<'t, Dm: DiskManager, Ep: EvictionPolicy> {
    table: &'t Table<'t, Dm, Ep>,
}
```
- Right now this is just a wrapper around a `Table` struct that holds a `BufferPoolManager` and a `Schema`.
- Root `PageId` are always read from or written to the underlying metadata `Page` through `ReadGuard`s or `WriteGuard`s, so there shouldn't ever be a race condition resulting in an invalid root `PageId` and subsequent invalid tree traversal.
- I implemented a `vacuum` method on `Table` that will sequentially scan all the leaf `Page`s and collect the underlying `Row`s.
  - It then constructs a series of tightly packed leaf `Page`s and recursively generates internal `Page`s on top until the `Page` layer's length is 1, meaning we're at the root `Page`.
  - Then I generate a new meta `Page` directed to the appropriate root.
  - This collection of `Page`s is written to a temp file, then renamed to the `Table`s source file's name. The source directory is `sync`ed and the file is reopened and assigned to `Table`'s internal file handle.
  - The `big_dumb_lock` is held during the file rename, so there shouldn't be any races to worry about.
  - **Tradeoff:** I'm packing leaves as tight as possible, which means a single `insert` after a `vacuum` will immediately result in a split that will cascade all the way up the tree.
    - I could consider packing the leaves only 90% full to avoid this.

### Phase 3.5 - REPL / Dumb Queries (Constant Work in Progress)
- I have a super simple REPL that supports `INSERT <Row>`, `SELECT <Key>`, and `DELETE <Key>`.
- I added a rudimentary `WHERE` clause option that currently only supports column name and value (e.g. ... `WHERE active true`).
- Next Steps here are to build out an actual `SQL` parser that translates user input into executable actions for the storage engine and REPL.

### Phase 4 — Concurrency (Done for Now)
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
- Once an operation is safe (e.g. a `Page` can fit a max-sized entry, or if a `Page` below it split or merged with its neighbor this one can definitely handle the `Key` deletion or insertion), we clear the ancestors `Vec`, releasing all the latches above and allowing another thread to grab the root.
  - This is a pessimistic approach to latching with an 'early' release on `ancestor` latches.
  - Alternatively I could try an optimistic approach where I only take read guards all the way down and if the leaf is safe to `insert` or `delete`, only take that write guard. Otherwise, release everything and try again using `WriteGuard`s.

### Phase 5 — WAL / ARIES (Planned Next)
- Next new feature to roll out. `Wal` actions will need to occur before the push to the `DiskManager` managed through the `BufferPoolManager`.
- I imagine them being physiological actions such as "insert row r into leaf page n", "insert key k into internal page m", "update root_page to n on meta page" for example.
- When `write` actions occur, we can get an `Lsn` from the `Wal` that will update the `Page` before the `write` completes. The action can be put on a `Wal` backlog.
  - When the `Frame` is evicted and `Page` is written to disk, first we will ensure that we `sync` the `Wal` and track the `Lsn`s that have been flushed.
- Periodically we need a `checkpointer` that flushes the `Wal` entries to disk and records the `Lsn` for that event.
  - On replay, we need to review all the `Page`s with an `Lsn` before the latest 'flushed' `Lsn` and complete the actions on those `Page`s before allowing the user to access them.
- Rough order of implementation:
1. Redo-only logging + crash recovery
2. Undo + CLRs (compensation log records) for transaction abort
- Build a crash-test harness as a real deliverable (kill the process
  mid-transaction, restart, assert recovery produces a consistent state) —
  the only way to know ARIES is actually correct rather than "looks right".
- Will need to devise a protocol for logging splits and merges and the multiple page modifications that result.

### Phase 6 — TransactionManager (Planned)
- Basic `TransactionManager`: begin/commit/abort, transaction IDs, hooked
  into WAL. A single global lock serializing all transactions is a
  reasonable v1 concurrency model — get commit/abort/WAL integration correct
  before attempting real isolation levels (2PL/MVCC).
- Add `BEGIN`, `ABORT`, and `COMMIT` to the dumb REPL.

### Phase 7 - QueryEngine (Planned)
- Basic `QueryEngine`: thin dispatch layer once everything below it works,
  similar in spirit to `vm.rs` from cstack's tutorial.
- Add support for range selections
- Maybe support `JOIN`? That's gonna be fun.

### Phase 8 - Consensus (Eventually)
- RAFT or VSR, whichever I find easier to implement
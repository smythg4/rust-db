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
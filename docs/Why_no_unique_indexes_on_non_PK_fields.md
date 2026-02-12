User: Hi. I'm using this repo in another project and just hit a big surprise. I can easily add the `NOT NULL` to the BLOB primary keys in my SQLite3 database, but I'm really sad/worried about having to throw out unique indexes on non-PK fields.

Would you please evaluate this repo and let me know whether it might be possible to use this library to keep my database instances in sync without removing all my non-PK unique indexes? If we modify this repo to allow unique indexes on non-PK fields, what might be the consequences?

The following is from another Claude chat session:

The errors you're seeing are schema compatibility issues - cr-sqlite has strict requirements for CRR (Conflict-free Replicated Relations) tables:

No unique indices besides the primary key
Non-nullable primary key required...

Now we see the real problem. cr-sqlite **enforces** that PKs must be declared `NOT NULL` in the schema - our check relaxation doesn't help.

The situation:
- **24 tables** have unique indices (cr-sqlite incompatible without removing them)
- **15 tables** passed the unique check but have nullable PKs in schema

To use cr-sqlite, we need a **schema migration** that:
1. Adds `NOT NULL` to all PK columns (requires table recreation in SQLite)
2. Optionally removes unique indices (but this affects data integrity)

This is a significant change. Options:

**Option A**: Create a migration to fix the 15 tables that only need `NOT NULL` on PKs - these can work with cr-sqlite immediately.

**Option B**: Full migration of all 39 tables - requires deciding what to do about unique indices (remove them or keep those tables out of sync).

GitHub Copilot: 

 

Created 4 todos

Searched for text `unique index`, 20 results

Searched for regex `unique.*index|no_unique|UNIQUE|uniqueIndex` (`**/*.rs`), 20 results

Searched for regex `unique.*index|UNIQUE|no_unique` (`**/*.c`), 20 results

Searched for regex `non_pk_unique|non_pk.*unique|unique.*non.*pk|has_unique|unique_idx_check|unique_index_check|no_unique_index` (`**/core/rs/core/src/**"`), no results

Searched for regex `non.*pk.*unique|unique.*constraint|unique_indices_are_not_supported|unique.*not.*allow`, 20 results

Searched for regex `is_table_compatible|validate.*table|crr.*valid|table_info.*unique|check.*unique|ensure.*no.*unique`, 20 results

Searched for regex `crsql_as_crr|as_crr|create_crr`, 20 results

Now let me read the key files — the `is_table_compatible` function, the `create_crr` module, and the relevant test: 

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#1020-1020), lines 1020 to 1130

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/create_crr.rs)

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/integration_check/src/t/tableinfo.rs#170-170), lines 170 to 300

Starting: *Understand the CRR design rationale* (2/4)

Now let me understand the merge/sync logic — how changes are applied, and what would happen if two nodes independently created rows that collide on a unique index: 

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#1-1), lines 1 to 100

Searched for regex `fn merge|fn apply_changes|fn merge_insert|fn merge_update|fn merge_delete` (`**/core/rs/core/src/**`), 3 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#1-1), lines 1 to 50

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#229-229), lines 229 to 560

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#560-560), lines 560 to 732

Now let me look at how the merge insert statements are constructed — specifically the SQL that inserts into the actual user table during merge: 

Searched for regex `merge_insert_stmt|merge_pk_only_insert|INSERT OR REPLACE|INSERT OR IGNORE|ON CONFLICT` (`**/core/rs/core/src/**`), 20 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#690-690), lines 690 to 850

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#390-390), lines 390 to 510

Now let me look at the triggers that cr-sqlite creates — these are what enforce CRR behavior on local writes: 

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/triggers.rs)

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/notes.md)

Now let me look at the spec doc and the README for a deeper understanding of the design rationale: 

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/py/correctness/spec.md#1-1), lines 1 to 100

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/README.md)

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/README.md#1-1), lines 1 to 200

Now let me look at how the changes virtual table reads changes (the changes-vtab.c and changes_vtab_read.rs): 

Analyze changes vtab read logic

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_read.rs)

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/backfill.rs#1-1), lines 1 to 150

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/src/changes-vtab.c#1-1), lines 1 to 150

Searched for regex `unique.*index|unique.*constraint|UNIQUE|forbidden|not allowed|non-pk|non_pk`, 50 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/src/changes-vtab.c#150-150), lines 150 to 188

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/backfill.rs#150-150), lines 150 to 239

Searched for files matching `**/changes_vtab_write*`, 1 match

Searched for regex `CONSTRAINT|constraint.*violat|merge_insert|merge.*insert|INSERT.*OR|on_conflict`, 20 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#1-1), lines 1 to 200

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/py/correctness/spec.md#1-1), lines 1 to 100

Searched for regex `unique.*index|create_crr|as_crr|CREATE.*INDEX.*UNIQUE` (`**/core/rs/**`), 20 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#200-200), lines 200 to 450

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/create_crr.rs)

Searched for regex `unique|UNIQUE|unique_index|is_unique` (`**/core/rs/**`), 20 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/changes_vtab_write.rs#450-450), lines 450 to 732

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#1030-1030), lines 1030 to 1080

Searched for regex `get_merge_insert_stmt|merge_insert|merge_pk_only|merge_delete_stmt|INSERT.*INTO|INSERT OR` (`**/core/rs/core/src/tableinfo.rs`), 20 results

Searched for regex `INSERT OR REPLACE|INSERT OR IGNORE|SQLITE_CONSTRAINT|constraint.*error` (`**/core/rs/core/src/**`), 9 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#380-380), lines 380 to 750

Searched for regex `fn get_merge_insert_stmt|ColumnInfo|struct ColumnInfo` (`**/core/rs/core/src/tableinfo.rs`), 10 results

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#850-850), lines 850 to 1000

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#760-760), lines 760 to 855

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#300-300), lines 300 to 380

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/core/rs/core/src/tableinfo.rs#1040-1040), lines 1040 to 1132

Read [](file:///home/mateusz/Git/Infocorder-com_cr-sqlite/notes.md#1-1), lines 1 to 60

Starting: *Evaluate feasibility and consequences* (4/4)

Completed: *Evaluate feasibility and consequences* (4/4)

Here's my analysis after a thorough review of the codebase:

---

## Can you keep non-PK unique indexes with cr-sqlite?

**Short answer: No — the restriction is deeply architectural, not just a superficial validation check.** Removing the check in `is_table_compatible` would let the table become a CRR, but merges would crash with `SQLITE_CONSTRAINT` errors at runtime. Here's why.

### Where the restriction lives

A single guard in tableinfo.rs blocks `crsql_as_crr()`:

```rust
// No unique indices besides primary key
if db.count(&format!(
    "SELECT count(*) FROM pragma_index_list('{table}')
        WHERE \"origin\" != 'pk' AND \"unique\" = 1"
))? != 0
```

### Why it exists — the fundamental conflict

cr-sqlite tracks and merges changes **one column at a time**. Each non-PK column gets its own independent version clock. The merge SQL for a single column change looks like:

```sql
INSERT INTO "my_table" (pk_cols..., "changed_col")
VALUES (?, ?)
ON CONFLICT DO UPDATE SET "changed_col" = ?
```

That `ON CONFLICT` clause is **keyed only on the primary key**. If a unique index exists on `changed_col` (or any other non-PK column), SQLite sees it as a *different* conflict target — and there is **zero handling** for `SQLITE_CONSTRAINT` errors in the merge path. The merge simply fails.

### The deeper problem: convergence is impossible

Even if you added error handling, the math doesn't work out:

1. **Node A** creates row `(pk=1, email='alice@test.com')`
2. **Node B** creates row `(pk=2, email='alice@test.com')`
3. Both are valid locally. When they sync, **one must lose** — but cr-sqlite's column-level LWW (Last Writer Wins) resolution compares values **within the same row**, not **across rows**. There is no cross-row conflict detection.
4. Whichever row syncs second will cause a `SQLITE_CONSTRAINT` violation. Even if you suppressed it, the two nodes would **permanently diverge** — Node A would have `{pk=1}`, Node B would have `{pk=2}`, and neither would accept the other's row. This violates the fundamental convergence guarantee of CRDTs.

### What would it take to actually support this?

Supporting unique indexes properly would require:

| Change | Scope | Difficulty |
|---|---|---|
| Change `ON CONFLICT` to target both PK and unique indexes | Merge SQL generation in `ColumnInfo::get_merge_insert_stmt` | Medium |
| Add cross-row conflict detection during merge | New merge logic — when inserting a row, check if an existing *different* row already holds the unique value | Hard |
| Decide on a conflict resolution policy for unique violations | Design question: evict the old row? Reject the new one? Merge them? What happens to the evicted row's clock data? | Very Hard (design) |
| Handle cascading effects | Evicting a row means its clock data, causal length, and sentinel need to be cleaned up or redirected | Hard |
| Ensure convergence | Both nodes must deterministically arrive at the **same** decision about which row keeps the contested unique value, regardless of merge order | Very Hard |
| Handle multi-column unique indexes | A unique index on `(col_a, col_b)` means changes to *either* column can trigger conflicts — but these columns are versioned independently | Very Hard |

This is essentially implementing a **set CRDT with a unique constraint invariant**, which is an open research problem. No mainstream CRDT library provides this out-of-the-box.

### Practical options for your project

**Option 1: Enforce uniqueness in the application layer** *(recommended)*
- Remove the `UNIQUE` indexes from CRR tables
- Replace them with regular indexes (for query performance)
- Enforce uniqueness in your application before writes
- Accept that concurrent writes can create duplicates, and build a reconciliation/dedup process

**Option 2: Split tables into synced and non-synced**
- Keep tables with unique constraints out of `crsql_as_crr()` 
- Only sync the 15 tables that pass the uniqueness check (after fixing `NOT NULL` on PKs)
- Handle the 24 uniquely-indexed tables with traditional sync or treat them as reference/config data

**Option 3: Convert unique indexes to primary key participation**
- If the uniquely-indexed column *truly* identifies the row (like `email`), make it part of the primary key instead
- Composite PKs are fully supported: `PRIMARY KEY (id, email) NOT NULL`
- This makes cr-sqlite's PK-based conflict resolution handle it natively

**Option 4: Relax to "soft" uniqueness**
- Drop the `UNIQUE` index, keep a regular index
- Add a `BEFORE INSERT` trigger that checks for duplicates and applies your preferred policy (reject, update, etc.) for **local** writes only
- Accept that merges can create duplicates, then periodically clean them up

### Bottom line

Simply removing the check in `is_table_compatible` would let you call `crsql_as_crr()`, but the first time two nodes independently create rows that collide on a unique-indexed column, the sync will fail with an unhandled constraint error, and even if that were caught, the databases would stop converging. The restriction isn't arbitrary — it reflects a genuine incompatibility between column-level CRDTs and cross-row uniqueness invariants.
# Pushing the table filter down in the `crsql_changes` virtual table

**Status:** Proposal / design. Revised after measuring SQLite's actual behaviour against the SQL this
vtab generates (bundled SQLite 3.42.0).
**Area:** `crsql_changes` virtual table — `xBestIndex` / `xFilter`.
**Compatibility:** No schema change, no wire/format change, no new C ABI. Purely a query-planner
improvement. Result sets are unchanged for every query **provided** the predicate is emitted as
`CAST(tbl AS TEXT) = ?` **and** the constraint is accepted only under `BINARY` collation (see
[Emitting the predicate with TEXT affinity](#emitting-the-predicate-with-text-affinity)). A bare
`tbl = ?` silently changes results for numerically-named CRR tables; skipping the collation gate
silently changes results for `... = ? COLLATE NOCASE`.

## Summary

`crsql_changes` presents every row of every CRR table's clock as a uniform change-set. It is
implemented as a `UNION ALL` over one subquery per CRR table (each reading that table's
`<table>__crsql_clock`), wrapped as `SELECT ... FROM ( <union> ) <idx_str>`, where `idx_str` is the
`WHERE`/`ORDER BY` text that `xBestIndex` built from the pushed-down constraints.

Today a `WHERE "table" = ?` constraint is **not** pushed down: `constraint_is_usable` reports it
unusable, so it never reaches `idx_str`. SQLite therefore scans **every** table's clock and applies the
`"table"` filter itself, one row at a time, at the vtab layer. Reading the changes for a single table
costs O(total change rows across *all* tables).

**The fix is much smaller than it first appears.** The union arms each select a *constant* table name
(`'items' AS tbl`). Once `tbl = ?` appears in the wrapper's `WHERE`, SQLite's WHERE-clause push-down
optimization copies that term into every compound arm, and — because the term is constant with respect
to each arm's loop — hoists it *above* the arm's cursor opens. Non-matching arms are then skipped
outright at run time: no `OpenRead`, no `Rewind`, no rows.

So the work is confined to `xBestIndex`. **No change to `changes_union_query` is required** to get the
scan win. See [Evidence](#evidence-that-sqlite-already-prunes-the-arms) below.

## The current limitation

The vtab exposes columns `table`, `pk`, `cid`, `val`, `col_version`, `db_version`, `site_id`, `cl`,
`seq`, `ts` (declared in `core/src/changes-vtab.c:28`; `[table]` is `TEXT NOT NULL`). In `xBestIndex`,
`constraint_is_usable` refuses the `table`, `pk` and `val` columns:

```rust
// constraint_is_usable  (core/rs/core/src/changes_vtab.rs:185)
!matches!(
    col,
    CrsqlChangesColumn::Tbl | CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval
)
```

For `SELECT ... FROM crsql_changes WHERE "table" = ?` the constraint is skipped entirely: no
`argvIndex`, no `omit`, no contribution to `idxNum`, and no `tbl = ?` text in `idx_str`. The plan lands
in the "no usable constraints" branch (`estimatedCost = 2147483647.0`).

Note that the machinery to emit the predicate **already exists** and is generic:
`get_clock_table_col_name` (`changes_vtab.rs:200`) already maps `CrsqlChangesColumn::Tbl -> "tbl"`, and
the clock-union subqueries already expose `'<name>' as tbl` (`changes_vtab_read.rs:28`). The only thing
standing between today's behaviour and a pruned scan is the `constraint_is_usable` veto.

**Consequence.** The cost of a table-scoped change read grows with the *whole database's* change volume.
A small table cannot be read cheaply once other tables accumulate changes. This penalises exactly the
workloads that read `crsql_changes` per table: selective sync, per-table backfill or reconciliation,
per-table auditing, and tooling that inspects one table's history.

## Evidence that SQLite already prunes the arms

Reproduced against the bundled SQLite shell (`core/dist/sqlite3`, 3.42.0) and re-checked on 3.50.2,
using the *exact* SQL shape `changes_union_query` generates: the same wrapper column list, two clock
tables, the `__crsql_pks` join, both `LEFT JOIN`s, a bound parameter, and the default
`ORDER BY db_vrsn, seq ASC`. With `WHERE tbl = :t` in the wrapper, `EXPLAIN` shows, at the top of each
compound arm:

```
  3   Ne         6   64   5   BINARY-8  80    if r[5]!=r[6] goto 64    <-- r[5]='a', r[6]=:t
  4   OpenRead   5    2   0   7               a__crsql_clock
  5   OpenRead   6    3   0   0               a__crsql_pks
  ...
 82   Ne         6  143  34   BINARY-8  80    if r[34]!=r[6] goto 143  <-- r[34]='b'
 83   OpenRead   1    4   0   7               b__crsql_clock
 84   OpenRead   2    5   0   0               b__crsql_pks
```

The comparison is evaluated **once per arm, before the arm's tables are opened**. A non-matching arm
contributes zero page reads. Timing (3.50.2 shell, warm cache) on 50,000 clock rows in table
`a` and 3 in table `b`:

| query | rows | time |
|---|---|---|
| union with no `tbl` predicate (today's shape) | 50,003 | 7.0 ms |
| same union with `WHERE tbl = 'b'` in the wrapper | 3 | 0.066 ms |

This is the mechanism the vtab *already* relies on for `db_version`: `pushDownWhereTerms`
(`core/src/sqlite/sqlite3.c:144127`) copies wrapper terms into each `UNION ALL` arm. The preconditions
it requires all hold here — the compound is all `UNION ALL`, there is no `LIMIT` inside the subquery
(user `LIMIT`s are applied by SQLite *above* the vtab, never inside `idx_str`), no window functions, no
recursive CTE, and no `RIGHT`/`FULL` join above the subquery.

> The original draft of this document asserted that `idx_str` "filters the *outer* query — it does not
> reduce which clocks are scanned." That is true for `db_version`/`site_id` in the sense that every arm
> must still be opened, but it is **false** for `tbl`, whose per-arm value is a compile-time constant.
> The corrected premise is what shrinks this proposal from three changes to one.

## The change

### Required: `xBestIndex` accepts an equality constraint on `table`

1. **`constraint_is_usable`** — allow `CrsqlChangesColumn::Tbl` for `INDEX_CONSTRAINT_EQ` only (SQLite
   reports the `IN` operator as `EQ` here, so `IN` comes along for free). Keep `Pk` and `Cval`
   unusable: `pk` is a packed blob produced by `crsql_pack_columns` and `val` is materialised from the
   base table *after* the scan, so neither can be evaluated inside the union.

   ```rust
   fn constraint_is_usable(constraint: &sqlite::index_constraint) -> bool {
       if constraint.usable == 0 {
           return false;
       }
       match CrsqlChangesColumn::from_i32(constraint.iColumn) {
           // `pk` is packed and `val` is materialized after the scan; neither
           // exists in a form the clock union can filter on.
           Some(CrsqlChangesColumn::Pk) | Some(CrsqlChangesColumn::Cval) => false,
           // `tbl` is a constant literal in each union arm, so an equality
           // predicate lets SQLite skip non-matching arms before opening them.
           Some(CrsqlChangesColumn::Tbl) => {
               constraint.op == sqlite::INDEX_CONSTRAINT_EQ as u8
           }
           Some(_) => true,
           None => false,
       }
   }
   ```

   No other code has to change for the predicate to appear in `idx_str`: the existing loop in
   `changes_best_index` already resolves `Tbl -> "tbl"`, appends `tbl = ?`, and assigns the next
   `argvIndex`. `changes_filter` already binds `args` positionally in that same order.

2. **Do *not* set `omit = 1` for the `table` constraint.** The loop currently sets `omit = 1`
   unconditionally; special-case `Tbl` to leave it at `0`:

   ```rust
   constraint_usage[i].argvIndex = arg_v_index;
   constraint_usage[i].omit = if matches!(col, Some(CrsqlChangesColumn::Tbl)) { 0 } else { 1 };
   ```

   `omit = 0` is the strictly-safer choice — never worse than `omit = 1` — for the price of one redundant
   comparison per *returned* row. But be clear about what it does **not** buy: it does not, on its own,
   give parity with the pre-patch vtab, because the pushdown prunes the arm *before* SQLite's re-check can
   see the row. Affinity parity comes from step 3, not from `omit`. Keep `omit = 0` anyway as cheap
   insurance — it costs nothing measurable and guards against a future pruning predicate that is looser
   than SQLite's own check.

3. **Emit the predicate as `CAST(tbl AS TEXT) = ?`, not `tbl = ?`.** This is what actually preserves
   semantics, and it is one token. See
   [Emitting the predicate with TEXT affinity](#emitting-the-predicate-with-text-affinity).

4. **Cost model** — recording a `Tbl` bit in `idxNum` is *necessary but not sufficient*; the cost
   **cascade** has to gain a branch that reads it. Recording the bit —

   ```rust
   Some(CrsqlChangesColumn::Tbl) => idx_num |= 1,
   ```

   — does nothing on its own. Today the cascade's terminal `else` reports `estimatedCost = 2147483647.0`
   for *both* "no usable constraints" and "table only", and setting `idx_num |= 1` leaves that `else`
   untouched, so a table-only plan **still** costs `2147483647.0`. You must add an explicit branch to the
   cost cascade that inspects the new bit and assigns the lower tier:

   ```rust
   // after the db_version / site_id tiers, ahead of the unconstrained fallback:
   } else if idx_num & 1 != 0 {
       // table-only: one table's clock, not the whole union
       estimated_cost = 1000.0;
       estimated_rows = 1000;
   } else {
       estimated_cost = 2147483647.0;
       // ...
   }
   ```

   Why it matters, in **two** cases — not one:

   - **Joins.** When `crsql_changes` appears in a join and the right-hand side of `"table" = x` depends on
     another table, SQLite calls `xBestIndex` once with the constraint usable and once without; with two
     identical `2147483647.0` costs there is nothing to prefer the pushdown, and SQLite may pick the full
     scan.
   - **`IN`.** `whereLoopAddVirtual` explicitly re-calls `xBestIndex` "with `IN(...)` terms disabled"
     (`core/src/sqlite/sqlite3.c:162020`) and compares the two plans. Worse, when the vtab *does* consume
     an `IN` constraint SQLite force-clears `orderByConsumed`
     (`core/src/sqlite/sqlite3.c:161754`), so the `IN` plan must add a sorter that the non-`IN` plan does
     not need. At equal cost the `IN` pushdown therefore **loses**, and
     `WHERE "table" IN (...)` — the headline "multi-table reconciliation" benefit — silently does not
     materialise. The cost tier is what makes it win.

   A plain `WHERE "table" = ?` with a bound-parameter RHS is the one case that does *not* depend on the
   cost tier: the term has no prerequisites and no `IN`, so `whereLoopAddVirtual` makes no further
   `xBestIndex` calls and the single plan is used regardless of cost.

   **The combined `tbl + db_version` plan also needs tuning.** When both constraints are usable, `idxNum`
   is `3` (bit 0 = tbl, bit 1 = db_version) and the cascade currently matches the `db_version` tier
   **first**, so `"table" = ? AND db_version > ?` is costed as an un-pruned `db_version` scan even though
   the tbl bit will confine it to one table's clock. Scale the `db_version` / `site_id` tiers **down** when
   the tbl bit is *also* set (e.g. multiply their cost by a small factor), so the estimate reflects the
   pruning. The absolute numbers are hand-picked guesses; only the *ordering* between tiers matters.

   **This tuning is load-bearing for `IN`, not mere hygiene.** For a plain `"table" = ? AND db_version > ?`
   it is only cosmetic — the single plan is used regardless of cost. But `WHERE "table" IN (...) AND
   db_version > ? ORDER BY db_version, seq` hits the same tie-break trap as bare `IN`: the `IN` plan
   (`idxNum = 3`) and the non-`IN` rival (`idxNum = 2`) **both** match the `db_version` tier and land on the
   same `10.0`, and the force-cleared `orderByConsumed` then adds a sorter to the `IN` plan only — so at
   equal cost the pushdown loses. Scaling the `db_version`/`site_id` tiers down when the tbl bit is set is
   what breaks that tie in the `IN` plan's favour. One caution the other way: resist driving `estimatedRows`
   toward `1`, since that number feeds join-cardinality estimates elsewhere and the existing
   `estimatedRows = 1` on the top tier is already an aggressive fiction.

   **Those two pulls are in tension, so do not assume a factor — verify.** Breaking the tie requires the
   cost reduction to exceed the *sort* cost SQLite adds to the `IN` path, and `rSortCost` scales with
   `estimatedRows`; lowering rows helps win the tie but degrades join estimates. There is no factor that
   is provably right a priori. Treat "`EXPLAIN QUERY PLAN` confirms `WHERE "table" IN (...)` reaches the
   pushdown" as an explicit acceptance test, not an assumption.

   **Better, if the `IN` case matters enough:** opt into `sqlite3_vtab_in()` for the `Tbl` constraint.
   The `orderByConsumed` clearing is in the `else` branch of an `mHandleIn` test
   (`core/src/sqlite/sqlite3.c:161746`), so a constraint the vtab handles via the `IN` iterator keeps
   `ORDER BY` consumption — which removes the sorter, removes the tie-break entirely, and collapses N
   `xFilter` calls (and N re-prepares of the whole union) into one. `sqlite3_vtab_in` /
   `sqlite3_vtab_in_first` / `sqlite3_vtab_in_next` need SQLite ≥ 3.38 (bundled is 3.42) and are present
   in `sqlite3ext.h`, though **not** currently re-exported by `sqlite3_capi/src/capi.rs`, so this needs a
   binding addition plus real `xFilter` work (iterate the value list and prune the union to those tables).
   It is strictly more work than the cost tier; it is also the only option that makes the `IN` win
   robust rather than contingent on hand-tuned estimates.

   Note the snippet above uses `estimated_cost` / `estimated_rows` locals; today each branch writes
   `(*index_info).estimatedCost` directly inside its own `unsafe` block, so this is a (small, welcome)
   refactor rather than a drop-in insertion.

`xFilter` and `changes_union_query` are untouched. `pExtData.tbl_infos` is untouched — the cursor's
`tbl_infos.iter().position(...)` lookup and `slab_rowid` bookkeeping keep working exactly as before.

### Optional follow-up: prune the generated SQL text

The scan is bounded after the change above, but `changes_filter` still calls `db.prepare_v2` on a SQL
string containing **one subquery per CRR table**, on **every** `xFilter` call — the statement is never
cached (`changes_vtab.rs:294`; finalized in `changes_crsr_finalize`). For a database with many CRRs, or
for a `WHERE "table" IN (t1..tN)` that SQLite decomposes into N separate `xFilter` calls, parse cost
becomes the dominant term for small result sets.

Only if measurement shows that matters:

- Encode the `table` constraint's argument position in the high bits of `idxNum`
  (`idx_num |= arg_v_index << 8`) — a single bit cannot carry a position, which the original draft
  overlooked. Un-ignore the `_idx_num` parameter of `crsql_changes_filter`.
- In `changes_filter`, read `args[pos-1].text()` and pass an optional filter to `changes_union_query`
  so it emits only the matching `TableInfo`'s subquery.
- **Keep `tbl = ?` in `idx_str` regardless.** It is load-bearing twice over: it keeps the positional
  `bind_value` loop aligned, and it keeps the vtab's filtering exact.
- If no `TableInfo` matches (a non-CRR name), return early from `changes_filter` leaving
  `pChangesStmt` null — `crsql_changes_eof` then reports EOF, exactly like the existing
  `tbl_infos.len() == 0` path. Do **not** let `changes_union_query` emit `FROM ()`, which fails to
  prepare.
- Caching prepared change statements (keyed by `idx_str`) would address the same cost more broadly and
  is probably the better investment; the two compose well, since a pruned statement is small and
  table-specific.

## Emitting the predicate with TEXT affinity

A bare `tbl = ?` is *not* semantically equivalent to what the pre-patch vtab does, and `omit = 0` does
not fix it. `CAST(tbl AS TEXT) = ?` does, for one token and one bind slot.

### Why bare `tbl = ?` diverges

- `[table]` is declared `TEXT NOT NULL`, so the pre-patch comparison `"table" = ?` — performed by SQLite
  against the vtab column — applies **TEXT affinity** to the bound value: integer `5` becomes `'5'`.
- SQLite does **not** apply affinity to values handed to `xFilter`; there is no `OP_Affinity` before
  `OP_VFilter` (`core/src/sqlite/sqlite3.c:154596`). The raw integer arrives.
- Inside the union, `tbl` is a bare string literal. A literal is not a column reference, so it carries
  **no affinity**, and neither does a subquery column derived from one. So `tbl = ?1` compares integer
  `5` against `'5'` with no conversion — false.

Measured on the bundled shell (`core/dist/sqlite3`, 3.42.0):

```
sqlite> SELECT '5' = 5;                                             -- 0
sqlite> SELECT count(*) FROM (SELECT '5' AS tbl) WHERE tbl = 5;     -- 0
sqlite> CREATE TABLE decl(a TEXT); INSERT INTO decl VALUES ('5');
sqlite> SELECT count(*) FROM decl WHERE a = 5;                      -- 1
```

`omit = 0` cannot rescue this: the arm is pruned *inside* the union, so SQLite's outer re-check — which
would apply TEXT affinity — has no row left to re-admit. `omit = 0` and `omit = 1` yield the identical,
diverging result set. (The divergence is only ever in the *stricter* direction — the pruning predicate
is provably a subset of SQLite's own check — which is why `omit = 0` is free insurance but not a fix.)

### The fix: `CAST(tbl AS TEXT) = ?`

`CAST(x AS TEXT)` has TEXT affinity (unlike a literal), so the comparison acquires TEXT affinity and
the bound value is converted exactly as SQLite would convert it. Verified against the pre-patch
behaviour (a declared-`TEXT` column) across every storage class:

| RHS | pre-patch (`decl.a = ?`) | bare `tbl = ?` | `CAST(tbl AS TEXT) = ?` |
|---|---|---|---|
| `5` vs table `5` | match | **no match** | match |
| `5.0` vs table `5.0` | match | **no match** | match |
| `'items'` vs table `items` | match | match | match |
| `x'35'` vs table `5` | no match | no match | no match |
| `0` vs table `0` | match | **no match** | match |

**Pruning is fully preserved.** After push-down and substitution the term becomes
`CAST('items' AS TEXT) = ?1`, still loop-invariant, so SQLite still hoists it above the arm's cursor
opens. `EXPLAIN` of the realistic union shape with `CAST(tbl AS TEXT) = :t` on 3.42.0 shows the same
guards in the same places as the bare form:

```
  3   Ne    6   64   5   BINARY-8  82    <-- guards arm 'a', before its OpenReads
  4   OpenRead  5  2  0  7
 82   Ne    6  143  34   BINARY-8  82    <-- guards arm 'b', before its OpenReads
 83   OpenRead  1  4  0  7
184   Cast  5   66   0                   <-- evaluated once, in the prologue
```

The EXPLAIN excerpt above shows a single `Cast` for brevity; a real N-arm union emits **one `Cast` per
CRR table** (arm `'a'` casts `'a'`, arm `'b'` casts `'b'`, …), and each of those lands in the run-once
init block after the top-level `Halt`, reached once via `Init`/`Goto`:

```
 47   Halt
 49   String8  0  2  0  'a'
 51   Cast     2  66  0          <-- arm 'a' literal, once per statement
 52   Variable 1  3  0  :t
 53   String8  0  9  0  'b'
 55   Cast     9  66  0          <-- arm 'b' literal, once per statement
 56   Goto     0  1
```

**There is also one `Cast` per output row, and the doc previously denied this.** SQLite's push-down
*copies* a WHERE term into the subquery arms; it does not move it. The original term is retained at the
outer level of the vtab's own generated statement and re-evaluated per row yielded by the co-routine:

```
 28   InitCoroutine 1  0  2
 29     Yield ...                <-- next row from the union
 31     Cast  11  66  0          <-- per row
 32     Ne     3  39  11         <-- per row
 39   Goto  0  29
```

This is not a scaling concern and is not a reason to avoid the `CAST`: those rows are only the ones that
survived arm pruning — i.e. the target table's rows, which is the whole point — and the bare `tbl = ?`
form pays the same per-row `Copy`+`Ne` there anyway, so the `CAST` adds at most one opcode per returned
row. But it is per-row work, so describe it accurately.

Implement it by mapping `CrsqlChangesColumn::Tbl` to `CAST(tbl AS TEXT)` rather than `tbl`. Nothing else
changes: still one `?`, still one `argvIndex`, still aligned with `changes_filter`'s positional
`bind_value` loop.

**One caveat on where to map it.** `get_clock_table_col_name` is shared with the **`ORDER BY`** emission
path, not only the `WHERE` predicate. If you make the substitution there, `ORDER BY [table]` also becomes
`ORDER BY CAST(tbl AS TEXT)`. That is harmless — the cast is identity on a `TEXT` literal, the ordering
stays BINARY, and `ORDER BY` consumption remains valid — but it is a wider blast radius than the fix needs.
If you prefer to leave `ORDER BY` byte-for-byte unchanged, inject the `CAST` only at the point the
`WHERE`-predicate text is built and leave `get_clock_table_col_name` alone for the sort key.

### The remaining parity gap: explicit `COLLATE`

`CAST(tbl AS TEXT) = ?` closes the *affinity* gap. It does **not** close the *collation* gap, and that
gap is the same shape: prune-before-recheck, so `omit = 0` does not help.

`allocateIndexInfo` applies **no** collation filter to constraints — its only collation check is in the
`ORDER BY` loop, not the constraint loop. SQLite therefore hands
`WHERE "table" = 'ITEMS' COLLATE NOCASE` to `xBestIndex` as an ordinary `EQ` constraint, and expects the
vtab to ask about the collation itself; the API docs are explicit that "the collating sequence of
constraints does not matter" only "for most real-world virtual tables". cr-sqlite never calls
`sqlite3_vtab_collation()`. So the generated predicate compares under `BINARY`, prunes the arm, and
returns nothing — while stock cr-sqlite (which applies the constraint itself, under `NOCASE`) returns the
rows.

This is not novel to `table`: the vtab already pushes down `cid`, another `TEXT` column, so the same
divergence is reachable today via `WHERE cid = 'X' COLLATE NOCASE`. But for `"table"` it is *newly*
introduced by this patch, so it belongs in the parity ledger.

**Fix — gate on `BINARY`.** In `constraint_is_usable` (or in `changes_best_index`, where the index is in
hand), accept the `Tbl` constraint only when the comparison collation is `BINARY`, and otherwise fall
back to today's behaviour of letting SQLite apply it:

```rust
// `sqlite3_vtab_collation` is already re-exported by the bindings:
//   sqlite3_capi/src/capi.rs:74 -> `sqlite3_vtab_collation as vtab_collation`
//   sqlite_nostd/src/nostd.rs:19 -> `pub use sqlite3_capi::*;`
let coll = unsafe { CStr::from_ptr(sqlite::vtab_collation(index_info, i)) };
let binary = coll.to_bytes().eq_ignore_ascii_case(b"BINARY");
```

Only a constraint that is both `INDEX_CONSTRAINT_EQ` *and* `BINARY`-collated gets an `argvIndex`. Note
this check needs the `index_info` pointer and the constraint index, which `constraint_is_usable` does not
currently receive — either pass them in or do the check inline in the `changes_best_index` loop. With
this gate plus `CAST(tbl AS TEXT) = ?`, "result sets are unchanged for every query" is an unqualified
claim rather than a nearly-true one.

### Why not the `CASE`/`typeof` form

An earlier draft proposed emulating affinity with
`tbl = CASE WHEN typeof(?) IN ('integer','real') THEN CAST(? AS TEXT) ELSE ? END`. That form is
semantically right but **not implementable as written**: it emits *three* anonymous `?` for a single
constraint, while `changes_best_index` increments `arg_v_index` by one and `changes_filter` binds
positionally —

```rust
for (i, arg) in args.iter().enumerate() {
    stmt.bind_value(i as i32 + 1, *arg)?;   // changes_vtab.rs:295
}
```

— so parameters 2 and 3 would be left unbound (i.e. `NULL`), making the predicate `tbl = NULL` and
returning **zero rows for every query**, silently. Any later constraint's `argvIndex` would collide with
the extra placeholders as well. It could be salvaged by emitting numbered parameters (`?1` repeated)
throughout `changes_best_index`, but `CAST(tbl AS TEXT) = ?` is exact, shorter, and needs no such change.


## Correctness considerations

- **`IN` decomposition.** With `argvIndex` assigned and `sqlite3_vtab_in()` not used, SQLite loops over
  the `IN` values and calls `xFilter` once per value. Each call is a bounded probe. This is free in
  code, but *not* free in prepare cost — see the optional follow-up above.
- **Non-EQ operators on `table`.** `!=`, `LIKE`, `IS`, `IS NULL` stay unusable and are applied by
  SQLite as today. (`LIKE`/`GLOB` on a constant `tbl` would in principle prune arms too, but `omit`
  semantics for `LIKE` are hint-only and `case_sensitive_like` interacts; not worth it.)
- **`pk` / `val` stay post-scan** — but now over one pruned table's clock instead of the whole union,
  which is the real win for pk-targeted reads.
- **Empty / non-CRR table name.** With the required change alone, the union is still built in full and
  every arm is skipped at run time; the statement steps straight to `DONE`, the cursor finalizes, and
  `xEof` reports EOF. No special case needed.
- **Zero CRRs.** Unchanged: `changes_filter` returns early when `tbl_infos` is empty.
- **`ORDER BY` consumption is *not* untouched for `IN`.** When the vtab consumes an `IN` constraint,
  SQLite force-clears `orderByConsumed` (`core/src/sqlite/sqlite3.c:161754`) because IN-value order does
  not imply output order. So `WHERE "table" IN (...) ORDER BY db_version, seq` gains a sorter that it
  does not have today. Results are unchanged and the scan is still bounded, but the "free" in
  "multi-table reconciliation for free" now costs a sort. Plain `=` is unaffected.
- **`db_version`/`site_id` pushdown** is additive and untouched.
- **Writes.** `xUpdate` / `crsql_merge_insert` never consult `idxNum` or `idx_str`; the merge path is
  unaffected.

## Benefits

- **Bounded table-scoped change reads.** `WHERE "table" = ?` costs O(rows in that table) instead of
  O(total change rows). A small table stays cheap regardless of how large the rest of the database gets.
- **Efficient multi-table reconciliation.** `WHERE "table" IN (...)` becomes one bounded probe per table
  — provided the cost tier is added, and accepting an added sorter (see Correctness considerations).
- **Better selective sync and tooling.** Anything that syncs, backfills, reconciles, audits or inspects
  specific tables via `crsql_changes` benefits, increasingly so as the database grows.
- **No cost to anyone else.** Queries that do not constrain `"table"` are planned exactly as before.
  No schema migration, no format change, no ABI change.
- **No result changes at all**, for any table name, given `CAST(tbl AS TEXT) = ?` plus the `BINARY`
  collation gate.
- **Tiny diff.** Roughly ten lines in one function plus a cost tier, with no new state and no new
  invariants to maintain.

## Validation plan

- **Planner:** `EXPLAIN QUERY PLAN` / `EXPLAIN` for `SELECT ... FROM crsql_changes WHERE "table" = ?`.
  The load-bearing assertion is not the `idxNum` in the `SCAN crsql_changes VIRTUAL TABLE INDEX ...`
  line but the presence of the per-arm `Ne ... goto` guard ahead of each arm's `OpenRead` in the
  statement the vtab prepares. A debug hook that logs the generated union SQL is the practical way to
  check this from a test.
- **Scaling benchmark:** read one small table's changes while an *unrelated* table grows. Before, cost
  scales with the unrelated growth; after, it stays flat.
- **Correctness A/B:** assert equality of the full result set with and without the change, across all
  column types (incl. `NULL` and empty string), a delete sentinel, a pk-only (insert-sentinel) row, a
  composite primary key, a table spanning multiple `db_version`s, `"table" = ?` combined with
  `db_version > ?` and `site_id IS NOT ?`, and `"table" IN (...)`. With `CAST(tbl AS TEXT) = ?` this
  equality is unconditional. **Include the affinity cases explicitly** — a CRR table named `5` queried
  with `"table" = 5`, `= 5.0`, `= '5'` and `= x'35'` — as the direct regression test for the predicate
  form; with a bare `tbl = ?` these are exactly the assertions that fail. **Add a collation case too** —
  `WHERE "table" = 'ITEMS' COLLATE NOCASE` against a table named `items` — which fails without the
  `BINARY` gate.
- **Existing tests:** `py/correctness/tests/test_crsql_changes_filters.py::test_table_filter` already
  exercises `=` and `!=` on `[table]` with integer right-hand sides `0..4`, against a table named `item`.
  Those all evaluate false both before and after, so the test passes either way — it will **not** catch a
  bare-`tbl = ?` affinity regression. Add the numerically-named-table cases above, plus a text
  right-hand side that matches a table and one that matches nothing.
- **`IN` plan selection:** assert that `WHERE "table" IN (...)` actually reaches the pushdown. Without
  the cost tier, SQLite prefers the non-`IN` plan (which keeps `orderByConsumed`), and the optimisation
  silently does not happen — a passing correctness suite will not reveal this.
- **Regression:** existing `db_version`/`site_id`-filtered and unfiltered `crsql_changes` queries, and
  `ORDER BY` consumers, are unchanged.

## Scope / non-goals

- Only the `table`-column equality (and `IN`) path. `pk` and `val` predicates remain post-scan; a
  `pk`-index access path would require broader vtab changes.
- No change to `crsql_changes` columns, the clock schema, or the merge/apply path.
- Statement caching for `xFilter` is called out as adjacent, not included.

## Note on reproducing this locally

`cd core && make loadable` currently fails in this tree for an unrelated reason: `sqlite3_capi`
(`core/rs/sqlite-rs-embedded/sqlite3_capi/src/lib.rs:5`) uses `#![feature(concat_idents)]`, which was
removed in Rust 1.90. Building the loadable extension needs a nightly older than that (or migrating to
`${concat(..)}`). The measurements above were therefore taken by running the vtab's generated SQL shape
directly against the bundled SQLite, which is sufficient — the pruning happens entirely inside the
statement `xFilter` prepares.

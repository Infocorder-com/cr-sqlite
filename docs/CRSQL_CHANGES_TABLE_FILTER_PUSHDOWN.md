# Pushing the table filter down in the `crsql_changes` virtual table

**Status:** Proposal / design. Revised after measuring SQLite's actual behaviour against the SQL this
vtab generates, using the bundled `core/dist/sqlite3` shell (3.42.0 — the engine this tree actually
ships) and re-checked on 3.50.2. See
[Which SQLite is actually in play](#which-sqlite-is-actually-in-play).
**Area:** `crsql_changes` virtual table — `xBestIndex` / `xFilter`.
**Compatibility:** No schema change, no wire/format change, no new C ABI. Purely a query-planner
improvement. With the predicate emitted as `CAST(tbl AS TEXT) = ?` **and** the `BINARY` collation gate,
result sets are unchanged for every query. Dropping either one is a silent behaviour change: a bare
`tbl = ?` changes results for numerically-named CRR tables, and skipping the gate changes results for
`WHERE "table" = 'ITEMS' COLLATE NOCASE`. `IN` queries keep identical *results* either way but do change
*plan* — see [Row 4](#row-4-the-right-decision-but-not-for-the-stated-reason).

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

   Why it matters — **measured, and not what an earlier draft of this document claimed.** A probe vtab
   reproducing this exact cost model (both tiers landing on `10.0`), linked against the bundled 3.42.0
   amalgamation, shows that **the `IN` plan already wins without any cost tier**:

   ```
   SELECT * FROM probe WHERE tbl IN ('a','b','c') AND dbv > 1 ORDER BY dbv;
     [xBestIndex] ... tbl=EQ(pushed) dbv(pushed)   -> idxNum=3 cost=10.0 orderByConsumed=1
     [xBestIndex] ... col0(UNUSABLE) dbv(pushed)   -> idxNum=2 cost=10.0 orderByConsumed=1
     [xFilter] idxNum=3 args: a 1
     [xFilter] idxNum=3 args: b 1
     [xFilter] idxNum=3 args: c 1
   ```

   The mechanism: every vtab loop gets `iSortIdx = 0` (`core/src/sqlite/sqlite3.c:162187`) and vtab loops
   carry **no** `nIn` cost multiplier, so with identical `rRun`/`nOut`/`prereq` the two plans are directly
   comparable in `whereLoopFindLesser`. The `IN` plan is inserted first (the "all constraints usable" call
   at `:162004`), so the later non-`IN` template is **discarded outright** — it never reaches the path
   solver where the sorter would have been priced in. The forced `orderByConsumed = 0` therefore costs a
   sort but does not cost the plan.

   So the cost tier is **not** what makes `IN` win. Its only remaining justification is the **join** case:
   `crsql_changes` joined on `"table" = other.col`, where the constraint carries a prerequisite and SQLite
   genuinely compares a constrained plan against an unconstrained one. For a plain
   `WHERE "table" = ?` with a bound-parameter right-hand side there is no rival plan at all —
   `whereLoopAddVirtual` makes no further `xBestIndex` calls ("there is no point in making any further
   calls to xBestIndex() since they will all return the same result", `:162008`) and the single plan is
   used regardless of cost.

   **If you do not want the `IN` pushdown, you must decline it explicitly** — omitting the cost tier does
   not achieve that. Declining is measured to restore today's behaviour exactly:

   ```
   // in the xBestIndex constraint loop, for CrsqlChangesColumn::Tbl:
   if sqlite3_vtab_in(index_info, i, -1) != 0 { continue; }   // leave IN to SQLite
   ```
   ```
   SELECT * FROM probe WHERE tbl IN ('a','b','c') AND dbv > 1 ORDER BY dbv;
     [xBestIndex] ... tbl=IN(DECLINED) dbv(pushed)  -> idxNum=2 cost=10.0 orderByConsumed=1
     [xFilter] idxNum=2 args: 1                      <-- one call, ORDER BY still consumed
   ```

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

**Does the outer term also run per row? Not in the statement this vtab generates.** SQLite's push-down
*copies* a WHERE term into the subquery arms rather than moving it (`sqlite3ExprDup` in
`pushDownWhereTerms`), so in principle the original outer copy could be re-evaluated per row. A
per-row form does reproduce in a *reduced* union — fewer select-list columns, no `LEFT JOIN`s — which
does not flatten, leaving an outer co-routine consumer loop:

```
 28   InitCoroutine 1  0  2
 29     Yield ...                <-- next row
 31     Cast  11  66  0          <-- per row
 32     Ne     3  39  11         <-- per row
 39   Goto  0  29
```

But that is **not the shape this vtab emits**, and the difference is not the literals — it is whether the
subquery flattens. The real arms each carry `<table>__crsql_clock`, the `__crsql_pks` join, the
`crsql_site_id` LEFT JOIN, and a self-join back onto `<table>__crsql_clock` for the insert sentinel
(there is no `__crsql_del` table; the fourth join is `t2` on the clock itself). With the full ten-column
select list, that subquery flattens into the outer query, turning the outer `ORDER BY db_vrsn, seq` into
a *compound* `ORDER BY` — so SQLite plans it as one co-routine plus sorter **per arm**, merged
(`InitCoroutine`/`SorterOpen` per arm, then a `Gosub`/`Yield` merge loop). There is no outer co-routine
consumer loop for a retained term to live in.

Verified by reproducing the exact generated statement for both a 1-arm and a 2-arm union: **every**
`Cast` lands after the top-level `Halt`, i.e. in the run-once init block, and there is no per-row
`Cast`+`Ne`. Pruning happens entirely at the per-arm `Ne … goto` guard ahead of each `OpenRead`.

This shape is stable rather than lucky: `idx_str` only ever contributes `WHERE` terms and an `ORDER BY`
(a user `LIMIT` is applied by SQLite *above* the vtab, never inside the generated SQL), and the
`ORDER BY` clause is never empty — `changes_best_index` appends `ORDER BY db_vrsn, seq ASC` when the
user supplies none. Either way the cost would be immaterial (a per-row copy, where it occurs at all,
adds at most one opcode per *returned* row over bare `tbl = ?`, and returned rows are only the target
table's), but the accurate description for the vtab's own SQL is "once per statement, in the init
block," not "per row."

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
let coll = unsafe { CStr::from_ptr(sqlite::vtab_collation(index_info, i)) };
let binary = coll.to_bytes().eq_ignore_ascii_case(b"BINARY");
```

**This is not as cheap as it looks, and an earlier draft of this document understated it.**
`sqlite3_vtab_collation` appears in `sqlite3_capi/src/capi.rs:74` only as a raw alias inside the private
`mod aliased`, gated on `#[cfg(feature = "static")]`. There is no callable `pub fn vtab_collation(..)`
wrapper, and the loadable build dispatches through `invoke_sqlite!` against a **private**
`static mut SQLITE3_API` (`capi.rs:125`) that a downstream crate cannot reach. Adding the wrapper means
editing `core/rs/sqlite-rs-embedded` — which is a **git submodule pointing at
`vlcn-io/sqlite-rs-embedded`**, the unmaintained upstream. So this costs a submodule fork or vendoring,
not four lines. The same is true of `sqlite3_vtab_in`, which is not even aliased. Price both accordingly
in the ledger below.

Only a constraint that is both `INDEX_CONSTRAINT_EQ` *and* `BINARY`-collated gets an `argvIndex`. Note
this check needs the `index_info` pointer and the constraint index, which `constraint_is_usable` does not
currently receive — either pass them in or do the check inline in the `changes_best_index` loop. With
this gate plus `CAST(tbl AS TEXT) = ?`, "result sets are unchanged for every query" is an unqualified
claim rather than a nearly-true one.

`sqlite3_vtab_collation` returns the *effective comparison* collation, so a plain `"table" = ?` (no
explicit `COLLATE`) reports `BINARY` and is accepted — the gate keeps the scan win for the common case
and only sheds the pushdown for an explicit non-`BINARY` comparison. This relies on the `[table]` column
being declared with `BINARY` collation, which it is (plain `TEXT NOT NULL`). If it were ever redeclared
`COLLATE NOCASE`, a bare `"table" = ?` would report `NOCASE` and the gate would (correctly) fall back to
post-scan — losing the pushdown but never correctness. Worth keeping in mind if the vtab's column
declarations change.

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
- **`IN` plan change:** `WHERE "table" IN (...)` switches from one `xFilter` call to one **per listed
  table**, plus a sort — measured, and it happens with or without the cost tier. Results are unchanged,
  so a correctness suite will not show it. If the three `IN` call sites matter, time them before and
  after rather than assuming parity.
- **Regression:** existing `db_version`/`site_id`-filtered and unfiltered `crsql_changes` queries, and
  `ORDER BY` consumers, are unchanged.

## Decision ledger

This fork optimises for [Infocorder](https://infocorder.com)'s needs first; where a change serves every
consumer at no meaningful cost, that is preferred. Costs below are corrected against two things that
turned out to matter: `sqlite3_vtab_*` helpers are **not** reachable without forking a submodule, and
the `IN` plan wins **without** the cost tier.

**Usage, verified in the consuming app:** no `crsql_changes` reader joins another table on `"table"`.
Three sites use `"table" IN (...)`, each already dominated by a pushed-down `db_version` constraint
(`db_version > ?`; `db_version = ? AND seq > ?` with a `LIMIT`; `db_version > ? AND site_id = ?`). The
hot paths are `"table" = ?` and `"table" = ? AND pk IN (...)`.

| # | Choice | Real cost | Call |
|---|---|---|---|
| 1 | `CAST(tbl AS TEXT) = ?` instead of `tbl = ?` | One token; `Cast` opcodes land in the run-once init block, zero per-row cost (measured) | **Take** — free generality |
| 2 | `omit = 0` on the `Tbl` constraint | One redundant comparison per *returned* row | **Take** — free generality |
| 3 | `BINARY` collation gate | A `vtab_collation` wrapper in `sqlite-rs-embedded` — cheap **now that a fork of that repo already exists** (see below) | **Reconsider — take it** |
| 4 | `idxNum` tbl bit + cost tier | A cascade branch plus estimate tuning | **Drop** — no join sites, and it is *not* what makes `IN` win |
| 5 | `sqlite3_vtab_in()` full handling | Wrapper is now cheap; the real `xFilter` work is not | **Drop** |
| 6 | Prune the union SQL / cache statements | Moderate; new `xFilter` paths | **Measure first** |

Rows 1 and 2 are still the clean "costs nothing, helps everyone" cases and should ship.

### Row 4: the right decision, but not for the stated reason

Dropping the cost tier is correct — there are no join sites. But it does **not** leave the three `IN`
sites at today's behaviour. As measured above, the `IN` plan wins on its own: those queries will issue
**one `xFilter` per listed table** instead of one, each re-preparing the full union SQL (whose size is
O(number of CRRs)), plus a sort, since the vtab's `ORDER BY` consumption is force-cleared for `IN`.

Results are identical either way. The performance question is genuinely open: each probe is now pruned
to one clock *and* still `db_version`-bounded, which is cheaper per probe, against N re-prepares of a
large statement and a sorter. With a small N and few CRRs this is likely a wash or a win; it degrades as
the CRR count grows, because prepare cost scales with it and nothing in items 1–2 shrinks the SQL text.

Three ways to land this, in order of preference given the usage above:

1. **Accept it.** Ship items 1–2, let the `IN` sites take the N-probe plan. None is a hot path and all
   are already `db_version`-bounded. Cost: zero. Requires accepting a behaviour change, so state it as
   one rather than as "no regression", and spot-check the three sites once.
2. **Decline `IN` explicitly** via `sqlite3_vtab_in(p, i, -1)`. Measured to restore today's plan exactly
   (single `xFilter`, `ORDER BY` still consumed). Deterministic, but costs the submodule fork.
3. **Add item 6** (prune the union SQL), which removes the N-re-prepare objection and makes the `IN`
   plan straightforwardly good. Most work, best end state.

Option 1 is the right first move. Revisit only if a spot-check shows one of the three sites regressing.

### Row 3: the collation gate is cheap again

This was downgraded to "drop" on the grounds that `sqlite3_vtab_collation` has no callable wrapper and
adding one meant forking a submodule owned by the unmaintained `vlcn-io` upstream. That premise no
longer holds: a fork of `superfly/sqlite-rs-embedded` already exists under this org, and the submodule
is being repointed at it regardless (see [Note on reproducing this locally](#note-on-reproducing-this-locally)).
Adding a wrapper alongside the existing `pub fn vtab_distinct(index_info: *mut index_info)` is then the
four lines it originally looked like:

```rust
pub fn vtab_collation(idx_info: *mut index_info, i: c_int) -> *const c_char {
    unsafe { invoke_sqlite!(vtab_collation, idx_info, i) }
}
```

`sqlite3_vtab_collation` is already in the `#[cfg(feature = "static")]` alias list
(`sqlite3_capi/src/capi.rs:78`) and in `sqlite3_api_routines` for the loadable path, so no bindgen or
header change is needed. That restores it to the "costs nothing, helps everyone" tier — **take it**, and
the compatibility claim at the top of this document becomes unqualified.

## Scope / non-goals

- Only the `table`-column equality (and `IN`) path. `pk` and `val` predicates remain post-scan; a
  `pk`-index access path would require broader vtab changes.
- No change to `crsql_changes` columns, the clock schema, or the merge/apply path.
- Statement caching for `xFilter` is called out as adjacent, not included.

## Which SQLite is actually in play

`core/src/sqlite/sqlite3.c` (`SQLITE_VERSION 3.42.0`) is the **only** SQLite implementation in this tree.
It is what `core/dist/sqlite3` is built from and what both the loadable extension and the static bundle
compile against, so the measurements in this document are on the shipping engine rather than a proxy.

The `3.45.0` at `core/rs/sqlite-rs-embedded/sqlite3_capi/deps/sqlite3.h` is **not** a linked library —
that directory holds headers only, `sqlite3_capi/wrapper.h` includes just `deps/sqlite3ext.h`, and the
crate compiles no amalgamation. The version there only shapes the Rust FFI *declarations* bindgen emits.

It is worth knowing for an unrelated reason. For a loadable extension, `sqlite3ext.h` defines the
`sqlite3_api_routines` dispatch struct, so generating bindings from a **newer** header than the engine
actually in use means a function added after the engine's version would be dispatched past the end of the
host's struct. This is benign today — the struct is append-only and nothing here calls a post-3.42
function — and both APIs this document suggests are comfortably older: `sqlite3_vtab_collation` (3.37)
and `sqlite3_vtab_in` (3.38). Check that floor before reaching for anything newer.

## Note on reproducing this locally

`cd core && make loadable` fails **in the analysis environment used for this document**, and the
attribution matters. `sqlite3_capi/src/lib.rs` carries `#![feature(concat_idents)]`, removed in Rust
1.90 — a hard error on any toolchain. The crates pin `nightly-2023-10-05` in `rust-toolchain.toml`, but
that machine has no `rustup`, so the pins are inert and the system stable compiler (1.95.0) is used.
The production static build is **not** broken this way: it pins a nightly explicitly
(`SB_CRSQLITE_NIGHTLY` in `build_crsqlite_static.sh`) and `sed`-deletes the `concat_idents` gate. The
upside below is therefore not "unbreak the build" — it is **dropping the nightly requirement and the
`sed` workarounds**, which is worthwhile on its own but separable from this pushdown.

**Sequence it *after* the pushdown, not before.** The pushdown builds fine on the existing pinned
nightly and has no dependency on this port: the `vtab_collation` wrapper needs only the submodule pin
(already landed), and `stable_trap` is not linked unless `core/rs/bundle/Cargo.toml` adds it. Porting
first would entangle a three-platform toolchain migration — panic handling, `eh_personality`, linking,
with Windows/MinGW the long pole — with a vtab logic change in one rebuild, making a cross-platform
failure hard to bisect.

The fix exists upstream, and is proven — but it is **not** on `superfly/cr-sqlite`'s `main`, which still
pins the same `aba5628` submodule commit we do, still points `.gitmodules` at `vlcn-io`, and still
carries every nightly gate. The stable-rust work lives on the unmerged branch
`gorbak/replace-submodule-with-subrepo` (`f347c8d9`, 2026-01-27), paired with
`superfly/sqlite-rs-embedded` `1872f70`. Verified locally: with that submodule, `sqlite3_capi`,
`sqlite3_allocator`, `sqlite_nostd` and `crsql_core` all build on stable 1.95.

The recipe, from `f347c8d9`:

| File | Change |
|---|---|
| `core/rs/core/src/lib.rs` | delete `#![feature(vec_into_raw_parts)]` (stable since 1.93) |
| `core/rs/fractindex-core/src/lib.rs` | delete `#![feature(core_intrinsics)]` — a stale gate, the crate uses no intrinsics |
| `core/rs/bundle/src/lib.rs` | delete both gates; `core::intrinsics::abort()` → `stable_trap::abort()` (×3) |
| `core/rs/bundle/src/lib.rs` | replace `#[lang = "eh_personality"]` with `#[no_mangle] extern "C" fn rust_eh_personality() {}`, plus `_rust_eh_personality` for `target_arch = "arm"` |
| `core/rs/bundle/Cargo.toml` | add `stable_trap = { path = "../sqlite-rs-embedded/stable_trap" }` |
| 5 × `rust-toolchain.toml` | repin off `nightly-2023-10-05` |

The `eh_personality` step is the non-obvious one: rather than declaring a lang item (nightly-only), it
defines the symbol the linker asks for directly.

Until that lands, the measurements in this document were taken by running the vtab's generated SQL
directly against the bundled engine, which is sufficient — the pruning happens entirely inside the
statement `xFilter` prepares.

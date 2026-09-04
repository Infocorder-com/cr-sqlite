# Pushing the table filter down in the `crsql_changes` virtual table

**Status:** Proposal / design. Revised after measuring SQLite's actual behaviour against the SQL this
vtab generates (bundled SQLite 3.42.0).
**Area:** `crsql_changes` virtual table — `xBestIndex` / `xFilter`.
**Compatibility:** No schema change, no wire/format change, no new C ABI. Purely a query-planner
improvement; existing queries are unaffected.

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
   comparison per *returned* row. It does **not**, however, buy exact parity with the pre-patch vtab in
   every case: see [Why `omit = 0`](#why-omit--0) for the one shape (a numerically-named CRR table) where
   the pushdown itself diverges regardless of `omit`.

3. **Cost model** — recording a `Tbl` bit in `idxNum` is *necessary but not sufficient*; the cost
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

   Why it matters: when `crsql_changes` appears in a join and the right-hand side of `"table" = x`
   depends on another table, SQLite calls `xBestIndex` once with the constraint usable and once without;
   with two identical `2147483647.0` costs there is nothing to prefer the pushdown, and SQLite may pick
   the full scan. A distinctly lower tbl-only tier fixes that. (A plain literal `WHERE "table" = ?` with a
   bound param pushes down regardless of cost — the cost tier only decides the *join* case.)

   **The combined `tbl + db_version` plan also needs tuning.** When both constraints are usable, `idxNum`
   is `3` (bit 0 = tbl, bit 1 = db_version) and the cascade currently matches the `db_version` tier
   **first**, so `"table" = ? AND db_version > ?` is costed as an un-pruned `db_version` scan even though
   the tbl bit will confine it to one table's clock. Scale the `db_version` / `site_id` tiers **down** when
   the tbl bit is *also* set (e.g. multiply their cost by a small factor), so the estimate reflects the
   pruning. The absolute numbers are hand-picked guesses; only the *ordering* between tiers matters.

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

## Why `omit = 0` (and the one shape where the pushdown still diverges)

Set `omit = 0` for the `table` constraint, so SQLite keeps applying `"table" = ?` itself *in addition to*
the vtab's pushed-down pruning. This is the safe default — never worse than `omit = 1`. But be precise
about what it does and does **not** buy, because an earlier draft of this doc overclaimed here.

**It does not buy exact parity with the pre-patch vtab for a numerically-named table.** The divergence
below is *intrinsic to the pushdown itself* and is present under `omit = 0` and `omit = 1` **alike** —
`omit = 0` does not fix it:

- `[table]` is declared `TEXT NOT NULL`, so the pre-patch comparison `"table" = ?` applies **TEXT
  affinity** to the bound value: integer `5` is converted to `'5'` before comparison.
- SQLite does **not** apply affinity to values handed to `xFilter` — there is no `OP_Affinity` before
  `OP_VFilter` (`core/src/sqlite/sqlite3.c:154596`). The raw value arrives.
- Inside the union, `tbl` is a bare string literal, an expression with affinity NONE. So the pushed-down
  `tbl = ?1` compares integer `5` against `'5'` with **no** conversion — false — and the arm for a table
  literally named `5` is pruned *before it emits a row*.

So `SELECT ... FROM crsql_changes WHERE "table" = 5` against a table named `5` returns its rows in the
pre-patch vtab and returns **none** after the pushdown. `omit = 0` does not rescue this: the row was
already dropped inside the union, so SQLite's outer `"table" = 5` re-check (which *would* apply TEXT
affinity) has nothing left to re-admit. `omit = 0` and `omit = 1` yield the **identical**, diverging
result set in this case. The shape is not purely hypothetical —
`py/correctness/tests/test_crsql_changes_filters.py:75` probes `[table] = 0..4` — so a consumer that
relies on numeric right-hand sides matching numerically-named CRR tables through `crsql_changes` is a
poor fit for this patch as written.

**What `omit = 0` does buy:** it is strictly the safer of the two, so there is no reason to prefer
`omit = 1`. Wherever the pushdown *does* return a row, SQLite still re-applies `"table" = ?` with correct
TEXT affinity and collation as a belt-and-suspenders check — so any hypothetical case where the vtab's
pruning were too *loose* is caught by SQLite rather than leaking. The cost is one redundant comparison per
returned row, unmeasurable next to the page reads the pushdown saves.

**For every realistic (identifier) table name there is no divergence at all.** `'items' = 5` is false
both ways; `'items' = 'items'` is true both ways. The numeric-name case above is the *only* result-level
difference, and only for callers who name a CRR table with digits. If exact parity for such names is ever
required, apply TEXT affinity inside the union predicate
(`tbl = CASE WHEN typeof(?) IN ('integer','real') THEN CAST(? AS TEXT) ELSE ? END`) — that fixes the
divergence at its source, independent of `omit`, at the cost of three bind slots and readability.

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
- **`ORDER BY` consumption and `db_version`/`site_id` pushdown** are additive and untouched.
- **Writes.** `xUpdate` / `crsql_merge_insert` never consult `idxNum` or `idx_str`; the merge path is
  unaffected.

## Benefits

- **Bounded table-scoped change reads.** `WHERE "table" = ?` costs O(rows in that table) instead of
  O(total change rows). A small table stays cheap regardless of how large the rest of the database gets.
- **Efficient multi-table reconciliation.** `WHERE "table" IN (...)` becomes one bounded probe per table.
- **Better selective sync and tooling.** Anything that syncs, backfills, reconciles, audits or inspects
  specific tables via `crsql_changes` benefits, increasingly so as the database grows.
- **No cost to anyone else.** Queries that do not constrain `"table"` are planned exactly as before.
  No schema migration, no format change, no ABI change.
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
  `db_version > ?` and `site_id IS NOT ?`, and `"table" IN (...)`. For **identifier** table names this
  equality holds unconditionally. The one documented exception is a **numerically-named** CRR table with a
  numeric right-hand side (see [Why `omit = 0`](#why-omit--0)): the pushdown diverges there regardless of
  `omit`, so either exclude that shape from the A/B or assert the *known* diverging result for it — do not
  expect equality.
- **Existing tests:** `py/correctness/tests/test_crsql_changes_filters.py::test_table_filter` already
  exercises `=` and `!=` on `[table]` (with integer right-hand sides). If it asserts that a numeric
  `[table] = n` matches a numerically-named table, that assertion changes under the pushdown — update it
  to the new (pruned) result rather than treating the change as a regression. Add a case with a text
  right-hand side that actually matches a table, and one that matches nothing.
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

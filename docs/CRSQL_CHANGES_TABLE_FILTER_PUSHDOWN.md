# Pushing the table filter down in the `crsql_changes` virtual table

**Status:** Proposal / design.
**Area:** `crsql_changes` virtual table — `xBestIndex` / `xFilter`.
**Compatibility:** No schema change, no wire/format change, no new C ABI. Purely a query-planner
improvement; existing queries are unaffected.

## Summary

`crsql_changes` is a virtual table that presents every row of every CRR table's clock as a uniform
change-set. It is implemented as a `UNION ALL` over one subquery per CRR table (each reading that
table's `<table>__crsql_clock`). Today, a `WHERE "table" = ?` (or `WHERE "table" IN (...)`) constraint
is **not** pushed down: the planner is told the constraint is unusable, so SQLite scans **every**
table's clock and applies the `"table"` filter afterward. Reading the changes for a single table
therefore costs O(total change rows across *all* tables), not O(rows in the target table).

This proposal makes an equality constraint on the `"table"` column **usable** in `xBestIndex` and prunes
the `UNION ALL` to the matching table's subquery in `xFilter`. The result: a table-scoped change read
becomes bounded by that table's own clock, independent of how large the rest of the database is. The
emitted rows are byte-identical to today.

## The current limitation

The vtab exposes columns `table`, `pk`, `cid`, `val`, `col_version`, `db_version`, `site_id`, `cl`, `seq`
(and `ts`). In `xBestIndex`, `constraint_is_usable` explicitly refuses the `table`, `pk`, and `value`
columns:

```rust
// constraint_is_usable (core/rs/core/src/changes_vtab.rs)
!matches!(col, CrsqlChangesColumn::Tbl | CrsqlChangesColumn::Pk | CrsqlChangesColumn::Cval)
```

So for `SELECT ... FROM crsql_changes WHERE "table" = ?`, the `table = ?` constraint is skipped: it is
never assigned an `argvIndex`, never contributes to `idxNum`/`idxStr`, and the plan falls into the
"no usable constraints" branch (`estimatedCost = i64::MAX`). SQLite then does a full scan of the vtab
and filters `"table" = ?` itself. `EXPLAIN QUERY PLAN` shows `SCAN crsql_changes VIRTUAL TABLE INDEX 0`.

`xFilter` builds the full union unconditionally:

```rust
// changes_filter -> changes_union_query (core/rs/core/src/changes_vtab_read.rs)
// one subquery per TableInfo in pExtData, joined by UNION ALL, wrapped as
//   SELECT ... FROM ( <union of all tables> ) <idx_str>
```

`idx_str` carries the pushed-down `db_version` / `site_id` predicates and the `ORDER BY`, but it filters
the *outer* query — it does **not** reduce which clocks are scanned.

**Consequence.** The cost of any table-scoped change read grows with the *whole database's* change
volume. A tiny table's changes cannot be read cheaply once the database accumulates changes in *other*
tables. This is O(total-clock) where it should be O(rows-in-table). It penalizes exactly the workloads
that read `crsql_changes` for a specific table: targeted/selective sync, per-table backfill or
reconciliation, per-table auditing, and any tooling that inspects one table's history. It also makes a
`WHERE "table" IN (t1, ..., tN)` reconciliation of N tables cost N × O(total-clock).

## The change

Make the `table`-column **equality** constraint usable and honor it by pruning the union:

1. **`constraint_is_usable`** — allow `CrsqlChangesColumn::Tbl` **only** for `INDEX_CONSTRAINT_EQ`
   (SQLite also reports `IN` as `EQ` here). Keep `Pk` and `Cval` unusable (they are genuinely
   post-scan predicates and not the subject of this change).

2. **`changes_best_index`** — for that constraint, assign an `argvIndex`, set `omit = 1`, and record its
   argument position in a new `idxNum` bit. Continue to emit the `table = ?` text into the wrapper
   predicate as well (belt-and-suspenders: the vtab still applies the filter, so correctness never
   depends solely on the pruning). Leave the existing `db_version`/`site_id` pushdowns and the
   `ORDER BY` consumption untouched.

3. **`changes_filter` / `changes_union_query`** — decode the `idxNum` bit, read the bound table name
   from `argv`, and build the union over **only** the matching `TableInfo`. If no table matches (e.g. a
   non-CRR name), short-circuit to an empty result. **Do not mutate** the `tbl_infos` vector held in
   `pExtData` — the cursor advance and rowid logic still index the full vector by name; only the SQL
   string built for this scan is pruned.

Because SQLite decomposes `"table" IN (a, b, c)` into repeated `xFilter` calls (one per value) when the
constraint is usable + omitted and the vtab does not opt into `sqlite3_vtab_in()`, an `IN` over N tables
automatically becomes N bounded per-table probes — no additional code.

### Why the output is unchanged

Pruning changes only *which subqueries appear in the union*, not how any row is produced. Each per-table
subquery (`crsql_changes_query_for_table`) and the cursor's column materialization (packed `pk` via
`crsql_pack_columns`, `val` from the base table, `site_id` resolved from the site-id table, `cl` from the
sentinel self-join, `seq`, `ts`) are identical. For any `WHERE "table" = ?` query, the result set is
exactly the same rows either way — the pushdown only avoids reading clocks that could not have
contributed. This is verifiable as a byte-for-byte A/B: the same query with and without the pushdown
must return identical rows.

## Correctness considerations

- **Exactness with `omit = 1`.** With `omit`, SQLite stops applying `table = ?` itself; the vtab must
  enforce it. Pruning on exact table-name equality does so, and retaining `table = ?` in the wrapper is
  a redundant guarantee.
- **Existing pushdowns preserved.** The `db_version` / `site_id` constraint handling and the
  `ORDER BY db_version, seq` consumption are additive and unchanged.
- **`pk` / `value` stay post-scan.** A `pk IN (...)` filter remains a post-scan predicate — but now over
  a single pruned table's clock rather than the whole union, which is the intended win for pk-targeted
  reads.
- **Vector integrity.** `pExtData.tbl_infos` must remain the full set; only the generated union SQL is
  narrowed. Mutating the shared vector would break cursor/rowid bookkeeping.
- **Non-CRR / unknown table name.** Must yield an empty result (no matching subquery), not a malformed
  `FROM ()`.

## Benefits to cr-sqlite users

- **Bounded table-scoped change reads.** `SELECT ... FROM crsql_changes WHERE "table" = ?` costs
  O(rows-in-that-table) instead of O(total change rows). A small table stays cheap to read no matter how
  large the rest of the database grows.
- **Efficient multi-table reconciliation.** `WHERE "table" IN (...)` becomes one bounded probe per
  table, for free.
- **Better scaling for selective sync and tooling.** Any application that syncs, backfills, reconciles,
  audits, or inspects specific tables via `crsql_changes` — rather than draining the whole change
  stream — benefits directly, and increasingly so as the database grows.
- **No cost to anyone else.** Queries that do not constrain `"table"` are planned exactly as before; the
  change is opt-in by query shape. No schema migration, no format change, no ABI change.

## Validation plan

- **Planner:** `EXPLAIN QUERY PLAN` for `SELECT ... FROM crsql_changes WHERE "table" = ?` before/after.
- **Scaling benchmark:** measure the read cost of one small table's changes while an *unrelated* table
  grows; before, the cost scales with the unrelated growth; after, it stays flat (bounded by the target
  table).
- **Correctness A/B:** for a populated table, assert byte-for-byte equality of the full result set with
  vs. without the pushdown, across: all column types (incl. `NULL` and empty string), a deleted row
  (delete sentinel), a composite primary key, and a table spanning multiple `db_version`s; plus paging
  boundaries (exactly-`LIMIT`, empty table) if a `LIMIT`/cursor is used.
- **Regression:** the existing `db_version`/`site_id`-filtered and unfiltered `crsql_changes` queries,
  and `ORDER BY` consumers, are unchanged.

## Scope / non-goals

- Only the `table`-column **equality** (and `IN`) path is pushed down. `pk`- and `value`-column
  predicates remain post-scan (a `pk`-index access path would require broader vtab changes and is out of
  scope here).
- No change to `crsql_changes` columns, to the clock schema, or to the merge/apply path.

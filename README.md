# cr-sqlite - Convergent, Replicated, SQLite

[![c-tests](https://github.com/vlcn-io/cr-sqlite/actions/workflows/c-tests.yaml/badge.svg)](https://github.com/vlcn-io/cr-sqlite/actions/workflows/c-tests.yaml)
[![c-valgrind](https://github.com/vlcn-io/cr-sqlite/actions/workflows/c-valgrind.yaml/badge.svg)](https://github.com/vlcn-io/cr-sqlite/actions/workflows/c-valgrind.yaml)
[![py-tests](https://github.com/vlcn-io/cr-sqlite/actions/workflows/py-tests.yaml/badge.svg)](https://github.com/vlcn-io/cr-sqlite/actions/workflows/py-tests.yaml)
[![rs-tests](https://github.com/vlcn-io/cr-sqlite/actions/workflows/rs-tests.yml/badge.svg)](https://github.com/vlcn-io/cr-sqlite/actions/workflows/rs-tests.yml)

A component of the [vulcan](https://vlcn.io) project.

[![](https://dcbadge.vercel.app/api/server/AtdVY6zDW3)](https://discord.gg/AtdVY6zDW3)

# Infocorder fork of superfly fork of vlcn-io's cr-sqlite repo

`vlcn-io` created `cr-sqlite` (https://github.com/vlcn-io/cr-sqlite) but hasn't maintained it, so `superfly` forked it and has improved on it (https://github.com/superfly/cr-sqlite).

> ⚠️ **This repository is NOT a drop-in replacement for the original `vlcn-io` project or the `superfly` fork.**

This repository is an **extension AND SPECIALIZATION** of the `superfly` fork that addresses two specific changes we wanted for [Infocorder](https://infocorder.com)'s hub-and-spoke SQLite sync that do **not** generalize to all use cases. If you just want vanilla cr-sqlite, use `vlcn-io/cr-sqlite` or `superfly/cr-sqlite` instead — this fork deliberately diverges from both. The two changes are:

### 1. Finalize-on-close: release OS file descriptors when a connection closes (already shipped)

Stock cr-sqlite keeps its internal prepared statements (in `crsql_ExtData`) alive for the life of a connection and never finalizes them on close, so — unless the host explicitly calls `SELECT crsql_finalize()` first — `sqlite3_close` / `sqlite3_close_v2` leaves the connection a "zombie" and the OS file handles (`*.db`, `*-wal`, `*-shm`) leak until the process exits. This fork makes cr-sqlite finalize its *own* internal statements as part of connection teardown, so the handles are released without the host having to call `crsql_finalize()` first. It also adds `crsql_build_id()` as a build/provenance marker. See [`docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md`](docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md).

- **Benefit:** long-lived processes that open and close *many* connections (e.g. a server holding one SQLite database per project/tenant) no longer leak file descriptors, and no app-side `crsql_finalize()` dance is required on every teardown path.
- **Cost / who should be careful:** it changes connection-teardown behavior (cr-sqlite now hooks close to finalize its cached statements). Hosts that deliberately rely on stock close/finalize semantics, or that already call `crsql_finalize()` themselves, should confirm the new path suits them. And, like any fork change, it makes tracking upstream exactly harder.

### 2. `crsql_changes` table-filter pushdown (about to be made)

Stock cr-sqlite's `crsql_changes` virtual table treats `WHERE "table" = ?` as a *post-scan* filter: it scans the entire `UNION ALL` over **every** table's change-clock and discards non-matching rows afterward, so a table-scoped read costs O(total change rows across the whole database), independent of the target table's size. This fork patches the vtab's `xBestIndex` so a `"table" = ?` (or `"table" IN (...)`) constraint is pushed into the union and prunes the scan to the one matching `<table>__crsql_clock`. See [`docs/CRSQL_CHANGES_TABLE_FILTER_PUSHDOWN.md`](docs/CRSQL_CHANGES_TABLE_FILTER_PUSHDOWN.md).

- **Benefit:** bounded table-scoped change reads — O(rows in that table), not O(total). Anything that syncs, backfills, reconciles, audits or inspects *specific* tables via `crsql_changes` stays cheap as the rest of the database grows.
- **Cost / who should be careful:** the pushed-down predicate is compared *without* TEXT affinity, so the result is **not** byte-identical to stock cr-sqlite for a CRR table whose name is purely numeric — `WHERE "table" = 5` against a table literally named `5` returns rows in stock cr-sqlite but **none** after the pushdown (an intrinsic quirk, present regardless of the `omit` flag). It also needs a planner cost-model tuning. All of Infocorder's table names are ordinary identifiers, so this never bites us; a consumer that names CRR tables with digits and reads them through `crsql_changes` would be affected.

### Especially poor fits for this fork

- You want a **drop-in, binary-compatible** replacement for stock cr-sqlite, or need to **track `vlcn-io` / `superfly` upstream exactly** — this fork intentionally diverges and is maintained for Infocorder's needs, not general parity.
- You have (or might create) **CRR tables with purely-numeric names** and query them by `"table" = <n>` through `crsql_changes` — change 2 alters those results.
- You depend on stock cr-sqlite's **connection-close / finalize semantics** — change 1 alters those.

For everyone else, this fork is effectively a superset of `superfly`'s behavior: unfiltered and identifier-named-table `crsql_changes` reads, the on-disk format, the wire/changeset format, and the public API are all unchanged.

## How to merge in commits from superfly

I maintain three repos in `~/Git`:

* `~/Git/cr-sqlite`: the original vlcn.io project
* `~/Git/superfly_cr-sqlite`: superfly's fork of vlcn.io's repo
* `~/Git/infocorder_cr-sqlite`: Infocorder.com's fork of superfly's fork

In my fork (`~/Git/infocorder_cr-sqlite`), I added:

```
git remote add superfly_cr-sqlite ../superfly_cr-sqlite
```

Confirm with:
```
~/Git/infocorder_cr-sqlite$ git remote -v
origin	git@github.com:Infocorder-com/cr-sqlite.git (fetch)
origin	git@github.com:Infocorder-com/cr-sqlite.git (push)
superfly_cr-sqlite	../superfly_cr-sqlite (fetch)
superfly_cr-sqlite	../superfly_cr-sqlite (push)
```

To update, I:

* `cd ~/Git/superfly_cr-sqlite`
* `git pull origin main`
* `cd ~/Git/infocorder_cr-sqlite`
* `git fetch superfly_cr-sqlite`
* `git merge superfly_cr-sqlite/main`
* `git push origin main`

# Examples

Example applications using cr-sqlite to sync state.

- Vite starter - [Example](https://vite-starter2.fly.dev/) | [Repository](https://github.com/vlcn-io/vite-starter)
- TodoMVC - [Example](https://vlcn-live-examples.fly.dev/) | [Repository](https://github.com/vlcn-io/live-examples)
- [Svelte Store](https://github.com/Azarattum/CRStore)
- [Tutorials](https://vlcn.io/docs/cr-sqlite/networking/whole-crr-sync)
- [WIP Local-First Presentation Editor](https://github.com/tantaman/strut)
- Basic setup & sync via an [Observable Notebook](https://observablehq.com/@tantaman/cr-sqlite-basic-setup)

# "It's like Git, for your data."

CR-SQLite is a [run-time loadable extension](https://www.sqlite.org/loadext.html) for [SQLite](https://www.sqlite.org/index.html) and [libSQL](https://github.com/libsql/libsql). It allows merging different SQLite databases together that have taken independent writes.

In other words, you can write to your SQLite database while offline. I can write to mine while offline. We can then both come online and merge our databases together, without conflict.

**In technical terms:** cr-sqlite adds multi-master replication and partition tolerance to SQLite via conflict free replicated data types ([CRDTs](https://en.wikipedia.org/wiki/Conflict-free_replicated_data_type)) and/or causally ordered event logs.

# When is this useful?

1. Syncing data between devices
2. Implementing realtime collaboration
3. Offline editing
4. Being resilient to network conditions
5. Enabling instantaneous interactions

All of the above involve a merging of independent edits problem. If your database can handle this for you, you don't need custom code in your application to handle those 5 cases.

Discussions of these problems in the application space:

- [Meta Muse](https://museapp.com/podcast/56-sync/)
- [FB Messenger re-write](https://softwareengineeringdaily.com/2020/03/31/facebook-messenger-engineering-with-mohsen-agsen/)

# Sponsors

Companies:
<a href="https://turso.tech"><img src="https://images.ctfassets.net/8fv5t5my8687/01j7yaLj77zqmYK62Y49g7/aee841e7bd176864aa5388448db0f8ef/iku-turquoise.svg" width="64" /></a> <a href="https://fly.io"><img src="https://fly.io/static/images/brand/brandmark.svg" height="64" /></a> <a href="https://reflect.app/"><img src="https://reflect.app/_next/image?url=%2Fsite%2Ficons%2F1024x1024.png&w=64&q=100" /></a><a href="https://expo.dev"><img src="https://avatars.githubusercontent.com/u/12504344?s=200&v=4" width="64" /></a> <a href="https://electric-sql.com"><img width="108" alt="Screenshot 2023-11-16 at 8 29 27 AM" src="https://github.com/vlcn-io/cr-sqlite/assets/1009003/5c0c8ab3-005a-4b03-ba0a-de7ed213e26d"></a>

Individuals:
[robinvasan](https://github.com/robinvasan) | [iansinnott](https://github.com/iansinnott) | [davefowler](https://github.com/davefowler) | [barbalex](https://github.com/barbalex) | [MohannadNaj](https://github.com/MohannadNaj)

# Perf

Perf data: https://github.com/vlcn-io/cr-sqlite/blob/main/py/perf/perf.ipynb

- Currently inserts into CRRs are 2.5x slower than inserts into regular SQLite tables.
- Reads are the same speed

# Usage

The full documentation site is available [here](https://vlcn.io/docs).

`crsqlite` exposes three main APIs:

- A function extension (`crsql_as_crr`) to upgrade existing tables to "crrs" or "conflict free replicated relations"
  - `SELECT crsql_as_crr('table_name')`
- A virtual table (`crsql_changes`) to ask the database for changesets or to apply changesets from another database
  - `SELECT "table", "pk", "cid", "val", "col_version", "db_version", "site_id", cl, seq FROM crsql_changes WHERE db_version > x AND site_id = crsql_site_id()` -- to get local changes
  - `SELECT "table", "pk", "cid", "val", "col_version", "db_version", "site_id", cl, seq FROM crsql_changes WHERE db_version > x AND site_id != some_site_id` -- to get all changes excluding those synced from some actor
  - `INSERT INTO crsql_changes VALUES ([patches received from select on another peer])`
- And `crsql_begin_alter('table_name')` & `crsql_alter_commit('table_name')` primitives to allow altering table definitions that have been upgraded to `crr`s.
  - Until we move forward with extending the syntax of SQLite to be CRR aware, altering CRRs looks like:
    ```sql
    SELECT crsql_begin_alter('table_name');
    -- 1 or more alterations to `table_name`
    ALTER TABLE table_name ...;
    SELECT crsql_commit_alter('table_name');
    ```
    A future version of cr-sqlite may extend the SQL syntax to make this more natural.

Application code uses the function extension to enable crr support on tables.

Networking code uses the `crsql_changes` virtual table to fetch and apply changes.

Usage looks like:

```sql
-- load the extension if it is not statically linked
.load crsqlite
.mode qbox
-- create tables as normal
create table foo (a primary key not null, b);
create table baz (a primary key not null, b, c, d);

-- update those tables to be crrs / crdts
select crsql_as_crr('foo');
select crsql_as_crr('baz');

-- insert some data / interact with tables as normal
insert into foo (a,b) values (1,2);
insert into baz (a,b,c,d) values ('a', 'woo', 'doo', 'daa');

-- ask for a record of what has changed
select "table", "pk", "cid", "val", "col_version", "db_version", "site_id", "cl", "seq" from crsql_changes;

┌───────┬─────────────┬─────┬───────┬─────────────┬────────────┬──────────────────────────────────────┬────┬─────┐
│ table │     pk      │ cid │  val  │ col_version │ db_version │ "site_id" │ cl │ seq │
├───────┼─────────────┼─────┼───────┼─────────────┼────────────┼──────────────────────────────────────┼────┼─────┤
│ 'foo' │ x'010901'   │ 'b' │ 2     │ 1           │ 1          │ x'049c48eadf4440d7944ed9ec88b13ea5'  │ 1  │ 0   │
│ 'baz' │ x'010b0161' │ 'b' │ 'woo' │ 1           │ 2          │ x'049c48eadf4440d7944ed9ec88b13ea5'  │ 1  │ 0   │
│ 'baz' │ x'010b0161' │ 'c' │ 'doo' │ 1           │ 2          │ x'049c48eadf4440d7944ed9ec88b13ea5'  │ 1  │ 1   │
│ 'baz' │ x'010b0161' │ 'd' │ 'daa' │ 1           │ 2          │ x'049c48eadf4440d7944ed9ec88b13ea5'  │ 1  │ 2   │
└───────┴─────────────┴─────┴───────┴─────────────┴────────────┴──────────────────────────────────────┴────┴─────┘

-- merge changes from a peer
insert into crsql_changes
  ("table", "pk", "cid", "val", "col_version", "db_version", "site_id", "cl", "seq")
  values
  ('foo', x'010905', 'b', 'thing', 5, 5, X'7096E2D505314699A59C95FABA14ABB5', 1, 0);
insert into crsql_changes ("table", "pk", "cid", "val", "col_version", "db_version", "site_id", "cl", "seq")
  values
  ('baz', x'010b0161', 'b', 123, 101, 233, X'7096E2D505314699A59C95FABA14ABB5', 1, 0);

-- check that peer's changes were applied
sqlite> select * from foo;
┌───┬─────────┐
│ a │    b    │
├───┼─────────┤
│ 1 │ 2       │
│ 5 │ 'thing' │
└───┴─────────┘

select * from baz;
┌─────┬─────┬───────┬───────┐
│  a  │  b  │   c   │   d   │
├─────┼─────┼───────┼───────┤
│ 'a' │ 123 │ 'doo' │ 'daa' │
└─────┴─────┴───────┴───────┘

-- tear down the extension before closing the connection
-- https://sqlite.org/forum/forumpost/c94f943821
select crsql_finalize();
```

# Packages

Pre-built binaries of the extension are available in the [releases section](https://github.com/vlcn-io/cr-sqlite/releases).

These can be loaded into `sqlite` via the [`load_extension` command](https://www.sqlite.org/loadext.html#loading_an_extension) from any language (Python, NodeJS, C++, Rust, etc.) that has SQLite bindings.

The entrypoint to the loadable extension is [`sqlite3_crsqlite_init` ](https://github.com/vlcn-io/cr-sqlite/blob/92df9b4f3a6bdf2bd7c5d9a76949496fa5dc88cf/core/src/crsqlite.c#L536) so you'll either need to provide that to `load_extension` or rename your binary to `crsqlite.[dylib/dll/so]`. See the linked sqlite [`load_extension` docs](https://www.sqlite.org/loadext.html#loading_an_extension).

```
load_extension(extension_path, 'sqlite3_crsqlite_init')
```

> Note: if you're using `cr-sqlite` as a run time loadable extension, loading the extension should be the _first_ operation you do after opening a connection to the database. The extension needs to be loaded on every connection you create.

For a WASM build that works in the browser, see the [js](https://github.com/vlcn-io/js) directory.

For UI integrations (e.g., React) see the [js](https://github.com/vlcn-io/js) directory.

# How does it work?

There are two approaches with very different tradeoffs. Both will eventually be supported by `cr-sqlite`. `v1` (and current releases) support the first approach. `v2` will support both approaches.

## Approach 1: History-free CRDTs

Approach 1 is characterized by the following properties:

1. Keeps no history / only keeps the current state
2. Automatically handles merge conflicts. No options for manual merging.
3. Tables are Grow Only Sets or variants of Observe-Remove Sets
4. Rows are maps of CRDTs. The column names being the keys, column values being a specific CRDT type
5. Columns can be counter, fractional index or last write wins CRDTs.
   1. multi-value registers, RGA and others to come in future iterations

Tables which should be synced are defined as a composition of other types of CRDTs.

Example table definition:

```sql
CREATE CLSet post (
 id INTEGER PRIMARY KEY NOT NULL,
 views COUNTER,
 content PERITEXT,
 owner_id LWW INTEGER
);
```

> note: given that extensions can't extend the SQLite syntax this is notional. We are, however, extending the libSQL syntax so this will be available in that fork. In base SQLite you'd run the `select crsql_as_crr` function as seen earlier.

- CLSet - [causal length set](https://dl.acm.org/doi/pdf/10.1145/3380787.3393678)
- COUNTER - [distributed counter](https://www.cs.utexas.edu/~rossbach/cs380p/papers/Counters.html)
- PERITEXT - [collaborative text](https://www.inkandswitch.com/peritext/)

Under approach 1, merging two tables works roughly like so:

1. Rows are identified by primary key
2. Tables are unioned (and a delete log is consulted) such that both tables will have the same rows.

If a row was modified in multiple places, then we merge the row. Merging a row involves merging each column of that row according to the semantics of the CRDT for the column.

1. Last-write wins just picks the lastest write
2. Counter CRDT sums the values
3. Multi-value registers keep all conflicting values
4. Fractional indices are taken as last write

For more background see [this post](https://vlcn.io/blog/gentle-intro-to-crdts.html).

Notes:

- LWW, Fractional Index, Observe-Remove sets are available now.
- Counter and rich-text CRDTs are still [being implemented](https://github.com/vlcn-io/cr-sqlite/issues/65).
- Custom SQL syntax will be available in our libSQL integration. The SQLite extension requires a slightly different syntax than what is depicted above.

## Approach 2: Causal Event Log

> To be implemented in v2 of cr-sqlite

Approach 2 has the following properties:

1. A history of every modification that happens to the database is kept
   1. This history can be garbage collected in certain network topologies
2. Merge conflicts can be automatically handled (via CRDT style rules) or the developer can define their own conflict resolution plan.
3. The developer can choose to fork the data on merge conflict rather than merging
4. Forks can live indefinitely or a specific fork can be chosen and other forks dropped

This is much more akin to git and event sourcing but with the drawback being that it is much more write heavy and much more space intensive.

# Building

For a stable version, build against a [release tag](https://github.com/vlcn-io/cr-sqlite/releases) as main may not be 100% stable.

You'll need to install Rust.

- Installing Rust: https://www.rust-lang.org/tools/install

## [Run Time Loadable Extension](https://www.sqlite.org/loadext.html)

Instructions on building a native library that can be loaded into SQLite in non-wasm environments.

```bash
rustup toolchain install nightly # make sure you have the rust nightly toolchain
git clone --recurse-submodules git@github.com:vlcn-io/cr-sqlite.git
cd cr-sqlite/core
make loadable
```

This will create a shared library at `dist/crsqlite.[lib extension]`

[lib extension]:

- Linux: `.so`
- Darwin / OS X: `.dylib`
- Windows: `.dll`

## WASM

For a WASM build that works in the browser, see the [js](https://github.com/vlcn-io/js) repository.

## CLI

Instructions on building a `sqlite3` CLI that has `cr-sqlite` statically linked and pre-loaded.

In the `core` directory of the project, run:

```bash
make sqlite3
```

This will create a `sqlite3` binary at `dist/sqlite3`

## Tests

core:

```bash
cd core
make test
```

py integration tests:

```bash
cd core
make loadable
cd ../py/correctness
./install-and-test.sh
```

# JS APIs

JS APIs for using `cr-sqlite` in the browser are not yet documented but exist in the [js repo](https://github.com/vlcn-io/js). You can also see examples of them in use here:

- [Observable Notebook](https://observablehq.com/@tantaman/cr-sqlite-basic-setup)
- https://github.com/vlcn-io/live-examples

# Research & Prior Art

cr-sqlite was inspired by and built on ideas from these papers:

- [Towards a General Database Management System of Conflict-Free Replicated Relations](https://munin.uit.no/bitstream/handle/10037/22344/thesis.pdf?sequence=2)
- [Conflict-Free Replicated Relations for Multi-Synchronous Database Management at Edge](https://hal.inria.fr/hal-02983557/document)
- [Merkle-CRDTs](https://arxiv.org/pdf/2004.00107.pdf)
- [Time, Clocks, and the Ordering of Events in a Distributed System](https://lamport.azurewebsites.net/pubs/time-clocks.pdf)
- [Replicated abstract data types: Building blocks for collaborative applications](http://csl.skku.edu/papers/jpdc11.pdf)
- [CRDTs for Brrr](https://josephg.com/blog/crdts-go-brrr/)

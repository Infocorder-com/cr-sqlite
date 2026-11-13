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

This repository is an **extension AND SPECIALIZATION** of the `superfly` fork, carrying changes we wanted for [Infocorder](https://infocorder.com)'s hub-and-spoke SQLite sync that do **not** all generalize to other use cases. If you just want vanilla cr-sqlite, use `vlcn-io/cr-sqlite` or `superfly/cr-sqlite` instead — this fork deliberately diverges from both.

It also vendors a **different `sqlite-rs-embedded`**: the submodule points at [`Infocorder-com/superfly_sqlite-rs-embedded`](https://github.com/Infocorder-com/superfly_sqlite-rs-embedded) (our fork of Fly.io's fork) rather than `vlcn-io/sqlite-rs-embedded`. That brings a real soundness fix — `value_text` / `value_blob` called `slice::from_raw_parts` on a possibly-NULL pointer, which is UB and is hit on every merge and pk unpack — plus the stable-Rust support that change 3 depends on.

**Detecting what you've got.** `crsql_build_id()` returns `<capability-tag> <commit-sha>`, where the tag is a `+`-joined cumulative list. A host should **substring-match the token it cares about** rather than compare the whole string, so appending capabilities never breaks an existing check:

```
sqlite> SELECT crsql_build_id();
e1-fd-close+e2-tbl-pushdown+e3-tbl-collate-gate+e4-init-atomic+e5-merge-validation <commit-sha>
```

| token | capability |
|---|---|
| `e1-fd-close` | change 1 — file descriptors released on connection close |
| `e2-tbl-pushdown` | change 2 — `crsql_changes WHERE "table" = ?` pushed into the clock union |
| `e3-tbl-collate-gate` | change 2's collation gate — the pushdown is declined under a non-`BINARY` collation |
| `e4-init-atomic` | change 4 — extension init is atomic, a Half-Initialized Database is healed, and opens through `sqlite3_crsqlite_autoext_init` wait for a competing connection. ⚠️ The token does not show which entry point a host registered |
| `e5-merge-validation` | change 5 — a peer's change merged through `crsql_changes` is refused, before anything is written, when its packed primary key has the wrong column count or a counter is outside 0 to 2^53; a NUL in its table name no longer reports `SQLITE_NOMEM` |

Upstream cr-sqlite has no `crsql_build_id()` at all, so its absence distinguishes a stock build from this fork.

The changes are:

### 1. Finalize-on-close: release OS file descriptors when a connection closes (already shipped)

Stock cr-sqlite keeps its internal prepared statements (in `crsql_ExtData`) alive for the life of a connection and never finalizes them on close, so — unless the host explicitly calls `SELECT crsql_finalize()` first — `sqlite3_close` / `sqlite3_close_v2` leaves the connection a "zombie" and the OS file handles (`*.db`, `*-wal`, `*-shm`) leak until the process exits. This fork makes cr-sqlite finalize its *own* internal statements as part of connection teardown, so the handles are released without the host having to call `crsql_finalize()` first. It also adds `crsql_build_id()` as a build/provenance marker. See [`docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md`](docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md).

- **Benefit:** long-lived processes that open and close *many* connections (e.g. a server holding one SQLite database per project/tenant) no longer leak file descriptors, and no app-side `crsql_finalize()` dance is required on every teardown path.
- **Cost / who should be careful:** it changes connection-teardown behavior (cr-sqlite now hooks close to finalize its cached statements). Hosts that deliberately rely on stock close/finalize semantics, or that already call `crsql_finalize()` themselves, should confirm the new path suits them. And, like any fork change, it makes tracking upstream exactly harder.

### 2. `crsql_changes` table-filter pushdown (shipped)

Stock cr-sqlite's `crsql_changes` virtual table treats `WHERE "table" = ?` as a *post-scan* filter: it scans the entire `UNION ALL` over **every** table's change-clock and discards non-matching rows afterward, so a table-scoped read costs O(total change rows across the whole database), independent of the target table's size. This fork patches the vtab's `xBestIndex` so a `"table" = ?` (or `"table" IN (...)`) constraint is pushed into the union and prunes the scan to the one matching `<table>__crsql_clock`. The predicate is emitted as `CAST(tbl AS TEXT) = ?`, which preserves SQLite's normal TEXT-affinity comparison, so **result sets are byte-identical to stock cr-sqlite for every table name**, including numeric ones.

A second commit added the **collation gate** (`e3-tbl-collate-gate`): SQLite hands a virtual table an equality constraint *without* filtering on collation, so `WHERE "table" = 'ITEMS' COLLATE NOCASE` initially arrived as a plain equality and got pruned under `BINARY`, silently returning nothing. The vtab now asks `sqlite3_vtab_collation()` and **declines** the constraint unless the comparison is `BINARY`, in which case SQLite applies it post-scan exactly as before the pushdown existed. So **result sets are byte-identical to stock cr-sqlite for every query shape**, not just for ordinary ones. See [`docs/CRSQL_CHANGES_TABLE_FILTER_PUSHDOWN.md`](docs/CRSQL_CHANGES_TABLE_FILTER_PUSHDOWN.md).

- **Benefit:** bounded table-scoped change reads — O(rows in that table), not O(total). Anything that syncs, backfills, reconciles, audits or inspects *specific* tables via `crsql_changes` stays cheap as the rest of the database grows.
- **Cost / who should be careful:** results are unchanged, but two behaviours differ.
  - A `WHERE "table" = ? COLLATE NOCASE` (or any non-`BINARY` collation) query is **correct but not accelerated** — the gate declines the pushdown and it falls back to a full scan.
  - `WHERE "table" IN (...)` keeps identical results but changes *plan*: SQLite reports `IN` as an equality, so it issues one `xFilter` per listed table plus a sort, instead of one scan. Each probe is pruned, but the generated SQL is re-prepared per value, and that SQL grows with the number of CRR tables. Time it if you use `IN` over many tables.
  - Otherwise the usual fork caveat: it is a divergence from upstream. No schema, wire-format, or ABI change.

### 3. Builds on stable Rust (shipped)

Upstream cr-sqlite requires a **nightly** toolchain: `sqlite-rs-embedded` used the `concat_idents` feature (removed from the compiler in Rust 1.90, so newer toolchains reject it outright), and cr-sqlite's own crates used `core_intrinsics`, `lang_items` and `vec_into_raw_parts`. Consumers were stuck pinning an old nightly, or patching the feature gates out at build time.

This fork builds on **stable Rust 1.95.0**, pinned in each crate's `rust-toolchain.toml`. Following Fly.io's unmerged `f347c8d9`: the stale gates are deleted, `core::intrinsics::abort()` becomes `stable_trap::abort()`, and the `eh_personality` lang item is replaced by a plain `#[no_mangle] extern "C" fn rust_eh_personality()` that defines the symbol the linker wants without needing nightly. See [`docs/STABLE_RUST_PORT.md`](docs/STABLE_RUST_PORT.md).

- **Benefit:** no nightly pin, and no build-time patching of feature gates. One less way for a toolchain change to break the build — which had already happened once, when `concat_idents` was removed.
- **Cost / who should be careful:** `make asan` still needs nightly, because `-Zbuild-std` is a nightly-only flag; the same applies to the cross-compiled targets (`CI_MAYBE_TARGET`, iOS, Android), which are untouched here. Native `make loadable` / `make static` need only stable. Both `Cargo.lock` files are now lockfile **v4**, which requires cargo ≥ 1.78 — so build this ref with the toolchain it pins, and don't pair a new ref with an old nightly.

### 4. Atomic extension init, Half-Initialized Database heal, and an init busy timeout (`e4-init-atomic`)

Stock cr-sqlite runs a setup routine on every new connection, and on a brand-new file it writes its bookkeeping tables in **several separately committed steps**, inside `sqlite3_open_v2`, before the host can set a busy timeout. When a connection pool opens one fresh file from several connections at once, most opens fail with `SQLITE_BUSY` — and one interleaving leaves a file with an **empty** `crsql_master` table that every later open refuses forever with *"Opening a db created with cr-sqlite version 0 is not supported"* (the **Init Race Wedge**; the file is a **Half-Initialized Database**). A crash, a full disk or an I/O error between those commits does the same with a single connection. This fork:

- runs every write of init in **one transaction** (`BEGIN IMMEDIATE`, entered only when a read-only check says the file needs initializing), so a file is either untouched or fully initialized; re-opening an initialized file still writes nothing and takes no write lock;
- **heals** a Half-Initialized Database that holds no cr-sqlite state at all (an empty `crsql_master`, empty bookkeeping tables, and no other object whose name contains `crsql`; application tables are allowed), initializing it as if it were new. Genuine older databases, including v0.12-era files with an empty `crsql_master`, are still refused;
- adds a second entry point, **`sqlite3_crsqlite_autoext_init`**, which runs init under a busy handler that allows **2 s of lock waiting in total per open**, and clears it before returning;
- fixes init's error paths: a failed init rolls back to its own savepoint instead of the host's transaction, frees everything exactly once, and never leaves unfinalized statements behind, so a failed open can always be closed.

See [`docs/CRSQLITE_INIT_RACE_FIX.md`](docs/CRSQLITE_INIT_RACE_FIX.md).

- **Benefit:** pool start on a fresh file no longer fails opens or wedges the file, and a file wedged by an earlier build opens again. Opens of initialized files no longer fail when a pool-mate holds a lock for a moment (for example while switching the file to WAL).
- **Cost / who should be careful:**
  - **Static hosts must register `sqlite3_crsqlite_autoext_init`** with `sqlite3_auto_extension` to get the wait. A host that keeps registering `sqlite3_crsqlite_init` gets the atomic init and the heal, but its opens still fail at once under contention (cleanly now). `core/src/core_init.c` registers the new entry point.
  - ⛔ **Never use `sqlite3_crsqlite_autoext_init` with `load_extension`.** It installs and then clears the connection's busy handler, which on the `load_extension` path would remove the host's own. `sqlite3_crsqlite_init` stays the `load_extension` entry point and does not touch the busy handler.
  - An open through the new entry point **can spend up to 2 s waiting** for a competing connection (plus init's own work). On a host that opens on a bounded thread pool (for example a dirty I/O scheduler), several waiting opens occupy several threads for up to that long.
  - ⛔ **A failed `load_extension` of cr-sqlite leaves the connection unusable.** SQLite unloads the library immediately, but the functions and virtual-table modules init registered before the failure stay registered and point into the unloaded library. Close the connection, and do not call any `crsql_*` function or use `crsql_changes` on it. Closing is safe: init registers no destructor that lives in the library until its last fallible step.
  - A Half-Initialized Database now **opens** instead of failing; a host that parsed the version-0 message to detect wedged files will stop seeing it.

### 5. Merge input validation (`e5-merge-validation`)

Every row inserted into `crsql_changes` comes from another node, so a merge must not trust it. Stock cr-sqlite accepted a change whose packed primary key had **fewer** columns than the table's primary key — the missing key columns bound as NULL, the merged value landed in a key column, and the NULL key was then sent on to every peer — and accepted counters that broke later local writes (a merged `col_version` of `i64::MAX` made every later update of that cell fail). This fork's `merge_insert` now refuses, with `SQLITE_ERROR` and before writing anything:

- a change whose packed primary key has a different column count from the table's primary key (**Packed PK Arity Check**);
- a change whose `col_version`, `db_version`, causal length (`cl`) or `seq` is negative or above 2^53 (**Merge Counter Bound**). Honest peers never come near the bound.

It also stops a NUL byte in a peer's table name from turning the "no such table" error into `SQLITE_NOMEM`; the NUL is shown as `\0` and the merge returns `SQLITE_ERROR` (**Error Message NUL Escape**). See [`docs/CRSQLITE_INIT_RACE_FIX.md`](docs/CRSQLITE_INIT_RACE_FIX.md), **Merge Input Validation**.

- **Benefit:** a malformed or hostile change is an error instead of silent data corruption, a wedged cell, or a misleading out-of-memory code.
- **Cost / who should be careful:** a change that was accepted before can now be refused. A host that applies a batch of changes in one transaction loses the whole batch when one change is refused. A database that already holds an out-of-range counter or a NULL key (only possible if a peer sent one to a build without this change) is not repaired, and fixed peers refuse the out-of-range counters it re-emits. No API, on-disk or wire-format change.

### Especially poor fits for this fork

- You want a **drop-in, binary-compatible** replacement for stock cr-sqlite, or need to **track `vlcn-io` / `superfly` upstream exactly** — this fork intentionally diverges and is maintained for Infocorder's needs, not general parity.
- You depend on stock cr-sqlite's **connection-close / finalize semantics** — change 1 alters those.
- You read `crsql_changes` with **`"table" IN (...)` over many tables** and are sensitive to plan changes — see change 2's cost note.
- You need the **cross-compiled or sanitizer targets** (iOS, Android, `CI_MAYBE_TARGET`, `make asan`) — those still require nightly; change 3 only makes the native builds stable.
- You need to **keep receiving upstream fixes and features** — see the drift warning below.

For everyone else, this fork is effectively a superset of `superfly`'s behavior: `crsql_changes` result sets are byte-identical for every query shape (change 2 is faster for table-scoped reads, not different), and the on-disk format, the wire/changeset format and the public API are all unchanged. The only additions to the API surface are `crsql_build_id()`, change 1's teardown behaviour, and change 4's auto-extension entry point `sqlite3_crsqlite_autoext_init`.

### ⚠️ Upstream drift is the biggest risk of depending on this repo

**This repository has drifted from and will likely _not_ track `vlcn-io/cr-sqlite` or `superfly/cr-sqlite` going forward.** It is maintained for [Infocorder](https://infocorder.com)'s needs. Merging upstream commits is a manual, best-effort exercise (see [How to merge in commits from superfly](#how-to-merge-in-commits-from-superfly)) that happens only when we happen to need something from upstream — there is no schedule, no automation, no CI gate on upstream parity, and no commitment to do it at all.

What that means for you, concretely:

- **You will likely miss upstream bug fixes**, including correctness fixes to the CRDT merge logic and fixes to bugs neither we nor upstream have found yet. This is the one that should worry you most: a convergence bug fixed upstream stays broken here until someone notices and ports it.
- **You will likely miss upstream features and performance work.** Anything `superfly` or `vlcn-io` add after our last merge simply will not be here.
- **The gap widens over time**, and every one of our local changes touching `xBestIndex`, the changes vtab, and connection teardown makes the *next* merge harder — so the practical cost of catching up grows the longer we go without doing it.
- **Security and upgrade response is on you.** We do not promise timely rebases onto upstream, and we do not track upstream advisories.
- **The bundled SQLite may lag**, since upgrading it is an upstream-driven change we would have to pull through ourselves.

If any of that is unacceptable, depend on [`superfly/cr-sqlite`](https://github.com/superfly/cr-sqlite) directly. If you specifically need the two changes above, the healthier pattern is to take them as patches applied on top of a current upstream checkout, rather than depending on this fork as a long-lived source. We would rather you carry a two-patch delta you control than inherit our drift.

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

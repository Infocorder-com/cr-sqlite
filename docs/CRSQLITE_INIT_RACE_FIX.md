# Making cr-sqlite's per-connection init atomic — the init race wedge

**Status:** 📋 **Plan, not built** (2026-11-13). Docs only; no code has changed. Awaiting adversarial
review.
**Area:** extension init — `core/rs/core/src/lib.rs` `sqlite3_crsqlcore_init`, and
`core/rs/core/src/bootstrap.rs`.
**Compatibility:** no wire or changeset format change, no new C ABI. On-disk format is unchanged for
every database that opens today. The one behaviour change is that a **Half-Initialized Database** —
which today can never be opened — opens.
**Downstream integration:** `silicon_brain` `docs/CRSQLITE_INIT_RACE_FIX_INTEGRATION.md` (pin bump,
static NIF rebuilds, fleet rollout, app tests). Release logistics go in the shared
`docs/CRSQLITE_FORK_UPGRADE_COORDINATION.md` when the release is built.
**Ruling:** the operator ruled on 2026-11-13 that the fix lives in this fork, with no app-side guard
(**Init Race Fork-Only Fix**, recorded in `silicon_brain` `docs/DECISION_REGISTER.md`).

---

## Why this plan exists — in plain language

cr-sqlite is a SQLite extension. Every time a program opens a connection to a database file with
cr-sqlite loaded, cr-sqlite runs a short setup routine (**extension init**) on that connection. On a
brand-new, empty file, that routine writes cr-sqlite's own bookkeeping into the file: a table that
records which cr-sqlite version created the database (`crsql_master`, holding a `crsqlite_version`
row), a table holding this database's unique identity (`crsql_site_id`), and a few smaller tables. On
a file that already has them, init only reads.

The defect is that init writes those things in **several separate, independently committed steps**,
and it runs **inside the open call itself**, before the program has had any chance to tell SQLite
"if the file is busy, wait a bit" (a busy timeout). So when several connections open the **same
brand-new file at the same moment** — which is exactly what a connection pool does when it starts — they
collide. Usually the collision is harmless: one connection finishes setup, the others fail their open,
and the pool retries them a moment later.

But one interleaving is not harmless. A connection can commit the `crsql_master` table, and then fail
to write the version row into it because another connection holds the lock (`SQLITE_BUSY`). Its open
fails, and it leaves behind a file with an **empty** `crsql_master`. From then on, every open of that
file — by anyone, forever — sees that `crsql_master` exists, concludes "this is an existing database",
looks for the version row, finds none, treats the version as 0, and refuses with *"Opening a db created
with cr-sqlite version 0 is not supported."* The file can never be opened again until someone deletes
it. We call this the **Init Race Wedge**, and the file it leaves behind
a **Half-Initialized Database**.

What a user sees:

- **A fresh install that will not start.** On a new desktop install, a database the app creates at
  first boot (on Infocorder's spokes this is `parking.db`) is opened by a pool of connections. If the
  race lands badly, the app fails to start on that machine and keeps failing on every restart, with a
  log full of `database_open_failed`. Nothing the user does fixes it; deleting a file they have never
  heard of does.
- **A new project that is "broken" from birth.** Any path that creates a database file and lets a
  multi-connection pool open it before one connection has finished setup can leave a project whose
  database can never be opened. No data is lost — the file is empty — but the project is unusable.
- **Unexplained "harmless" noise.** The losing side of the same race is the `Exqlite.Connection …
  failed to connect: database_open_failed` line that Infocorder has logged at every test boot for
  months, and that its test helper filters out. It was believed harmless. It is harmless only when the
  winner finishes.
- **A test suite that "breaks for no reason".** Infocorder recorded a leftover test metadata database
  holding only cr-sqlite's tables that made every later test run fail until it was deleted, with the
  cause not found (`silicon_brain` `docs/IMPROVEMENT_IDEAS.md` → **Stale Test Metadata Database**).
  That file is a Half-Initialized Database.

The evidence (Infocorder, 2026-11-13):

- A preserved Half-Initialized Database (`spoke_meta_test.db.halfinit`, 28,672 bytes, rollback
  `delete` journal mode, file change counter 3 — i.e. exactly three commits) holds
  `crsql_tracked_peers`, `crsql_db_versions` and an **empty** `crsql_master`, and nothing else: no
  `crsql_site_id`, no application tables. Read-only inspection of a copy confirms it.
- A probe opened 10 connections concurrently on a fresh file, 1,500 times, using a statically linked
  build of this fork. **Every one of the 1,500 rounds** had at least one failed open, and **5 of the
  1,500 files were left permanently unopenable.**

This plan makes the fix in three parts, each with its own name:

1. **Atomic Init Transaction** — all of init's writes happen in one transaction, so a file is either
   untouched or fully initialized. This stops new wedges.
2. **Init Residue Heal** — a file that is already a Half-Initialized Database is recognised, by a
   strict test that cannot match any database holding data, and initialized as if it were new. This
   rescues files wedged by earlier builds.
3. **Init Busy Retry** — init waits briefly for a competing connection instead of failing at once.
   This removes the noise, so a pool starting on a new file no longer sees failed opens.

It also fixes two smaller defects found on the way: a missing unique index that can never be
recreated (**Site-ID Index Gap**), and error paths that issue a bare `ROLLBACK` and so can abort a
transaction that belongs to the host (**Init Host Rollback**).

---

## Summary

| part | what it changes | required? |
|---|---|---|
| **Atomic Init Transaction** | wraps every persistent write of init in one `BEGIN IMMEDIATE` … `COMMIT`, entered only when a read-only check says the file needs initializing | ✅ required — this is the fix |
| **Init Residue Heal** | treats an exact, data-free Half-Initialized Database as a blank slate | ✅ required — files already wedged in the field stay wedged without it |
| **Init Busy Retry** | bounded internal retry on `SQLITE_BUSY` during init, without touching the host's busy handler | ⭐ recommended — correctness does not need it, but the pool-start noise only goes away with it |
| **Init Host Rollback** | error paths roll back to their own savepoint instead of issuing a bare `ROLLBACK` | ✅ required (it falls out of the transaction rewrite) |
| `crsql_build_id()` token | appends `e4-init-atomic` | ✅ required, so hosts can gate on the fix |

**Recommendation:** ship all five together as one release (`e4-init-atomic`).

---

## The mechanism — verified against this fork's source

### The init sequence

`sqlite3_crsqlite_init` (`core/src/crsqlite.c`) calls `sqlite3_crsqlrustbundle_init`
(`core/rs/bundle/src/lib.rs`), which calls `sqlite3_crsqlcore_init` (`core/rs/core/src/lib.rs`). In
order, the persistent steps are:

1. `bootstrap::crsql_init_peer_tracking_table` — `CREATE TABLE IF NOT EXISTS crsql_tracked_peers (…) STRICT`.
2. `bootstrap::crsql_init_db_versions_table` — `CREATE TABLE IF NOT EXISTS crsql_db_versions (…) STRICT`.
3. `bootstrap::crsql_maybe_update_db`:
   1. `has_table(db, "crsql_master")` is computed **first**, outside any transaction.
   2. `crsql_create_schema_table_if_not_exists` opens `SAVEPOINT crsql_create_schema_table`, runs
      `CREATE TABLE IF NOT EXISTS "crsql_master" ("key" TEXT PRIMARY KEY, "value" ANY)`, and
      `RELEASE`s it. A savepoint opened outside a transaction behaves as `BEGIN DEFERRED`, so the
      `RELEASE` **commits** `crsql_master` immediately.
   3. `SAVEPOINT crsql_maybe_update_db`; `maybe_update_db_inner(db, !has_schema_table, …)`:
      on a blank slate it writes `INSERT OR REPLACE INTO crsql_master VALUES ('crsqlite_version', 170000)`;
      otherwise it reads the row, treats a missing row as version 0, and fails with
      *"Opening a db created with cr-sqlite version 0 is not supported"* when it is below
      `CRSQLITE_VERSION_0_17_0` (`consts.rs`; today `CRSQLITE_VERSION` equals it, 170000).
   4. On success `RELEASE` (commit); on failure a bare `ROLLBACK`.
4. `crsql_newExtData` (`core/src/ext-data.c`) prepares statements that read `crsql_db_versions` and
   `crsql_master` (the `config.%` rows), so both must exist by now. Then `crsql_update_site_id` is
   registered — it must be, before step 5, because inserting into an existing `crsql_site_id` prepares
   its triggers, and the triggers call that function.
5. `bootstrap::crsql_init_site_id` → `init_site_id`: if there is no `crsql_site_id` table,
   `create_site_id_and_site_id_table` runs `CREATE TABLE "crsql_site_id" (…)` (no `IF NOT EXISTS`)
   and `CREATE UNIQUE INDEX crsql_site_id_site_id …` through one `exec_safe` call — which is
   `sqlite3_exec`, so **each statement commits separately** — and then `insert_site_id` inserts
   ordinal 0. If the table exists without an ordinal-0 row, it inserts one.
6. `crsql_initSiteIdExt` prepares statements against `crsql_site_id`.
7. `bootstrap::create_site_id_triggers` — three `CREATE TRIGGER IF NOT EXISTS`, again each committed
   separately.

The commit and rollback hooks are installed by `sqlite3_crsqlite_init` only **after** all of this, so
init's own writes never reach `crsql_commit_hook`.

**On a fully initialized database, steps 1–7 write nothing.** `CREATE TABLE IF NOT EXISTS` and
`CREATE TRIGGER IF NOT EXISTS` on existing objects, and an empty `SAVEPOINT`/`RELEASE`, all succeed on a
**read-only** connection (checked with the stock 3.50.2 shell, 2026-11-13). This property must survive
the fix; see **Keeping the re-open path read-only**.

### Why concurrent opens race

- With static linking the host registers `sqlite3_crsqlite_init` as an auto-extension
  (`core/src/core_init.c` does the same for this repo's CLI and tests), so init runs **inside**
  `sqlite3_open_v2`. The host cannot call `sqlite3_busy_timeout` until the open returns, so init has
  **no busy handler**: any lock conflict is an immediate `SQLITE_BUSY`.
- A brand-new file is in rollback-journal (`delete`) mode during init, because hosts set
  `journal_mode=WAL` with a pragma **after** the open. In rollback mode a commit needs an `EXCLUSIVE`
  lock, which any other connection's `SHARED` lock (even one reading `sqlite_master` in `has_table`)
  blocks.
- Steps 1, 2, 3.2, 3.3, 5 (three statements) and 7 (three statements) are separate transactions, so
  other connections observe — and act on — every intermediate state.

### The three failure modes

| failure | interleaving | result |
|---|---|---|
| **Init Race Wedge** | connection B commits `crsql_master` (step 3.2), then its version `INSERT` fails `SQLITE_BUSY`; every other connection already saw `crsql_master` and so took the "existing database" branch | **permanent**: no connection ever writes the version row again |
| transient version-0 refusal | A computes `has_schema_table = true` in the window between B's two commits, finds no version row, refuses | A's open fails; B finishes; pool retry succeeds |
| transient site-id collision | A and B both see no `crsql_site_id`; B creates it; A's un-guarded `CREATE TABLE` fails "already exists" — or A's ordinal-0 `INSERT` hits B's row | A's open fails; retry succeeds. Two different site ids can never both be stored: ordinal is the primary key |

`has_schema_table` being computed before the savepoint is what turns the second row into the first: a
connection that saw the half-state never writes the row, and the one connection that would have
written it failed.

### Two smaller defects in the same code

- **Site-ID Index Gap.** If `CREATE TABLE crsql_site_id` commits and `CREATE UNIQUE INDEX
  crsql_site_id_site_id` then fails, the table persists without its index. Every later open sees the
  table and never creates the index. The table then accepts duplicate `site_id` values, which the
  ordinal lookup (`SELECT ordinal FROM crsql_site_id WHERE site_id = ?`) does not expect. Not
  observed in the field; it is a property of the code.
- **Init Host Rollback.** `crsql_create_schema_table_if_not_exists` and `crsql_maybe_update_db`
  recover from an error with a bare `ROLLBACK`, not `ROLLBACK TO <savepoint>`. During an
  auto-extension open there is no outer transaction, so this is equivalent. But when a host calls
  `load_extension` inside its own transaction, a failing init **rolls back the host's transaction**.

---

## The fix

### Atomic Init Transaction — option (a), recommended

**Shape.** Split init into a cheap read-only check and a locked slow path:

1. **Init Completeness Check** (read-only). One read that decides whether the existing code would write
   anything. The file is complete when all of these hold:
   - tables `crsql_tracked_peers`, `crsql_db_versions`, `crsql_master` and `crsql_site_id` exist;
   - `crsql_master` holds `crsqlite_version` ≥ `CRSQLITE_VERSION`;
   - `crsql_site_id` holds an ordinal-0 row;
   - the three `crsql_site_id_*_trig` triggers exist.

   If complete, init skips straight to the connection-local work: ext data, function registration and
   statement preparation. That is exactly today's behaviour on an initialized file, minus nothing.
   The check's statements must be **finalized before the slow path starts**: a statement left
   un-reset keeps a `SHARED` lock and would make the slow path wait on itself.
2. **Slow path.** `BEGIN IMMEDIATE`. Then **re-run the same check under the lock** (double-checked:
   another connection may have finished while this one waited). Then run steps 1–7 in their current
   order — they are already idempotent, except step 5's `CREATE TABLE`, which `has_table` guards and
   which is now race-free because the lock is held. Then `COMMIT`.

`BEGIN IMMEDIATE`, not a deferred `SAVEPOINT`, because:

- it takes the `RESERVED` lock before reading, so the slow-path reads and writes see one consistent
  state, and `has_schema_table` can no longer be computed against a state that changes before the write;
- a deferred transaction that reads under `SHARED` and then upgrades to `RESERVED` can be refused with
  `SQLITE_BUSY` **without** the busy handler being invoked (SQLite's deadlock avoidance), so it cannot
  be made to wait.

**If the host is already inside a transaction** (`sqlite3_get_autocommit(db) == 0` — only possible on
the `load_extension` path), use `SAVEPOINT crsql_init` instead and roll back with
`ROLLBACK TO crsql_init; RELEASE crsql_init`. That fixes **Init Host Rollback** too.

**Everything joins the transaction.** All seven steps, including the peer and db-version tables, the
site-id table, its unique index, its row and its triggers. Leaving any of them out recreates a smaller
version of the same problem: a committed table with missing companions. Joining costs nothing — a
fresh init is a handful of DDL statements under one lock. It also closes the **Site-ID Index Gap** for
every database initialized from now on.

**What a losing connection sees.** It reads, finds the file incomplete, finalizes, and requests
`BEGIN IMMEDIATE`. Either it gets the lock first (and becomes the initializer) or it gets
`SQLITE_BUSY`. Without the **Init Busy Retry**, that `SQLITE_BUSY` fails its open — **cleanly**: it
wrote nothing and holds no lock, and the pool retries. With the retry, it waits, the re-check under
the lock finds the winner's committed work, and it proceeds without writing anything.

**What happens if the winner is interrupted** (error, `SQLITE_BUSY` on `COMMIT`, process killed): the
transaction rolls back, or the hot journal is rolled back by the next opener. The file returns to
untouched. That is the property this whole plan exists for.

**`COMMIT` can itself return `SQLITE_BUSY`** in rollback mode, while another connection still holds
`SHARED` from its completeness check. The transaction stays open; SQLite allows the `COMMIT` to be
retried. The **Init Busy Retry** retries it. Without the retry, init must `ROLLBACK` and fail the open.

**Implementation sketch.** The transaction has to span `lib.rs`, because `crsql_newExtData` and the
`crsql_update_site_id` registration sit between steps 3 and 5. Use one RAII guard (`InitTxn`), created
before step 1 on the slow path, whose `Drop` rolls back unless `commit()` was called. Every existing
`return null_mut()` then rolls back automatically, so the many early-return error paths in
`sqlite3_crsqlcore_init` cannot leave a half-written file.

**Rejected alternatives.**

- *Move only `crsql_create_schema_table_if_not_exists` inside the `crsql_maybe_update_db` savepoint.*
  Removes the wedge's exact interleaving, but not `has_schema_table` being read outside the lock, not
  the site-id collisions, and not the **Site-ID Index Gap**. It is also still a deferred savepoint that
  cannot wait.
- *`BEGIN IMMEDIATE` on every open, with no completeness check.* Makes every open of every database —
  including a healthy WAL database under a long write transaction — take the write lock. Opens would
  serialize, fail on a read-only connection, and fail while any writer is active. That is a regression
  for every host, not only racing ones.

### Init Residue Heal — option (b), recommended

The **Atomic Init Transaction** prevents new wedges. It does not rescue a file wedged by an earlier
build: `has_schema_table` is true and there is no version row, so the version-0 refusal still fires.

**The heal predicate.** Inside the slow path (so under the lock), treat the database as a blank slate
— `is_blank_slate = true` — when **all** of these hold:

1. `crsql_master` exists and has **zero rows** — not merely "no `crsqlite_version` row". (`crsql_master`
   also stores `config.*` rows written by `crsql_config_set`; a file with any row was opened
   successfully at least once.)
2. `crsql_tracked_peers` and `crsql_db_versions`, if they exist, have zero rows.
3. **No other cr-sqlite object exists.** No table, index, trigger or view in `sqlite_master` whose name
   contains `crsql` (case-insensitive), other than those three tables and their
   `sqlite_autoindex_*` indexes.

**Recommended scope: rule 3 constrains only cr-sqlite objects; application tables may be present.** A
Half-Initialized Database produced by a pool race contains no application tables (the field sample
contains none). But a file first created by a plain SQLite connection, then raced, would have them. For
that file, the blank-slate path is exactly what would have happened had the race not occurred: with no
clock tables and no CRR triggers there is nothing to migrate. A stricter alternative ("nothing at all
besides the three tables") is simpler to state; it leaves that second kind of file wedged. Either is
safe; the review should pick one.

**Why it cannot misfire on a genuine older database.** This fork ships three databases from earlier
cr-sqlite versions in `py/correctness/prior-dbs/` (inspected read-only, 2026-11-13):

| fixture | `crsql_master` | site-id table | other cr-sqlite objects |
|---|---|---|---|
| `v0.12.0.prior-db` | exists, **empty** | `__crsql_siteid` | `foo__crsql_*trig`, `bar__crsql_*trig`, clock tables |
| `v0.13.0.prior-db` | `crsqlite_version = 130000` | `__crsql_siteid` | triggers, clock tables |
| `v0.15.0.prior-db` | `crsqlite_version = 150000` | `__crsql_siteid` + index | triggers, clock tables |

⛔ **v0.12.0 is the case the naive rule gets wrong.** *"`crsql_master` exists, has no version row, and
there is no `crsql_site_id` table"* — the rule first proposed — **matches v0.12.0**, because that era
wrote no version row and named its site-id table `__crsql_siteid` (renamed to `crsql_site_id` in
`205d80ce`). Healing it would mint a new site id, record version 0.17, and leave 0.12-format clock
tables to be read as 0.17 — silent corruption instead of today's honest refusal. Rule 3 rejects it
(`__crsql_siteid`, its triggers and its clock tables all contain `crsql`), so v0.12.0 keeps failing
with the version-0 error, as today.

In general: **every cr-sqlite version that ever ran init to completion left a site-id table** (under
one of its names), and every CRR leaves a `__crsql_clock` table and `__crsql_*` triggers. A file with
none of those, an empty `crsql_master` and empty bookkeeping tables **holds no cr-sqlite state at all**
— so even in a case nobody has thought of, treating it as blank loses nothing. That is the real
argument: the predicate is safe not because we enumerated every old shape, but because it only admits
files with nothing to lose.

**A partially failed migration** cannot match either. A migration runs only after the version row has
been read, so the file already has a non-empty `crsql_master`. Future migrations will run inside the
**Atomic Init Transaction**, so they cannot half-commit.

**No new on-disk state.** The heal writes exactly what a fresh init writes: the version row, a fresh
site id, the site-id table, index and triggers. A wedged file never had a site id, so nothing that
synced can disagree with the new one.

**Logging.** cr-sqlite has no logger. The heal is silent at the SQLite level; hosts observe it as an
open that now succeeds. `silicon_brain`'s integration doc says how to detect and count heals.

### Init Busy Retry — option (c), recommended

Init cannot borrow the host's busy timeout, because it runs before the host can set one. Two ways to
let it wait:

- **(c1) Set a busy timeout during init and clear it afterwards** (`sqlite3_busy_timeout(db, N)`,
  then `sqlite3_busy_timeout(db, 0)`). On the auto-extension path no handler can exist yet, so clearing
  restores the exact prior state. On the `load_extension` path the host may already have installed a
  handler, and SQLite has **no API to read it back**, so clearing it silently removes the host's
  handler. ⛔ Rejected for that reason.
- **(c2) Retry internally, recommended.** Wrap the three points that can return `SQLITE_BUSY` on this
  path — the **Init Completeness Check** read, `BEGIN IMMEDIATE`, and `COMMIT` — in a bounded loop
  with `sqlite3_sleep` backoff (for example 1, 2, 4 … capped at 50 ms, with a total budget of about
  5 s). It never touches the connection's busy handler.

**Why retrying is safe here.** A failed `BEGIN IMMEDIATE` holds no lock, so waiting cannot deadlock. A
failed `COMMIT` holds `RESERVED`/`PENDING`. `PENDING` stops *new* readers, and the readers it waits on
are other connections' completeness checks, which finish in microseconds and do not themselves wait
while holding `SHARED`. That is why the check must finalize its statements before retrying.

**Risks.**

- **The open blocks for up to the budget** on a contended **fresh** file. On an initialized file the
  fast path never retries, so normal opens are unaffected. A host that opens on a latency-critical
  thread needs to know; Exqlite registers `open` as a dirty I/O NIF (`ERL_NIF_DIRTY_JOB_IO_BOUND`), which is where blocking belongs
  (verified in `deps/exqlite/c_src/sqlite3_nif.c`, 2026-11-13).
- **The budget is a compile-time constant.** A URI parameter could make it configurable
  (`sqlite3_uri_int64`). Not proposed: no host needs it yet.
- **It needs a binding.** `sqlite-rs-embedded`'s `sqlite_nostd` exposes `get_autocommit` but neither
  `sleep` nor `busy_timeout`. Either add a `sleep` binding to the `Infocorder-com/superfly_sqlite-rs-embedded`
  submodule — which, like the `vtab_collation` wrapper for Release B, must be **pushed before** the fork
  commit that records the new gitlink — or add a two-line C helper in `core/src/crsqlite.c` that calls
  `sqlite3_sleep` and is declared `extern` in Rust. **The C helper is recommended**: it avoids a
  submodule round-trip.

### Error paths, teardown and the rest of the extension

- **Every early return in `sqlite3_crsqlcore_init`** already frees ext data where it must. The
  `InitTxn` guard adds the rollback. Rolling back with prepared-but-not-stepped persistent statements
  (`crsql_newExtData`'s) outstanding is fine.
- **`crsql_finalize` / teardown and the E1 close hook** (`crsql_close_trace_hook`) are installed after
  init and only touch ext-data statements. Unaffected. A failed init returns before the hooks are
  installed, as today.
- **`crsql_automigrate`** is a SQL function a host calls after open. It does not run during init.
  Unaffected.
- **STRICT tables.** `crsql_tracked_peers` and `crsql_db_versions` are `STRICT`. DDL inside a
  transaction behaves identically, and the heal's row-count reads work on STRICT tables.
- **Static vs dynamic loading.** Static (auto-extension): init runs in `sqlite3_open_v2`, no host
  transaction possible, `BEGIN IMMEDIATE` path. Dynamic (`load_extension`): the host may be in a
  transaction, so the `SAVEPOINT` path is used. The host's double-init guard (static plus dynamic on
  one connection) is a separate failure and unchanged.
- **Read-only connections.** The fast path writes nothing, so a read-only open of an initialized file
  works as today. A read-only open of a fresh or wedged file fails, as today, now at `BEGIN IMMEDIATE`
  with `SQLITE_READONLY`.
- **`:memory:` and temp databases.** Private to one connection; the slow path always wins at once.
  Hosts that probe for static linkage with a `:memory:` open are unaffected.
- **WAL databases.** The slow path runs only on files that are not fully initialized. Those are
  normally in rollback mode, but if a WAL file ever needs it, `BEGIN IMMEDIATE` takes the WAL write
  lock and readers are not blocked.
- **libSQL build (`#ifdef LIBSQL`).** Same Rust path; no change.

### Keeping the re-open path read-only

The **Init Completeness Check** must reject exactly the files on which today's code would write, and
accept every other file. If it is too strict, healthy databases take the write lock on every open
(the rejected alternative above). If it is too loose, a file needing a write skips it. The test
**Re-open Writes Nothing** below pins this from both sides.

---

## Tests

### How this fork's tests run

- `cd core && make test` builds `dist/test` from `src/tests.c` and `src/*.test.c`, linking the Rust
  `integration_check` crate as a static library. `tests.c`'s `main` runs C suites, then
  `SUITE("rust_integration") crsql_integration_check()`, which runs the Rust suites listed in
  `core/rs/integration_check/src/lib.rs` (`t::automigrate`, `t::teardown`, …). One suite can be run on
  its own: `dist/test rust_integration`, `dist/test crsql`.
- `make valgrind` runs the same binary under valgrind. `make asan` needs nightly (`STABLE_RUST_PORT.md`).
- The test binary is compiled with **`-DSQLITE_THREADSAFE=0`**, with cr-sqlite registered as an
  auto-extension via `-DSQLITE_EXTRA_INIT=core_init`. So:
  - every `sqlite3_open` in a test runs init — a test that needs a connection **without** cr-sqlite
    must call `sqlite3_reset_auto_extension()`, open, then re-register with
    `sqlite3_auto_extension((void*)sqlite3_crsqlite_init)`. That is easiest from a C suite
    (`src/crsqlite.test.c` or a new `src/init-race.test.c`);
  - **threads are not allowed** in this binary. Contention must be built from **several connections
    on one thread**, which SQLite's unix VFS locks against each other as it would across threads.
- `py/correctness` (pytest, against `make loadable`) is the other harness. It already has the
  prior-version fixtures.

### Deterministic tests — in `make test`

Name them so a grep finds them all: prefix `init_race_`.

| test | setup | assertion |
|---|---|---|
| `init_race_heals_field_sample` | copy the preserved Half-Initialized Database (commit it as a fixture, e.g. `core/src/fixtures/halfinit.db`, 28 KB) to a temp file; open | open succeeds; version row = `CRSQLITE_VERSION`; one ordinal-0 site id; three triggers; unique index present. ⚠️ Fails today with the version-0 error — the red half of red→green |
| `init_race_heals_synthesized_residue` | plain connection writes the three tables with the **exact** DDL from `bootstrap.rs`, empty; close; open with cr-sqlite | same as above |
| `init_race_heal_refuses_v0_12` | copy `py/correctness/prior-dbs/v0.12.0.prior-db` | open still fails with the version-0 message; file bytes unchanged |
| `init_race_heal_refuses_v0_13_v0_15` | copy the other two fixtures | unchanged error (version 130000 / 150000); bytes unchanged |
| `init_race_heal_refuses_nonempty` | residue plus one row in `crsql_tracked_peers`; then separately one `config.x` row in `crsql_master` | open fails, as today |
| `init_race_heal_refuses_crsql_object` | residue plus an empty `foo__crsql_clock` table | open fails, as today |
| `init_race_heal_app_tables` | residue plus an application table with rows | heals (or fails, if the review picks the strict predicate) — pins whichever rule is chosen; app rows untouched |
| `init_race_atomic_at_each_step` | test-only fault injection (below) fails init after step *k*, for every *k* | open fails; afterwards `sqlite_master` contains **no** `crsql` object; next open succeeds |
| `init_race_loser_writes_nothing` | plain connection P holds `BEGIN IMMEDIATE` on a fresh file; open C with cr-sqlite | C fails with `SQLITE_BUSY` after the retry budget (measure the elapsed time against the bound); the file is still empty after P rolls back |
| `init_race_commit_busy` | P holds `SHARED` (`BEGIN; SELECT …`) on a fresh file; open C | C's `COMMIT` hits busy, retries, exhausts its budget (P cannot release on one thread), rolls back; file empty; after P ends, C's next open succeeds |
| `init_race_reopen_writes_nothing` | fully initialized file; open it **read-only** (`SQLITE_OPEN_READONLY`), and separately open read-write while P holds `BEGIN IMMEDIATE` | both opens succeed — proves the fast path takes no write lock |
| `init_race_host_txn_preserved` | only if `load_extension` is testable here — it is not, since the test binary sets `SQLITE_OMIT_LOAD_EXTENSION`. Cover in `py/correctness` instead: inside `BEGIN`, insert a row, `load_extension` on a wedged-but-unhealable file | the host's row is still there; the host's transaction is still open |

**Fault injection.** A `#[cfg(feature = "test")]` static (`INIT_FAIL_AT_STEP`), set by a test-only C
entry point, makes the persistent step with that index return `SQLITE_BUSY`. `integration_check`
already enables the `test` feature, and `test_exports.rs` is where such hooks live. It compiles to
nothing in release builds. A static is safe because the test binary is single-threaded.

**Mutation-verify the important ones.** With the `InitTxn` commit moved back to before step 5,
`init_race_atomic_at_each_step` must fail. With the heal predicate weakened to the naive rule,
`init_race_heal_refuses_v0_12` must fail.

### Concurrency stress test — separate from `make test`

The real race needs genuine parallelism, which the single-threaded test binary cannot provide. Two
places to run it:

1. **In this fork: a multi-process harness.** `make sqlite3` builds `dist/sqlite3` with cr-sqlite
   statically linked and auto-loaded. A small script (`tools/init_race_stress.sh`, Bash) does, for
   R rounds: create a fresh temp path, launch N `dist/sqlite3 <path> 'SELECT crsql_site_id();'`
   processes at once, wait, then make one quiet open and count failures. SQLite's file locking
   behaves the same across processes as across threads, so this reproduces the race faithfully. Run it
   **before** the fix on the current `main` to confirm it reproduces (the target is ≥ 1 wedge in 1,500
   rounds of 10, as the app's probe found), then after: **zero wedges, and — with the Init Busy Retry
   — zero failed opens.** Keep it out of `make test` (it is slow and timing-dependent); run it by hand
   and record the numbers in this doc.
2. **In `silicon_brain`:** the probe, folded into a durable app test that exercises the real NIF and
   Exqlite's threads. That belongs to the integration doc.

### What to run

```
cd core
make test                      # all suites; or: dist/test rust_integration / dist/test crsql
make valgrind                  # leak check
make loadable && (cd ../py/correctness && pytest -k "prior or init_race")
make sqlite3 && ../tools/init_race_stress.sh 1500 10   # by hand, before and after
```

---

## Release and versioning

Following Release A, A1 and Release B (`CRSQL_CHANGES_TABLE_FILTER_PUSHDOWN.md`, `STABLE_RUST_PORT.md`,
and the coordination doc's **Pin this** sections):

- **No tag.** This fork publishes `refs/heads/main` only. Downstream pins a full commit SHA
  (`CRSQLITE_REF` in `silicon_brain` `scripts/build_crsqlite_static.sh`) and fetches it with
  `git fetch --depth 1 origin <SHA>`.
- **One commit** on `main` (or two, if the stress script lands separately), stable Rust 1.95.0 as
  pinned in `rust-toolchain.toml`.
- **Append `e4-init-atomic` to `BUILD_ID`** in `core/rs/core/src/sha.rs`, and document it there and
  in the README's token table:
  `e1-fd-close+e2-tbl-pushdown+e3-tbl-collate-gate+e4-init-atomic <sha>`. Hosts gate with a substring
  match, so existing gates on `e2-tbl-pushdown` keep working.
- **If the Init Busy Retry uses a submodule binding** rather than the recommended C helper, push the
  `superfly_sqlite-rs-embedded` commit first (Release B's blocking handoff).
- **README:** add a change 4 under "Infocorder fork", with its benefit and its cost (opens of a
  contended fresh file can now block up to the retry budget; a Half-Initialized Database now opens).
- **Coordination doc:** when built, append a **Pin this** section with SHA, verification and the
  downstream steps. ⚠️ This repo's copy of `docs/CRSQLITE_FORK_UPGRADE_COORDINATION.md` is
  **git-ignored** and is currently a **stale prefix** of `silicon_brain`'s copy (identical for its first
  91,879 bytes; the app copy has 19 more lines, the downstream A2 pickup). Copy the app's version over
  this one before appending, then copy the result back.
- **Verification to record:** `make test` and `make valgrind` green; `py/correctness` green; the stress
  numbers before and after; the `crsql_build_id()` output.

---

## Risks

- **A too-strict Init Completeness Check** sends healthy databases down the slow path. Every open then
  takes the write lock: opens serialize, and read-only opens fail. Guarded by
  `init_race_reopen_writes_nothing`.
- **A too-loose heal predicate** silently re-initializes a genuine database with a new site id. That
  forks its identity, and every peer would see a new site. Guarded by the prior-db refusal tests. The
  argument for safety rests on the predicate admitting only files with no cr-sqlite state; keep rule 3
  name-based and broad (`%crsql%`), not a list of known names.
- **Blocking opens.** With the Init Busy Retry, an open on a contended fresh file can block for the
  whole budget. Initialized files never retry.
- **A held `SHARED` lock across the slow path** — a completeness-check statement not finalized — makes
  the connection's own `COMMIT` wait on itself until the budget runs out. A code-review item, and
  `init_race_commit_busy` exercises the path.
- **Hosts that relied on the noise.** A host that treated a first failed open as normal changes
  nothing; one that parses the version-0 message to detect wedged files will stop seeing it.
- **Drift.** Another local change to init, which upstream will not have. `lib.rs` init is upstream's
  hottest file for merges; keep the diff confined to `bootstrap.rs` plus the guard in `lib.rs`.
- **Fleet mix.** A device still running an older build can wedge a new file until it upgrades. The
  heal then rescues that file the first time an upgraded build opens it. Rollout belongs to the
  integration doc.

---

## Why an integration document is needed downstream

Adopting this release in `silicon_brain` is **not** a plain pin bump, so it has its own document,
`docs/CRSQLITE_INIT_RACE_FIX_INTEGRATION.md` in that repo. In brief, the reasons are:

- the app ships cr-sqlite through **three** build routes with **separate** pins (the static NIF
  script, the `sqlite3mc` (SQLite3 Multiple Ciphers) spike build script, still pinned to `0eb2f84e`, and a superfly prebuilt
  runtime `.so` fallback, which will **never** carry this fix);
- the CI marker and the hub deploy gate both key on `e2-tbl-pushdown` and would accept a build without
  the fix;
- one production path — the spoke's first-boot `parking.db` — **is** exposed;
- and the app hides the symptom: its test helper filters these log lines, and a code comment calls them
  spurious.

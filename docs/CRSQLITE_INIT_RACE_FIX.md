# Making cr-sqlite's per-connection init atomic — the init race wedge

**Status:** 📋 **Plan, not built** (2026-11-13; revised the same day after an adversarial review and
the operator's rulings). Docs only; no code has changed.
**Area:** extension init — `core/rs/core/src/lib.rs` `sqlite3_crsqlcore_init`,
`core/rs/core/src/bootstrap.rs`, `core/src/ext-data.c`, and a new auto-extension entry point in
`core/src/crsqlite.c` registered from `core/src/core_init.c`.
**Compatibility:** no wire or changeset format change. One new exported C symbol (the auto-extension
entry point of **Init Busy Timeout**); existing symbols are unchanged. On-disk format is unchanged for
every database that opens today. The behaviour changes are that a **Half-Initialized Database** — which
today can never be opened — opens, and that an open through the new entry point waits for a competing
connection instead of failing.
**Downstream integration:** `silicon_brain` `docs/CRSQLITE_INIT_RACE_FIX_INTEGRATION.md` (pin bump,
startup shim change, static NIF rebuilds, fleet rollout, app tests, Litestream). Release logistics go in
the shared `docs/CRSQLITE_FORK_UPGRADE_COORDINATION.md` when the release is built.
**Rulings** (operator, 2026-11-13; recorded in `silicon_brain` `docs/DECISION_REGISTER.md`): the fix
lives in this fork, with no app-side guard (**Init Race Fork-Only Fix**); the heal's scope is broad
(**Init Residue Heal**); the busy handling ships as a busy timeout set by a separate auto-extension entry
point (**Init Busy Timeout**).

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
"if the file is busy, wait a bit" (a busy timeout). So when several connections open the
**same brand-new file at the same moment** — which is exactly what a connection pool does when it starts —
they collide. Usually the collision is harmless: one connection finishes setup, the others fail their
open, and the pool retries them a moment later. The same missing busy timeout also fails some opens of
files that are **already** set up, because even init's reads can meet a lock.

But one interleaving is not harmless. A connection can commit the `crsql_master` table, and then fail
to write the version row into it because another connection holds the lock (`SQLITE_BUSY`). Its open
fails, and it leaves behind a file with an **empty** `crsql_master`. From then on, every open of that
file — by anyone, forever — sees that `crsql_master` exists, concludes "this is an existing database",
looks for the version row, finds none, treats the version as 0, and refuses with *"Opening a db created
with cr-sqlite version 0 is not supported."* The file can never be opened again until someone deletes
it. We call this the **Init Race Wedge**, and the file it leaves behind a **Half-Initialized Database**.
A single connection can leave the same file without any race, if anything stops it between those two
commits: a crash or kill, a full disk, an I/O error.

What a user sees:

- **A fresh install whose database never opens.** On a new desktop install, a database the app
  creates at first boot (on Infocorder's spokes this is `parking.db`) is opened by a pool of
  connections. If the race lands badly, that file fails to open on every start, with a log full of
  `database_open_failed`. Nothing the user does fixes it; deleting a file they have never heard of
  does.
- **A new project that is "broken" from birth.** Any path that creates a database file and lets a
  multi-connection pool open it before one connection has finished setup can leave a project whose
  database can never be opened. No data is lost — the file holds no cr-sqlite state — but the project
  is unusable.
- **Unexplained "harmless" noise.** The losing side of the same race is the `Exqlite.Connection …
  failed to connect: database_open_failed` line that Infocorder has logged at every test boot for
  months, and that its test helper filters out. It was believed harmless. It is harmless only when the
  winner finishes.
- **A test suite that "breaks for no reason".** Infocorder recorded a leftover test metadata database
  holding only cr-sqlite's tables that made every later test run fail until it was deleted
  (`silicon_brain` `docs/IMPROVEMENT_IDEAS.md` → **Stale Test Metadata Database**). That file is a
  Half-Initialized Database.

The evidence (Infocorder, 2026-11-13):

- A preserved Half-Initialized Database (28,672 bytes, rollback `delete` journal mode, file change
  counter 3 — i.e. exactly three commits) holds `crsql_tracked_peers`, `crsql_db_versions` and an
  **empty** `crsql_master`, and nothing else: no `crsql_site_id`, no application tables. Read-only
  inspection of a copy confirms it. It is now in this repo as `core/src/fixtures/halfinit.halfinit-db`.
- A probe opened 10 connections concurrently on a fresh file, 1,500 times, using a statically linked
  build of this fork (`6cf12654`). **Every one of the 1,500 rounds** had at least one failed open, and
  **5 of the 1,500 files were left permanently unopenable.** A reviewer's rerun confirmed that a wedged
  file keeps exactly the field shape and stays wedged on a second open.
- The reviewer (static NIF `6cf12654`) measured 10 concurrent opens in three cases, and two of them
  are files that are **already initialized**: about **91%** failed on a fresh file, about **3%** on an initialized WAL file at
  pool start, and about **41%** on an initialized rollback-journal file while a pool-mate
  switches it to WAL. So the missing busy timeout is not confined to fresh files.

This plan makes the fix in three parts, each with its own name:

1. **Atomic Init Transaction** — all of init's writes happen in one transaction, so a file is either
   untouched or fully initialized. This stops new wedges, from races and from interrupted single
   connections alike.
2. **Init Residue Heal** — a file that is already a Half-Initialized Database is recognised, by a
   test that admits only files with no cr-sqlite state, and initialized as if it were new. This rescues
   files wedged by earlier builds.
3. **Init Busy Timeout** — when cr-sqlite is registered as an auto-extension, init runs under a short
   busy timeout that it clears afterwards, so an open waits for a competing connection instead of
   failing at once — on fresh and initialized files alike.

It also fixes three smaller defects found on the way: a missing unique index that can never be
recreated (**Site-ID Index Gap**), error paths that issue a bare `ROLLBACK` and so can abort a
transaction that belongs to the host (**Init Host Rollback**), and error paths that leak or
double-free init's per-connection state (**Init Error-Path Frees**).

---

## Summary

| part | what it changes | status |
|---|---|---|
| **Atomic Init Transaction** | wraps every persistent write of init in one `BEGIN IMMEDIATE` … `COMMIT`, entered only when a read-only check says the file needs initializing, and committed before any registration that carries a destructor | ✅ required — this is the fix |
| **Init Residue Heal** | treats a data-free Half-Initialized Database as a blank slate, application tables allowed | ✅ required; scope ruled broad |
| **Init Busy Timeout** | a new auto-extension entry point sets `sqlite3_busy_timeout` before init and clears it after; nothing changes on the `load_extension` path | ✅ ruled: ship it |
| **Init Host Rollback** | error paths roll back to their own savepoint instead of issuing a bare `ROLLBACK` | ✅ required (falls out of the transaction rewrite) |
| **Init Error-Path Frees** | one owner for the ext data and the site-id buffer at every point of init | ✅ required (falls out of the transaction rewrite) |
| `crsql_build_id()` token | appends `e4-init-atomic` | ✅ required, so hosts can gate on the fix |

**Recommendation:** ship all of them together as one release (`e4-init-atomic`).

---

## The mechanism — verified against this fork's source

### The init sequence

`sqlite3_crsqlite_init` (`core/src/crsqlite.c`) calls `sqlite3_crsqlrustbundle_init`
(`core/rs/bundle/src/lib.rs`), which calls `sqlite3_crsqlcore_init` (`core/rs/core/src/lib.rs`). After
registering some functions and virtual-table modules, its persistent work is, in order (each item is
named by its function, and the rest of this document refers to it by that name):

- **`crsql_init_peer_tracking_table`** (`bootstrap.rs`) — `CREATE TABLE IF NOT EXISTS
  crsql_tracked_peers (…) STRICT`.
- **`crsql_init_db_versions_table`** — `CREATE TABLE IF NOT EXISTS crsql_db_versions (…) STRICT`.
- **`crsql_maybe_update_db`**:
  - `has_table(db, "crsql_master")` is computed **first**, outside any transaction (`has_schema_table`).
  - **`crsql_create_schema_table_if_not_exists`** opens `SAVEPOINT crsql_create_schema_table`, runs
    `CREATE TABLE IF NOT EXISTS "crsql_master" ("key" TEXT PRIMARY KEY, "value" ANY)`, and `RELEASE`s
    it. A savepoint opened outside a transaction behaves as `BEGIN DEFERRED`, so the `RELEASE`
    **commits** `crsql_master` immediately.
  - **the version write** — `SAVEPOINT crsql_maybe_update_db`; `maybe_update_db_inner(db,
    !has_schema_table, …)`: on a blank slate it writes `INSERT OR REPLACE INTO crsql_master VALUES
    ('crsqlite_version', 170000)`; otherwise it reads the row, treats a missing row as version 0, and
    fails with *"Opening a db created with cr-sqlite version 0 is not supported"* when it is below
    `CRSQLITE_VERSION_0_17_0` (`consts.rs`; today `CRSQLITE_VERSION` equals it, 170000). On success
    `RELEASE` (commit); on failure a bare `ROLLBACK`.
- **`crsql_newExtData`** (`core/src/ext-data.c`) prepares statements that read `crsql_db_versions` and
  `crsql_master`, reads the `config.%` rows of `crsql_master`, and steps `PRAGMA data_version`. Then
  `crsql_update_site_id` is registered with the ext data and **no destructor** — it must be, before
  **`crsql_init_site_id`**, because inserting into an existing `crsql_site_id` prepares its triggers,
  and the triggers call that function.
- **`crsql_init_site_id`** → `init_site_id`: if there is no `crsql_site_id` table,
  `create_site_id_and_site_id_table` runs `CREATE TABLE "crsql_site_id" (…)` (no `IF NOT EXISTS`)
  and `CREATE UNIQUE INDEX crsql_site_id_site_id …` through one `exec_safe` call — which is
  `sqlite3_exec`, so **each statement commits separately** — and then `insert_site_id` inserts
  ordinal 0. If the table exists, it reads the ordinal-0 row, inserting one if it is missing.
- **`crsql_initSiteIdExt`** takes ownership of the site-id buffer (`ext_data->siteId`) and prepares
  statements against `crsql_site_id`.
- **`create_site_id_triggers`** — three `CREATE TRIGGER IF NOT EXISTS`, again each committed
  separately.

After that, `sqlite3_crsqlcore_init` registers `crsql_site_id`, then
**`crsql_db_version` with the destructor `x_free_connection_ext_data`**, then the remaining functions. The commit and rollback hooks
are installed by `sqlite3_crsqlite_init` only after `sqlite3_crsqlcore_init` returns, so init's own
writes never reach `crsql_commit_hook`.

**On a fully initialized database, this sequence writes nothing.** `CREATE TABLE IF NOT EXISTS` and
`CREATE TRIGGER IF NOT EXISTS` on existing objects, and an empty `SAVEPOINT`/`RELEASE`, all succeed on a
**read-only** connection (checked with the stock 3.50.2 shell, 2026-11-13). This property must survive
the fix; see **Keeping the re-open path read-only**.

### Why concurrent opens race

- With static linking the host registers `sqlite3_crsqlite_init` as an auto-extension
  (`core/src/core_init.c` does the same for this repo's CLI and tests; `silicon_brain` uses its own
  startup shim), so init runs **inside** `sqlite3_open_v2`. The host cannot call `sqlite3_busy_timeout`
  until the open returns, so init has **no busy handler**: any lock conflict is an immediate
  `SQLITE_BUSY`.
- A brand-new file is in rollback-journal (`delete`) mode during init, because hosts set
  `journal_mode=WAL` with a pragma **after** the open. In rollback mode a commit needs an `EXCLUSIVE`
  lock, which any other connection's `SHARED` lock (even one reading `sqlite_master` in `has_table`)
  blocks.
- `crsql_init_peer_tracking_table`, `crsql_init_db_versions_table`,
  `crsql_create_schema_table_if_not_exists`, the version write, `create_site_id_and_site_id_table`
  (two statements), `insert_site_id` and `create_site_id_triggers` (three statements) are all separate
  transactions, so other connections observe — and act on — every intermediate state.

### Why opens of initialized files fail too

The fast path — an initialized file, on which init writes nothing — still reads, with no busy handler:
`has_table`, the version read, `crsql_newExtData`'s `config.%` loop and its `PRAGMA data_version`
step, and `init_site_id`'s ordinal-0 read. Any of them meets `SQLITE_BUSY` while another connection
holds a conflicting lock: in rollback mode, a pool-mate committing or switching the file to WAL; in WAL,
recovery or a checkpoint that resets the WAL. The reviewer's 3% and 41% figures are these. Most of them
fail the open cleanly, but one does not:

- ⛔ **`crsql_newExtData`'s `config.%` loop treats `SQLITE_BUSY` as end-of-rows** (`while
  (sqlite3_step(pStmt) == SQLITE_ROW)`), so `mergeEqualValues` silently falls back to its default of 0
  and the open **succeeds with the wrong setting** on a database that set `merge-equal-values`. The fix
  checks the final step result and fails the open on anything other than `SQLITE_DONE`. (Its
  "broken setting" early return also leaks the statement; finalize it.)

A failed open matters to hosts even when the pool retries it: `silicon_brain`'s `boot.ex` documents a
project whose single start-up probe hit `database_open_failed`, so it never reported ready and its
activity pipeline never started.

### The failure modes

| failure | how it happens | result |
|---|---|---|
| **Init Race Wedge** (race) | connection B commits `crsql_master` in `crsql_create_schema_table_if_not_exists`, then its version write fails `SQLITE_BUSY`; every other connection already saw `crsql_master` and so took the "existing database" branch | **permanent**: no connection ever writes the version row again |
| **Init Race Wedge** (single connection) | anything stops the only connection between the `crsql_master` commit and the version write: a crash or kill, `SQLITE_FULL` on a nearly full disk at first boot, an I/O error (for example Windows antivirus holding the file) | **permanent**, as above. Rare, but hosts that prime a file with one connection to avoid the race are still exposed to it |
| transient version-0 refusal | A computes `has_schema_table = true` in the window between B's two commits, finds no version row, refuses | A's open fails; B finishes; pool retry succeeds |
| transient site-id collision | A and B both see no `crsql_site_id`; B creates it; A's un-guarded `CREATE TABLE` fails "already exists" — or A's ordinal-0 `INSERT` hits B's row | A's open fails; retry succeeds. Two different site ids can never both be stored: ordinal is the primary key |
| transient fast-path failure | a read on an initialized file meets a lock (**Why opens of initialized files fail too**) | the open fails, or — for the `config.%` loop — succeeds with a default setting |

`has_schema_table` being computed before the savepoint is what turns the third row into the first: a
connection that saw the half-state never writes the row, and the one connection that would have
written it failed.

### Three smaller defects in the same code

- **Site-ID Index Gap.** If `CREATE TABLE crsql_site_id` commits and `CREATE UNIQUE INDEX
  crsql_site_id_site_id` then fails, the table persists without its index. Every later open sees the
  table and never creates the index. The table then accepts duplicate `site_id` values, which the
  ordinal lookup (`SELECT ordinal FROM crsql_site_id WHERE site_id = ?`) does not expect. Not
  observed in the field; it is a property of the code.
- **Init Host Rollback.** `crsql_create_schema_table_if_not_exists` and `crsql_maybe_update_db`
  recover from an error with a bare `ROLLBACK`, not `ROLLBACK TO <savepoint>`. During an
  auto-extension open there is no outer transaction, so this is equivalent. But when a host calls
  `load_extension` inside its own transaction, a failing init **rolls back the host's transaction**.
- **Init Error-Path Frees.** The failure paths of `sqlite3_crsqlcore_init` disagree about who owns the
  ext data and the site-id buffer:
  - **`create_site_id_triggers` failing** frees `site_id_buffer` — which `ext_data->siteId` already
    points at — and leaks `ext_data` and its prepared statements. On the `load_extension` path the
    connection survives, and `crsql_update_site_id` stays registered with that leaked `ext_data`, whose
    site id now dangles. This failure is reachable today: the triggers are created outside any
    transaction, so a race can fail them with `SQLITE_BUSY`.
  - **Any failure after `crsql_db_version` is registered** still calls `crsql_freeExtData`, but that
    registration carries the destructor `x_free_connection_ext_data`, which frees the same ext data
    again when the connection closes — a **double free** at close. SQLite also runs the destructor when
    the registration itself fails, so the failure path of the `crsql_db_version` registration frees it
    twice immediately. Hosts close a connection whose open failed (Exqlite calls `sqlite3_close_v2`),
    so this reaches production on any late failure.
  - **Functions registered with the ext data and no destructor** (`crsql_update_site_id`,
    `crsql_site_id`) keep a pointer to it after a later failure frees it. On the `load_extension` path
    they are callable afterwards: a use-after-free.

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
   ⛔ Any step result other than `SQLITE_ROW`/`SQLITE_DONE` is an **error that fails the open** — never
   "incomplete" and never "complete". The check's statements must be
   **finalized before the slow path starts**: a statement left un-reset keeps a `SHARED` lock and would make the slow path wait on
   itself.
2. **Slow path.** `BEGIN IMMEDIATE`. Then **re-run the same check under the lock** (double-checked:
   another connection may have finished while this one waited). Then run the existing sequence, from
   `crsql_init_peer_tracking_table` through `create_site_id_triggers`, in its current order — it is
   already idempotent, except `create_site_id_and_site_id_table`'s `CREATE TABLE`, which `has_table`
   guards and which is now race-free because the lock is held. Then `COMMIT`.

**Where the transaction ends.** `COMMIT` **immediately after `create_site_id_triggers`**, before
`crsql_site_id`, `crsql_db_version` or any other function is registered. Everything persistent is
inside; every registration that carries a destructor is outside. That boundary is what makes the error
paths simple (**Init Error-Path Frees** below): before it, a failure rolls back and frees by hand;
after it, nothing persistent can be undone and ownership has passed to SQLite.

`BEGIN IMMEDIATE`, not a deferred `SAVEPOINT`, because:

- it takes the `RESERVED` lock before reading, so the slow-path reads and writes see one consistent
  state, and `has_schema_table` can no longer be computed against a state that changes before the write;
- a deferred transaction that reads under `SHARED` and then upgrades to `RESERVED` can be refused with
  `SQLITE_BUSY` **without** the busy handler being invoked (SQLite's deadlock avoidance), so it cannot
  be made to wait.

**If the host is already inside a transaction** (`sqlite3_get_autocommit(db) == 0` — only possible on
the `load_extension` path), use `SAVEPOINT crsql_init` instead and roll back with
`ROLLBACK TO crsql_init; RELEASE crsql_init`. That fixes **Init Host Rollback** too. ⛔ Never wait or
retry inside a host's transaction: a write there may be refused by deadlock avoidance, and the only
correct response is to roll back to the savepoint and fail the load.

**Everything persistent joins the transaction**, including the peer and db-version tables, the
site-id table, its unique index, its row and its triggers. Leaving any of them out recreates a smaller
version of the same problem: a committed table with missing companions. Joining costs nothing — a
fresh init is a handful of DDL statements under one lock. It also closes the **Site-ID Index Gap** for
every database initialized from now on.

**What a losing connection sees.** It reads, finds the file incomplete, finalizes, and requests
`BEGIN IMMEDIATE`. With the **Init Busy Timeout** it waits, the re-check under the lock finds the
winner's committed work, and it proceeds without writing anything. Without one (the `load_extension`
path with no host handler), `SQLITE_BUSY` fails its open — **cleanly**: it wrote nothing and holds no
lock, and the pool retries.

**What happens if the winner is interrupted** (error, `SQLITE_BUSY` on `COMMIT` after the timeout,
process killed, disk full): the transaction rolls back, or the hot journal is rolled back by the next
opener. The file returns to untouched. That is the property this whole plan exists for, and it is what
closes the single-connection wedge as well as the race.

**`COMMIT` can itself return `SQLITE_BUSY`** in rollback mode, while another connection still holds
`SHARED` from its completeness check. SQLite invokes the busy handler while it waits for those readers,
so under the **Init Busy Timeout** the `COMMIT` waits. If it still fails, init `ROLLBACK`s and fails
the open.

**Implementation sketch.** The transaction spans `lib.rs`, because `crsql_newExtData` and the
`crsql_update_site_id` registration sit between `crsql_maybe_update_db` and `crsql_init_site_id`. Use
one RAII guard (`InitTxn`), created before `crsql_init_peer_tracking_table` on the slow path, whose
`Drop` rolls back unless `commit()` was called, and call `commit()` right after
`create_site_id_triggers`. Every early `return null_mut()` before that point then rolls back
automatically. Wherever this code compares a result with `SQLITE_BUSY`, compare `rc & 0xff`, so the
extended codes (`SQLITE_BUSY_RECOVERY`, `SQLITE_BUSY_SNAPSHOT`, `SQLITE_BUSY_TIMEOUT`) are treated as
busy too.

**Rejected alternatives.**

- *Move only `crsql_create_schema_table_if_not_exists` inside the `crsql_maybe_update_db` savepoint.*
  Removes the wedge's exact interleaving, but not `has_schema_table` being read outside the lock, not
  the site-id collisions, and not the **Site-ID Index Gap**. It is also still a deferred savepoint that
  cannot wait.
- *`BEGIN IMMEDIATE` on every open, with no completeness check.* Makes every read-write open of every
  database — including a healthy WAL database under a long write transaction — take the write lock.
  Opens would serialize and wait on any active writer. That is a regression for every host, not only
  racing ones. (On a read-only connection SQLite downgrades `BEGIN IMMEDIATE` to a read transaction,
  so read-only opens would not fail; they gain nothing either.)

### Init Error-Path Frees — one owner at every point

With the transaction ending before the destructor-carrying registrations, the ownership rules are:

- **Before `crsql_initSiteIdExt`**: a failure frees `site_id_buffer` (if allocated) and calls
  `crsql_freeExtData` (if allocated), separately.
- **From `crsql_initSiteIdExt` until the `crsql_db_version` registration is attempted**: `ext_data`
  owns the buffer. A failure calls `crsql_freeExtData` only — never a bare free of the buffer. This is
  the fix for the `create_site_id_triggers` path.
- **From the `crsql_db_version` registration on** (success or failure): SQLite owns `ext_data` through
  `x_free_connection_ext_data`. A failure returns without freeing anything.
- **Functions registered with `ext_data` and no destructor** must not outlive it. On every failure
  path that frees `ext_data` by hand, first delete them (`sqlite3_create_function_v2` with the same
  name and arity and a NULL implementation), so the `load_extension` path leaves no dangling function.

The fault-injection test below covers each of these points, and must pass under `make valgrind`.

### Init Residue Heal — option (b), scope ruled broad

The **Atomic Init Transaction** prevents new wedges. It does not rescue a file wedged by an earlier
build: `has_schema_table` is true and there is no version row, so the version-0 refusal still fires.

**The heal predicate.** Inside the slow path (so under the lock), treat the database as a blank slate
— `is_blank_slate = true` — when **all** of these hold:

1. `crsql_master` exists and has **zero rows** — not merely "no `crsqlite_version` row". (`crsql_master`
   also stores `config.*` rows written by `crsql_config_set`; a file with any row was opened
   successfully at least once.)
2. `crsql_tracked_peers` and `crsql_db_versions`, if they exist, have zero rows.
3. **No other cr-sqlite object exists.** No table, index, trigger or view in `sqlite_master` whose name
   contains `crsql`, other than those three tables and their `sqlite_autoindex_*` indexes. Match with
   `instr(lower(name), 'crsql') > 0`, **not** `LIKE '%crsql%'`: `LIKE`'s case sensitivity follows
   `PRAGMA case_sensitive_like`, which a host may have set on its connection before a `load_extension`
   (or the `SQLITE_CASE_SENSITIVE_LIKE` compile option), while `instr` over `lower()` does not change.

**Scope: broad (ruled 2026-11-13).** Rule 3 constrains only cr-sqlite objects; application tables may
be present. A Half-Initialized Database produced by a pool race contains no application tables (the
field sample contains none). But a file first created by a plain SQLite connection — or one whose
cr-sqlite state a host stripped before re-initializing it, as `silicon_brain`'s clone does — and then
raced or interrupted would have them. For that file, the blank-slate path is exactly what would have
happened had init not been interrupted: with no clock tables and no CRR triggers there is nothing to
migrate.

**Why it cannot misfire on a genuine older database.** This fork ships three databases from earlier
cr-sqlite versions in `py/correctness/prior-dbs/` (inspected read-only, 2026-11-13):

| fixture | `crsql_master` | site-id table | other cr-sqlite objects |
|---|---|---|---|
| `v0.12.0.prior-db` | exists, **empty** | `__crsql_siteid` | `foo__crsql_*trig`, `bar__crsql_*trig`, clock tables |
| `v0.13.0.prior-db` | `crsqlite_version = 130000` | `__crsql_siteid` | triggers, clock tables |
| `v0.15.0.prior-db` | `crsqlite_version = 150000` | `__crsql_siteid` + index | triggers, clock tables |

None of the three has `crsql_db_versions`.

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

**No new on-disk state, and no identity replaced.** The heal writes exactly what a fresh init writes:
the version row, a fresh site id, the site-id table, index and triggers. A file the predicate admits
holds no site-id table under any name and no clocks, so no site id is being replaced; under the broad
scope it may hold application rows, but cr-sqlite has no record of them, exactly as for a plain file
opened with cr-sqlite for the first time.

**Logging.** cr-sqlite has no logger. The heal is silent at the SQLite level; hosts observe it as an
open that now succeeds. `silicon_brain` decided not to count heals (**Heal Count Diagnostic**, in its
register); its integration doc lists what it observes instead.

### Init Busy Timeout — option (a), ruled

Init cannot borrow the host's busy timeout on the auto-extension path, because it runs before the host
can set one.

**The mechanism.** Add a second exported entry point to `core/src/crsqlite.c` — provisionally
`sqlite3_crsqlite_autoext_init`, with the auto-extension signature — that:

1. calls `SQLITE_EXTENSION_INIT2(pApi)`;
2. calls `sqlite3_busy_timeout(db, CRSQL_INIT_BUSY_TIMEOUT_MS)` **before any init step**;
3. calls `sqlite3_crsqlite_init(db, pzErrMsg, pApi)` and keeps its result;
4. calls `sqlite3_busy_timeout(db, 0)` — on success **and** on failure — and returns the result.

Register it **only** as an auto-extension: `core/src/core_init.c` here, and `silicon_brain`'s own
startup shim (`monkeypatched_deps/exqlite_crsqlite_init.c`), which that app links instead of
`core_init.c`. `sqlite3_crsqlite_init` itself is unchanged and stays the `load_extension` entry point.

**Why clearing is safe there.** An auto-extension runs inside `sqlite3_open_v2`, before the open
returns a handle, so the host cannot have installed a busy handler yet; setting the timeout and then
clearing it to 0 restores the exact prior state. (A different auto-extension registered **earlier**
could in principle have installed one, which clearing would remove. Neither this repo's builds nor
`silicon_brain`'s register any.) On the `load_extension` path the host may already have a handler, and
SQLite has **no API to read it back**, so that path must not touch it — which is why this is a separate
entry point and not a change to `sqlite3_crsqlite_init`.

**What it covers.** Every lock wait of an auto-extension init: the **Init Completeness Check** and the
other fast-path reads (**Why opens of initialized files fail too**), `BEGIN IMMEDIATE`, and `COMMIT`.
SQLite's own busy handler does the waiting, so init needs no sleep, no loop and no new binding.

**The `load_extension` path.** Init sets no timeout and adds no retry. The host's handler, if any,
applies to every wait above — `silicon_brain`'s Exqlite connections set `busy_timeout` before
`load_extensions`. A host with no handler gets an immediate, clean `SQLITE_BUSY`. Inside a host
transaction, init never waits or retries (**Atomic Init Transaction**).

**The budget, `CRSQL_INIT_BUSY_TIMEOUT_MS` = 2000.** A compile-time constant. Correctness does not
depend on it — an open that times out fails cleanly and leaves the file untouched — so it only has to
cover real contention while staying well inside every caller's deadline:

- **Real waits are milliseconds**: a fresh init is a handful of DDL statements and one commit, and a
  pool-mate's WAL switch is one more. 2 s leaves room for a slow `fsync` or a Windows antivirus scan
  of a new file.
- **`silicon_brain` callers**: its Exqlite build sets `-DHAVE_USLEEP=1`, so SQLite's busy handler
  backs off in 1 ms to 100 ms steps; 2000 ms matches Exqlite's own default `busy_timeout`; it is below
  the spoke boot's 3 s per-project floor (`Boot`'s `@per_project_budget_floor_ms`) and far below the
  hub's 90 s `ProjectDbManager.ensure_repo/1` call and 60 s provisioning lock. Burrito's own
  unpacking happens before the BEAM starts and is not affected.
- **Exqlite runs `open` as a dirty I/O NIF** (`ERL_NIF_DIRTY_JOB_IO_BOUND`, `deps/exqlite/c_src/
  sqlite3_nif.c`), so a waiting open occupies a dirty I/O scheduler, never a normal one. Ten opens
  waiting at once could occupy the default ten dirty I/O schedulers for up to the budget; a larger
  budget would make that stall longer.

⚠️ **This fork's own builds lack `HAVE_USLEEP`.** Without it, SQLite's busy handler sleeps in whole
seconds and gives up at once when the timeout is under 1 s. Add `-DHAVE_USLEEP=1` to the flags of the
test binary and `dist/sqlite3` (as `silicon_brain`'s Exqlite build has), so this repo's tests and stress
script measure the same waiting behaviour hosts get.

**Test-only override.** Under `UNIT_TEST` (already defined for the test binary), expose a setter for
the budget so the deterministic tests that block on purpose do not each wait 2 s. It compiles to
nothing in release builds.

**Why not an internal retry.** An earlier version of this plan retried `BEGIN IMMEDIATE` and `COMMIT`
in a bounded loop with `sqlite3_sleep`, precisely to avoid touching the busy handler. It protected only
those two points — not the fast-path reads, which the measurements showed failing on initialized files
— and needed a sleep binding or a C helper. With the timeout set by a separate auto-extension entry
point, SQLite's own handler covers every read and write, and neither the retry loop nor the C helper is
needed.

### Error paths, teardown and the rest of the extension

- **Every early return in `sqlite3_crsqlcore_init`** follows the ownership rules in
  **Init Error-Path Frees**, and the `InitTxn` guard adds the rollback. Rolling back with prepared-but-not-stepped
  persistent statements (`crsql_newExtData`'s) outstanding is fine.
- **`crsql_finalize` / teardown and the E1 close hook** (`crsql_close_trace_hook`) are installed after
  init and only touch ext-data statements. Unaffected. A failed init returns before the hooks are
  installed, as today.
- **`crsql_automigrate`** is a SQL function a host calls after open. It does not run during init.
  Unaffected.
- **STRICT tables.** `crsql_tracked_peers` and `crsql_db_versions` are `STRICT`. DDL inside a
  transaction behaves identically, and the heal's row-count reads work on STRICT tables.
- **Static vs dynamic loading.** Static (auto-extension): init runs in `sqlite3_open_v2`, no host
  transaction possible, `BEGIN IMMEDIATE` path, under the **Init Busy Timeout** when registered through
  the new entry point. Dynamic (`load_extension`): the host may be in a transaction, so the `SAVEPOINT`
  path is used, under the host's handler. The host's double-init guard (static plus dynamic on one
  connection) is a separate failure and unchanged.
- **Read-only connections.** The fast path writes nothing, so a read-only open of an initialized file
  works as today. A read-only open of a fresh or wedged file fails, as today, and cleanly: SQLite
  downgrades `BEGIN IMMEDIATE` on a read-only connection to a read transaction (checked with the stock
  3.50.2 shell, 2026-11-13), so the failure is `SQLITE_READONLY` at init's first write, which rolls
  back.
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
`init_race_reopen_writes_nothing` below pins this. ⚠️ Only its read-write half can detect an
over-strict check: on a read-only connection `BEGIN IMMEDIATE` becomes a read transaction, the re-check
finds the file complete, and the open succeeds whether or not the check was too strict.

### Litestream and other WAL-reading tools

Nothing here changes what an external WAL reader such as Litestream sees: fast-path opens stay
write-free, and the slow path and the heal are one ordinary committed transaction. The heal's
`crsql` name match ignores Litestream's own `_litestream_*` tables. `silicon_brain`'s integration doc
checks this claim by claim (**Litestream Compatibility**).

---

## Tests

### How this fork's tests run

- `cd core && make test` builds `dist/test` from `src/tests.c` and `src/*.test.c`, linking the Rust
  `integration_check` crate as a static library. `tests.c`'s `main` runs C suites, then
  `SUITE("rust_integration") crsql_integration_check()`, which runs the Rust suites listed in
  `core/rs/integration_check/src/lib.rs` (`t::automigrate`, `t::teardown`, …). One suite can be run on
  its own: `dist/test rust_integration`, `dist/test crsql`.
- `make valgrind` runs the same binary under valgrind. `make asan` needs nightly (`STABLE_RUST_PORT.md`).
- The test binary is compiled with **`-DSQLITE_THREADSAFE=0`** and `-DUNIT_TEST=1`, with cr-sqlite
  registered as an auto-extension via `-DSQLITE_EXTRA_INIT=core_init`. So:
  - every `sqlite3_open` in a test runs init — through the new entry point, so under the
    **Init Busy Timeout** — and a test that needs a connection **without** cr-sqlite must call
    `sqlite3_reset_auto_extension()`, open, then re-register the entry point with
    `sqlite3_auto_extension`. That is easiest from a C suite (`src/crsqlite.test.c` or a new
    `src/init-race.test.c`);
  - **threads are not allowed** in this binary. Contention must be built from
    **several connections on one thread**, which SQLite's unix VFS locks against each other as it would across threads. A
    test that blocks an open on purpose waits out the budget, so it sets the test-only override.
- `py/correctness` (pytest, against `make loadable`) is the other harness. ⚠️ Its prior-version test,
  `py/correctness/tests/test_prior_versions.py`, is **entirely commented out** ("We no longer support
  these versions"), so `pytest -k prior` runs nothing. The prior-version refusals are covered by the C
  tests below instead.

### Deterministic tests — in `make test`

Name them so a grep finds them all: prefix `init_race_`.

| test | setup | assertion |
|---|---|---|
| `init_race_heals_field_sample` | copy `core/src/fixtures/halfinit.halfinit-db` (the preserved Half-Initialized Database, 28,672 bytes) to a temp file; open | open succeeds; version row = `CRSQLITE_VERSION`; one ordinal-0 site id; three triggers; unique index present. ⚠️ Fails today with the version-0 error — the red half of red→green |
| `init_race_heals_synthesized_residue` | plain connection writes the three tables with the **exact** DDL from `bootstrap.rs`, empty; close; open with cr-sqlite | same as above |
| `init_race_heal_refuses_v0_12` | copy `py/correctness/prior-dbs/v0.12.0.prior-db` | open still fails with the version-0 message; file bytes unchanged |
| `init_race_heal_refuses_v0_13_v0_15` | copy the other two fixtures | unchanged error (version 130000 / 150000); file bytes unchanged |
| `init_race_heal_refuses_nonempty` | residue plus one row in `crsql_tracked_peers`; then separately one `config.x` row in `crsql_master` | open fails, as today |
| `init_race_heal_refuses_crsql_object` | residue plus an empty `foo__crsql_clock` table; then separately an empty `Foo__CRSQL_Clock` table | open fails, as today. The mixed-case name pins the case-insensitive match; the `case_sensitive_like` half needs `load_extension`, so it lives in `py/correctness` (`test_init_race_heal_case_sensitive_like`: `PRAGMA case_sensitive_like = ON`, then load the extension on that file) |
| `init_race_heal_app_tables` | residue plus an application table with rows | heals (broad scope); app rows untouched |
| `init_race_atomic_at_each_step` | test-only fault injection (below) fails init at each persistent point, from `crsql_init_peer_tracking_table` through `create_site_id_triggers`, and at `COMMIT` | open fails; afterwards `sqlite_master` contains **no** `crsql` object; next open succeeds |
| `init_race_error_paths_free_once` | the same injection at each ownership point of **Init Error-Path Frees**, including after the commit (a failing `crsql_db_version` registration and a later one) | open fails, connection closes; run under `make valgrind`: no leak, no invalid free, no use after free |
| `init_race_loser_writes_nothing` | plain connection P holds `BEGIN IMMEDIATE` on a fresh file; budget overridden to a short value; open C with cr-sqlite | C fails with `SQLITE_BUSY` after the budget (measure the elapsed time against the bound); the file is still empty after P rolls back |
| `init_race_commit_busy` | P holds `SHARED` (`BEGIN; SELECT …`) on a fresh file; short budget; open C | C's `COMMIT` waits out the budget (P cannot release on one thread), rolls back; file empty; after P ends, C's next open succeeds |
| `init_race_reopen_writes_nothing` | fully initialized file; open it read-write while P holds `BEGIN IMMEDIATE`; separately open it read-only (`SQLITE_OPEN_READONLY`) | both opens succeed at once. ⭐ The read-write half is the one that proves the fast path takes no write lock; the read-only half cannot detect an over-strict check (**Keeping the re-open path read-only**) |
| `init_race_config_busy_fails` | initialized file with `merge-equal-values` set; make the `config.%` read meet a lock (short budget) | open fails rather than succeeding with the default |
| `init_race_host_txn_preserved` | only if `load_extension` is testable here — it is not, since the test binary sets `SQLITE_OMIT_LOAD_EXTENSION`. Cover in `py/correctness` instead: inside `BEGIN`, insert a row, `load_extension` on a wedged-but-unhealable file | the host's row is still there; the host's transaction is still open |

⭐ **A free red→green.** The "file bytes unchanged" assertions in `init_race_heal_refuses_v0_12` and
`init_race_heal_refuses_v0_13_v0_15` **fail today**, not only after a regression: none of the three
prior-version fixtures has `crsql_db_versions`, and `crsql_init_db_versions_table` commits it before
the version check refuses the file. Under the **Atomic Init Transaction** the refusal happens inside
the transaction, which rolls back, so the bytes stay unchanged.

**Fault injection.** A `#[cfg(feature = "test")]` static (`INIT_FAIL_AT_STEP`), set by a test-only C
entry point, makes the persistent operation or registration at that index return an error. Inject
`SQLITE_BUSY` for the transactional points and `SQLITE_NOMEM` for the registrations.
`integration_check` already enables the `test` feature, and `test_exports.rs` is where such hooks live.
It compiles to nothing in release builds. A static is safe because the test binary is single-threaded.

**Mutation-verify the important ones.** With the `InitTxn` commit moved back to before
`crsql_init_site_id`, `init_race_atomic_at_each_step` must fail. With the heal predicate weakened to the
naive rule, `init_race_heal_refuses_v0_12` must fail. With rule 3 written as `LIKE`,
`test_init_race_heal_case_sensitive_like` must fail. With a `crsql_freeExtData`
call restored after the `crsql_db_version` registration, `init_race_error_paths_free_once` must fail
under valgrind.

### Concurrency stress test — separate from `make test`

The real race needs genuine parallelism, which the single-threaded test binary cannot provide. Two
places to run it:

1. **In this fork: a multi-process harness.** `make sqlite3` builds `dist/sqlite3` with cr-sqlite
   statically linked and auto-loaded through `core_init.c`. A small script
   (`tools/init_race_stress.sh`, Bash) does, for R rounds: create a fresh temp path, launch N
   `dist/sqlite3 <path> 'SELECT crsql_site_id();'` processes, and then make one quiet open and count
   failures. ⭐ **Synchronise the starts**, or the processes stagger and the race rarely fires: each
   process first blocks reading a per-round FIFO (`read < "$fifo"` before `exec`ing `dist/sqlite3`),
   and the script releases all of them at once by opening the FIFO for writing. Also run a second
   mode against an initialized file (WAL, and rollback while one process switches it to WAL). SQLite's
   file locking behaves the same across processes as across threads, so this reproduces the race
   faithfully. ⛔ **Run it on the current `main` first and confirm it reproduces** — failed opens in
   every mode, and at least one wedge in 1,500 rounds of 10 (the app's probe found 5) — before
   trusting its "after" numbers. Then after the fix: **zero wedges and zero failed opens.** Keep it
   out of `make test` (it is slow and timing-dependent); run it by hand and record the numbers in this
   doc.
2. **In `silicon_brain`:** the probe, folded into a durable app test that exercises the real NIF and
   Exqlite's threads, in both its builds. That belongs to the integration doc.

### What to run

```
cd core
make test                      # all suites; or: dist/test rust_integration / dist/test crsql
make valgrind                  # leaks, invalid frees, use after free — required for the error-path tests
make loadable && (cd ../py/correctness && pytest -k init_race)
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
  pinned in `rust-toolchain.toml`. No `superfly_sqlite-rs-embedded` submodule change is needed: the
  busy timeout is set from C.
- **Commit the fixture.** `core/src/fixtures/halfinit.halfinit-db` is untracked today. It deliberately
  does not end in `.db`, which `.gitignore` ignores; `git check-ignore` confirms the new name is not
  ignored, and it follows the `py/correctness/prior-dbs/*.prior-db` convention.
- **Append `e4-init-atomic` to `BUILD_ID`** in `core/rs/core/src/sha.rs`, and document it there and
  in the README's token table:
  `e1-fd-close+e2-tbl-pushdown+e3-tbl-collate-gate+e4-init-atomic <sha>`. Hosts gate with a substring
  match, so existing gates on `e2-tbl-pushdown` keep working.
- **README:** add a change 4 under "Infocorder fork", with its benefit and its cost (opens through the
  new auto-extension entry point can wait up to 2 s for a competing connection; a Half-Initialized
  Database now opens), and say that static hosts must register the new entry point to get the wait.
- **Coordination doc:** when built, append a **Pin this** section with SHA, verification and the
  downstream steps. The two copies of `docs/CRSQLITE_FORK_UPGRADE_COORDINATION.md` are reconciled:
  byte-identical, and tracked here since `1a6b0613` (**Coordination Doc Drift** is closed). ⚠️ This
  repo's `.gitignore` still lists the file on its second line; it no longer hides a tracked file, but it
  would hide a re-created one, so remove that entry. Append in one repo and copy to the other.
- **Verification to record:** `make test` and `make valgrind` green; `py/correctness` green; the stress
  numbers before and after, in both modes; the `crsql_build_id()` output.

---

## Risks

- **A too-strict Init Completeness Check** sends healthy databases down the slow path. Every read-write
  open then takes the write lock and waits on active writers. Guarded by the read-write half of
  `init_race_reopen_writes_nothing`.
- **A too-loose heal predicate** silently re-initializes a genuine database with a new site id. That
  forks its identity, and every peer would see a new site. Guarded by the prior-db refusal tests. The
  argument for safety rests on the predicate admitting only files with no cr-sqlite state; keep rule 3
  name-based and broad (`instr(lower(name),'crsql')`), not a list of known names.
- **Blocking opens.** Through the new entry point, any open can wait up to 2 s while another connection
  holds a conflicting lock during its init — on a fresh file, or on an initialized one mid-switch to WAL.
- **A clobbered host busy handler** if the new entry point were ever used outside the auto-extension
  path. It is not exported for `load_extension` use; say so in its comment and the README.
- **A held `SHARED` lock across the slow path** — a completeness-check statement not finalized — makes
  the connection's own `COMMIT` wait on itself until the budget runs out. A code-review item, and
  `init_race_commit_busy` exercises the path.
- **Hosts that relied on the noise.** A host that treated a first failed open as normal changes
  nothing; one that parses the version-0 message to detect wedged files will stop seeing it.
- **Hosts that do not adopt the new entry point** keep the atomic init and the heal, but not the wait:
  their pool starts still see failed opens, cleanly.
- **Drift.** Another local change to init, which upstream will not have. `lib.rs` init is upstream's
  hottest file for merges; keep the diff confined to `bootstrap.rs`, the guard and error paths in
  `lib.rs`, the `config.%` loop in `ext-data.c`, and the new entry point.
- **Fleet mix.** A device still running an older build can wedge a new file until it upgrades. The
  heal then rescues that file the first time an upgraded build opens it. Rollout belongs to the
  integration doc.

---

## Why an integration document is needed downstream

Adopting this release in `silicon_brain` is **not** a plain pin bump, so it has its own document,
`docs/CRSQLITE_INIT_RACE_FIX_INTEGRATION.md` in that repo. In brief, the reasons are:

- the app links its own startup shim instead of `core_init.c`, so the **Init Busy Timeout** needs a
  one-line change there, landed with the pin bump;
- the app ships cr-sqlite through **three** build routes with **separate** pins: the static NIF script
  (spoke and hub), the `sqlite3mc` (SQLite3 Multiple Ciphers) spike build script, which the operator
  ruled stays at `0eb2f84e` and so never carries this fix, and a superfly prebuilt runtime `.so`
  fallback, which will **never** carry it;
- the CI marker and the hub deploy gate both key on `e2-tbl-pushdown` and would accept a build without
  the fix, and the hub does not log its `crsql_build_id()`;
- one production path — the spoke's first-boot `parking.db` — **is** exposed;
- and the app hides the symptom: its test helpers filter these log lines, its test runner re-runs
  them as flakes, and several code comments call them spurious.

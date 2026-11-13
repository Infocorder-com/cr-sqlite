# Making cr-sqlite's per-connection init atomic — the init race wedge

**Status:** ✅ **Built, verified and committed in `578ef5d6`** (2026-11-13), with the code review's
follow-up (**Code review follow-up**, in the **Build record**) committed in `f39caadd`. ⛔ The dependency
bumps committed next, in `85faa868`, broke the decoding of packed primary keys
(**Packed Blob Length Sign-Extension**, in the **Build record**); that fix is committed in `b8c6ffb1`
and `75d42269`. A crash-safety review of it found three older defects a peer can trigger through a merge
(**Merge Input Validation**, in the **Build record**); their fixes are in the working tree, verified, and
awaiting the operator's commit. The release SHA to pin is the commit that includes them.
⛔ **Never pin `85faa868`, or any commit from it up to the sign-extension fix (including `ba3c7ff5`).** Every
part below is implemented; `make test`, `make valgrind-strict`, `py/correctness` and the stress script
are green, and the key guarantees are mutation-verified. Where the build made a choice the plan left open,
or added to it, **Build record** says so. The plan was revised twice after adversarial reviews and the
operator's rulings before it was built.
**Area:** extension init — `core/rs/core/src/lib.rs` `sqlite3_crsqlcore_init`,
`core/rs/core/src/bootstrap.rs`, `core/src/ext-data.c`, `core/src/crsqlite.c` (`sqlite3_crsqlite_init`
and a new auto-extension entry point, `sqlite3_crsqlite_autoext_init`), `core/src/ext.h` and
`core/src/core_init.c`.
**Compatibility:** the init fix itself changes no wire or changeset format. ⛔ The dependency bumps of
`85faa868` did break the **reading** of the packed primary-key format, though the bytes written are
unchanged: blob primary keys of 128–255, 32768–65535 and 2^23 to 2^24−1 bytes could not be read
from `crsql_changes`, unpacked or merged (**Packed Blob Length Sign-Extension**). The fix in `b8c6ffb1`
restores the decoding; the format itself never changed, so a fixed build and a pre-`85faa868` build
exchange changes normally. One new exported C symbol,
`sqlite3_crsqlite_autoext_init` (the auto-extension entry point of **Init Busy Timeout**); existing
exported API symbols are unchanged (the internal Rust init symbols `sqlite3_crsqlrustbundle_init` and
`sqlite3_crsqlcore_init`, which only `crsqlite.c` calls, gained an `rc_out` parameter for
**Init Failure Code and Message**). On-disk format is unchanged for every database that opens today. The behaviour
changes are that a **Half-Initialized Database** — which today can never be opened — opens, and that an
open through the new entry point waits for a competing connection instead of failing.
**Downstream integration:** `silicon_brain` `docs/CRSQLITE_INIT_RACE_FIX_INTEGRATION.md` (pin bump,
startup shim change, static NIF rebuilds, fleet rollout, app tests, Litestream). Release logistics go in
the shared `docs/CRSQLITE_FORK_UPGRADE_COORDINATION.md` when the release is built.
**Rulings** (operator, 2026-11-13; recorded in `silicon_brain` `docs/DECISION_REGISTER.md`): the fix
lives in this fork, with no app-side guard (**Init Race Fork-Only Fix**); the heal's scope is broad
(**Init Residue Heal**); the busy handling ships as a busy timeout set by a separate auto-extension entry
point (**Init Busy Timeout**). The second review's findings were approved by the operator on 2026-11-13
and are folded in below.

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
  inspection of a copy confirms it. It is committed in this repo (`00c3fae4`) as
  `core/src/fixtures/halfinit.halfinit-db`.
- A probe opened 10 connections concurrently on a fresh file, 1,500 times, using a statically linked
  build of this fork (`6cf12654`). **Every one of the 1,500 rounds** had at least one failed open, and
  **5 of the 1,500 files were left permanently unopenable.** A reviewer's rerun confirmed that a wedged
  file keeps exactly the field shape and stays wedged on a second open.
- The reviewer (static NIF `6cf12654`) measured 10 concurrent opens in three cases, and two of them
  are files that are **already initialized**: about **91%** failed on a fresh file, about **3%** on an
  initialized WAL file at pool start, and about **41%** on an initialized rollback-journal file while a
  pool-mate switches it to WAL. So the missing busy timeout is not confined to fresh files.

This plan makes the fix in three parts, each with its own name:

1. **Atomic Init Transaction** — all of init's writes happen in one transaction, so a file is either
   untouched or fully initialized. This stops new wedges, from races and from interrupted single
   connections alike.
2. **Init Residue Heal** — a file that is already a Half-Initialized Database is recognised, by a
   test that admits only files with no cr-sqlite state, and initialized as if it were new. This rescues
   files wedged by earlier builds.
3. **Init Busy Timeout** — when cr-sqlite is registered as an auto-extension through the new entry
   point `sqlite3_crsqlite_autoext_init`, init runs with a busy handler that allows it
   **2 s of lock waiting in total per open**, cleared afterwards, so an open waits for a competing
   connection instead of failing at once — on fresh and initialized files alike.

It also fixes four smaller defects found on the way: a missing unique index that can never be
recreated (**Site-ID Index Gap**); error paths that issue a bare `ROLLBACK` and so can abort a
transaction that belongs to the host (**Init Host Rollback**); error paths that leak, double-free, or
leave a connection that can never be closed (**Init Error-Path Frees**); and, on the `load_extension`
path, callbacks left pointing into a library SQLite has already unloaded, so that closing the
connection can jump into unmapped code (**Failed Load Unmapped Callback**).

---

## Summary

| part | what it changes | status |
|---|---|---|
| **Atomic Init Transaction** | wraps every persistent write of init in one `BEGIN IMMEDIATE` … `COMMIT`, entered only when a read-only check says the file needs initializing. Every registration that carries a destructor is outside the transaction: `crsql_internal_sync_bit` registers **before** it, and `crsql_db_version` is init's **last** fallible step, after it | ✅ built — this is the fix |
| **Init Residue Heal** | treats a data-free Half-Initialized Database as a blank slate, application tables allowed | ✅ built; scope ruled broad |
| **Init Busy Timeout** | the new entry point `sqlite3_crsqlite_autoext_init` installs a busy handler with a 2 s per-open budget before init and clears it after; nothing changes on the `load_extension` path | ✅ built, with the per-open budget handler (**The per-open budget**) |
| **Init Host Rollback** | error paths roll back to their own savepoint instead of issuing a bare `ROLLBACK` | ✅ built (falls out of the transaction rewrite) |
| **Init Error-Path Frees** | one owner for the ext data and the site-id buffer at every point of init; no failure can leave unfinalized statements behind, so a failed open always closes | ✅ built (falls out of the transaction rewrite) |
| **Failed Load Unmapped Callback** | `crsql_internal_sync_bit`'s destructor becomes SQLite's own `sqlite3_free`; no destructor that lives in the extension library is registered before the last fallible step; the README says a failed `load_extension` leaves the connection unusable | ✅ built |
| `crsql_build_id()` token | appends `e4-init-atomic` | ✅ built, so hosts can gate on the fix |

**Recommendation:** ship all of them together as one release (`e4-init-atomic`).

---

## The mechanism — verified against this fork's source

### The init sequence

`sqlite3_crsqlite_init` (`core/src/crsqlite.c`) calls `sqlite3_crsqlrustbundle_init`
(`core/rs/bundle/src/lib.rs`), which calls `sqlite3_crsqlcore_init` (`core/rs/core/src/lib.rs`). After
registering some functions and virtual-table modules (`crsql_set_debug`, `crsql_automigrate`,
`crsql_pack_columns`, `crsql_as_table`, `crsql_unpack_columns`, `clset`), its work is, in order (each
item is named by its function, and the rest of this document refers to it by that name):

- **`crsql_init_peer_tracking_table`** (`bootstrap.rs`) — `CREATE TABLE IF NOT EXISTS
  crsql_tracked_peers (…) STRICT`.
- **`crsql_init_db_versions_table`** — `CREATE TABLE IF NOT EXISTS crsql_db_versions (…) STRICT`.
- **the sync-bit registration** — `crsql_internal_sync_bit` is registered with a `sqlite3_malloc`'d
  `int` as its user data and **`crsql_sqlite_free`** (a wrapper in this library, `lib.rs`) as its
  destructor. `crsql_newExtData` needs it to exist, because it prepares `SELECT
  crsql_internal_sync_bit(1)` and `(0)`.
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
    `CRSQLITE_VERSION_0_17_0` (`consts.rs`; today `CRSQLITE_VERSION` equals it, 170000). A file whose
    version is higher than `CRSQLITE_VERSION` is accepted and not rewritten. On success `RELEASE`
    (commit); on failure a bare `ROLLBACK`.
- **`crsql_newExtData`** (`core/src/ext-data.c`) prepares statements that read `crsql_db_versions` and
  `crsql_master`, reads the `config.%` rows of `crsql_master`, and steps `PRAGMA data_version`. Then
  `crsql_update_site_id` is registered with the ext data and **no destructor** — it must be, before
  **`crsql_init_site_id`**, because inserting into an existing `crsql_site_id` prepares its triggers,
  and the triggers call that function.
- **`crsql_init_site_id`** → `init_site_id`: if there is no `crsql_site_id` table,
  `create_site_id_and_site_id_table` runs `CREATE TABLE "crsql_site_id" (…)` (no `IF NOT EXISTS`)
  and `CREATE UNIQUE INDEX crsql_site_id_site_id …` through one `exec_safe` call — which is
  `sqlite3_exec`, so **each statement commits separately** — and then `insert_site_id` inserts
  ordinal 0. If the table exists, it reads the ordinal-0 row, inserting one if it is missing. Either
  way it copies the site id into a buffer: **this read is needed on every open**, not only on a fresh
  file.
- **`crsql_initSiteIdExt`** takes ownership of the site-id buffer (`ext_data->siteId`) and prepares
  statements against `crsql_site_id`.
- **`create_site_id_triggers`** — three `CREATE TRIGGER IF NOT EXISTS`, again each committed
  separately.

After that, `sqlite3_crsqlcore_init` registers `crsql_site_id`, then
**`crsql_db_version` with the destructor `x_free_connection_ext_data`**, then about twenty more
functions with the ext data and no destructor, and returns the ext data to `sqlite3_crsqlite_init`.
That C function then registers the **`crsql_changes`** virtual-table module and, only if that
succeeds, installs the close-trace hook (`crsql_close_trace_hook`, the E1 fix), the commit hook and the
rollback hook. So init's own writes never reach `crsql_commit_hook`, and the close-trace hook — the
only thing that finalizes the ext data's statements when a connection closes — is
**not installed on any failure path**.

**On a fully initialized database, this sequence writes nothing.** `CREATE TABLE IF NOT EXISTS` and
`CREATE TRIGGER IF NOT EXISTS` on existing objects, and an empty `SAVEPOINT`/`RELEASE`, all succeed on a
**read-only** connection (checked with the stock 3.50.2 shell, 2026-11-13). This property must survive
the fix; see **Keeping the re-open path read-only**.

### Why concurrent opens race

- With static linking the host registers an init function as an auto-extension
  (`core/src/core_init.c` does so for this repo's CLI and tests; `silicon_brain` uses its own startup
  shim), so init runs **inside** `sqlite3_open_v2`. The host cannot install a busy handler until the
  open returns, so init has **no busy handler**: any lock conflict is an immediate `SQLITE_BUSY`.
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

### Four smaller defects in the same code

- **Site-ID Index Gap.** If `CREATE TABLE crsql_site_id` commits and `CREATE UNIQUE INDEX
  crsql_site_id_site_id` then fails, the table persists without its index. Every later open sees the
  table and never creates the index. The table then accepts duplicate `site_id` values, which the
  ordinal lookup (`SELECT ordinal FROM crsql_site_id WHERE site_id = ?`) does not expect. Not
  observed in the field; it is a property of the code.
- **Init Host Rollback.** `crsql_create_schema_table_if_not_exists` and `crsql_maybe_update_db`
  recover from an error with a bare `ROLLBACK`, not `ROLLBACK TO <savepoint>`. During an
  auto-extension open there is no outer transaction, so this is equivalent. But when a host calls
  `load_extension` inside its own transaction, a failing init **rolls back the host's transaction**.
- **Init Error-Path Frees.** The failure paths of `sqlite3_crsqlcore_init` and `sqlite3_crsqlite_init`
  disagree about who owns the ext data and the site-id buffer:
  - **`create_site_id_triggers` failing** frees `site_id_buffer` — which `ext_data->siteId` already
    points at — and leaks `ext_data` and its prepared statements. This failure is reachable today: the
    triggers are created outside any transaction, so a race can fail them with `SQLITE_BUSY`.
  - **Any failure after `crsql_db_version` is registered**, inside `sqlite3_crsqlcore_init`, still
    calls `crsql_freeExtData`, but that registration carries the destructor
    `x_free_connection_ext_data`, which frees the same ext data again when the connection closes — a
    **double free** at close. SQLite also runs the destructor when the registration itself fails
    (`createFunctionApi` calls it when the registration took no reference), so the failure path of the
    `crsql_db_version` registration frees it twice immediately.
  - ⛔ **A failure after `crsql_db_version` that frees nothing leaves a zombie connection.** The ext
    data's persistent prepared statements are finalized only by `crsql_freeExtData` or
    `crsql_finalize`, and the close-trace hook that calls `crsql_finalize` is installed only on
    success. `sqlite3_close_v2` on a connection with unfinalized statements marks it a **zombie** and
    returns; the connection is deallocated only when its last statement is finalized, which never
    happens. Its file descriptors, its `-shm` mapping and the ext data leak until the process exits —
    the same leak class the E1 fix closed for successful opens (`docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md`).
    Hosts reach this: Exqlite calls `sqlite3_close_v2` when `sqlite3_open_v2` fails
    (`deps/exqlite/c_src/sqlite3_nif.c`, `exqlite_open`). ⚠️ **It happens today** at the one late
    failure point that frees nothing: `sqlite3_crsqlite_init`'s `sqlite3_create_module_v2("crsql_changes")`
    failure returns after `crsql_db_version` has handed the ext data to a destructor that only runs on
    deallocation.
  - **Functions registered with the ext data and no destructor** (`crsql_update_site_id`,
    `crsql_site_id` and the rest) keep a pointer to it after a failure frees it.
- **Failed Load Unmapped Callback.** When `load_extension` fails, SQLite unloads the library at once:
  `sqlite3LoadExtension` calls `sqlite3OsDlClose` on a non-zero `xInit` result (SQLite 3.54.1
  amalgamation, around line 144401; the same code is in the bundled 3.42.0). Everything init registered
  before the failure stays registered and keeps pointing into the unmapped library. Above all,
  `crsql_internal_sync_bit`'s destructor `crsql_sqlite_free` is code in that library, and SQLite calls it
  when the connection closes — so closing a connection after a failed `load_extension` can
  **jump into unmapped code**, whatever the host does. The registered function implementations and virtual-table
  modules point there too, so calling any of them afterwards does the same. Removing the registrations
  on the failure path is not possible under `SELECT load_extension(...)`, which is how Exqlite loads
  extensions: SQLite refuses to delete or modify a function while any statement is active, with
  `SQLITE_BUSY` ("unable to delete/modify user-function due to active statements", `sqlite3CreateFunc`).
  Reachable today on any failed load: the wedge's version-0 refusal comes after the sync-bit
  registration.

---

## The fix

### Atomic Init Transaction

**Shape.** Split init into a read-only check and a locked slow path:

1. **Init Completeness Check** (read-only). One read that decides whether the existing code would write
   anything. The file is complete when all of these hold:
   - tables `crsql_tracked_peers`, `crsql_db_versions`, `crsql_master` and `crsql_site_id` exist;
   - `crsql_master` holds `crsqlite_version` ≥ `CRSQLITE_VERSION`;
   - `crsql_site_id` holds an ordinal-0 row;
   - the three `crsql_site_id_*_trig` triggers exist.

   ⛔ Any step result other than `SQLITE_ROW`/`SQLITE_DONE` is an **error that fails the open** — never
   "incomplete" and never "complete". The check's statements must be
   **finalized before the slow path starts**: a statement left un-reset keeps a `SHARED` lock and would
   make the slow path wait on itself.
2. **Fast path** (the check says complete). ⭐ Recommended:
   **run the existing sequence, unchanged, inside one deferred read transaction** — `BEGIN DEFERRED`, the check, then the sequence from
   `crsql_init_peer_tracking_table` through `create_site_id_triggers`, then `COMMIT`. On a complete
   file every step of the sequence writes nothing (the property **Keeping the re-open path read-only**
   pins), so the sequence costs a few catalog lookups and gives exactly today's connection-local result:
   the version check, the ext data, the site-id read into its buffer and the statements. Why not skip the
   sequence instead: `init_site_id`'s read of the site id is needed on every open, so a skipping fast
   path would need its own copy of that read and of the version check, and two code paths for one result
   drift apart. Why one read transaction: the check and the sequence see one snapshot, so a file cannot
   change between "complete" and the reads that rely on it; and it holds one `SHARED` lock instead of
   taking one per statement, so the fast path meets at most one lock wait. If the check is ever too
   loose and the sequence tries to write, the write upgrades the read transaction and can fail with
   `SQLITE_BUSY` — cleanly, inside the transaction — which is why the check is pinned by tests.
3. **Slow path** (the check says incomplete). `COMMIT` the read transaction (it wrote nothing, so this
   only releases `SHARED`), finalize the check's statements, then `BEGIN IMMEDIATE`.
   **Re-run the same check under the lock** (another connection may have finished while this one waited).
   - ⭐ **If the re-check finds the file complete, end the transaction at once** — `COMMIT`; it wrote
     nothing, so it needs no `EXCLUSIVE` lock and cannot wait —
     **and continue from the start of the fast path.** Holding `RESERVED` through the sequence and the registrations would block every other
     writer for that time and buy nothing, and re-entering the fast path keeps one code path for a
     complete file.
   - Otherwise run the existing sequence, from `crsql_init_peer_tracking_table` through
     `create_site_id_triggers`, in its current order — it is already idempotent, except
     `create_site_id_and_site_id_table`'s `CREATE TABLE`, which `has_table` guards and which is now
     race-free because the lock is held. Then `COMMIT`.

**Where the transaction ends, and what sits on each side of it.** The rule is:
**everything persistent is inside the transaction, and every registration with a destructor is outside it.**
Today's
order breaks the rule once — the sync-bit registration (destructor `crsql_sqlite_free`) sits between
`crsql_init_db_versions_table` and `crsql_maybe_update_db` — and it cannot simply move after the
`COMMIT`, because `crsql_newExtData`, which is inside, prepares statements that call it. So:

- **Before the transaction:** the early registrations (`crsql_set_debug` … `clset`) and
  **the sync-bit registration, moved up to join them**, with SQLite's own `sqlite3_free` as its destructor
  (**Failed Load Unmapped Callback**). From that registration on, SQLite owns the `int`: it frees it at
  close, or at once if the registration fails.
- **Inside the transaction:** `crsql_init_peer_tracking_table`, `crsql_init_db_versions_table`,
  `crsql_maybe_update_db`, `crsql_newExtData`, the `crsql_update_site_id` registration (no destructor),
  `crsql_init_site_id`, `crsql_initSiteIdExt` and `create_site_id_triggers`. `COMMIT` immediately after
  `create_site_id_triggers`.
- **After the transaction:** every remaining registration that carries **no** destructor
  (`crsql_site_id`, `crsql_next_db_version`, … `crsql_config_get`), then the `crsql_changes` module in
  `sqlite3_crsqlite_init`, then — as **the last fallible step of init** — the `crsql_db_version`
  registration with `x_free_connection_ext_data`, which hands the ext data to SQLite (the
  **ownership handoff**). Then the close-trace, commit and rollback hooks, which cannot fail.

Recommended placement of the handoff: move the `crsql_db_version` registration out of
`sqlite3_crsqlcore_init` into a small Rust export (for example `crsql_register_db_version`) that
`sqlite3_crsqlite_init` calls right after `crsql_changes` registers. The ordering is what matters: no
fallible step may follow the handoff (**Init Error-Path Frees**).

`BEGIN IMMEDIATE`, not a deferred `SAVEPOINT`, for the slow path, because:

- it takes the `RESERVED` lock before reading, so the slow-path reads and writes see one consistent
  state, and `has_schema_table` can no longer be computed against a state that changes before the write;
- a deferred transaction that reads under `SHARED` and then upgrades to `RESERVED` can be refused with
  `SQLITE_BUSY` **without** the busy handler being invoked (SQLite's deadlock avoidance), so it cannot
  be made to wait.

**If the host is already inside a transaction** (`sqlite3_get_autocommit(db) == 0` — only possible on
the `load_extension` path), use `SAVEPOINT crsql_init` instead of both the read transaction and
`BEGIN IMMEDIATE`, and roll back with `ROLLBACK TO crsql_init; RELEASE crsql_init`. That fixes
**Init Host Rollback** too, together with every savepoint inside the sequence rolling back with
`ROLLBACK TO <its savepoint>` instead of a bare `ROLLBACK`. ⛔ Never wait or retry inside a host's
transaction: a write there may be refused by deadlock avoidance, and the only correct response is to roll
back to the savepoint and fail the load.

**Everything persistent joins the transaction**, including the peer and db-version tables, the
site-id table, its unique index, its row and its triggers. Leaving any of them out recreates a smaller
version of the same problem: a committed table with missing companions. Joining costs nothing — a
fresh init is a handful of DDL statements under one lock. It also closes the **Site-ID Index Gap** for
every database initialized from now on.

**What a losing connection sees.** It reads, finds the file incomplete, finalizes, and requests
`BEGIN IMMEDIATE`. With the **Init Busy Timeout** it waits, the re-check under the lock finds the
winner's committed work, and it commits at once and takes the fast path, writing nothing. Without one
(the `load_extension` path with no host handler), `SQLITE_BUSY` fails its open — **cleanly**: it wrote
nothing and holds no lock, and the pool retries.

**What happens if the winner is interrupted** (error, `SQLITE_BUSY` on `COMMIT` after the budget,
process killed, disk full): the transaction rolls back, or the hot journal is rolled back by the next
opener. The file returns to untouched. That is the property this whole plan exists for, and it is what
closes the single-connection wedge as well as the race.

**`COMMIT` can itself return `SQLITE_BUSY`** in rollback mode, while another connection still holds
`SHARED` from its completeness check. SQLite invokes the busy handler while it waits for those readers,
so under the **Init Busy Timeout** the `COMMIT` waits, within the same per-open budget. If it still
fails, init `ROLLBACK`s and fails the open.

**Implementation sketch.** The transaction spans `lib.rs`, because `crsql_newExtData` and the
`crsql_update_site_id` registration sit between `crsql_maybe_update_db` and `crsql_init_site_id`. Use
one RAII guard (`InitTxn`) for either transaction kind, created before `crsql_init_peer_tracking_table`,
whose `Drop` rolls back unless `commit()` was called, and call `commit()` right after
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

With the order above, the ownership rules are:

- **Before `crsql_newExtData`**: there is nothing of init's to free. The sync-bit `int` belongs to
  SQLite from its registration.
- **From `crsql_newExtData` until `crsql_initSiteIdExt`**: a failure frees `site_id_buffer` (if
  allocated) and calls `crsql_freeExtData`, separately.
- **From `crsql_initSiteIdExt` until the ownership handoff**: `ext_data` owns the buffer. A failure
  calls `crsql_freeExtData` only — never a bare free of the buffer. This is the fix for the
  `create_site_id_triggers` path, and it covers the `crsql_changes` failure in `sqlite3_crsqlite_init`,
  which now sits before the handoff and so frees the ext data by hand.
- **The handoff** — the `crsql_db_version` registration with `x_free_connection_ext_data` — is the
  last fallible step. If it fails, SQLite has already run the destructor; return without freeing.
- **After the handoff, nothing is fallible.** ⛔ If a fallible step is ever added after it, its failure
  path must call `crsql_finalize(ext_data)` — statements only, keeping the struct, which the
  destructor frees — or the host's `sqlite3_close_v2` leaves a zombie. A comment at the handoff says so.

Because every failure before the handoff runs `crsql_freeExtData`, which finalizes every statement
(and clears the statement cache), **a failed open never leaves an unfinalized statement**, and the
host's `sqlite3_close_v2` deallocates the connection. This also covers one SQLite detail: if an
auto-extension returns `SQLITE_NOMEM`, `sqlite3_open_v2` itself closes the handle with `sqlite3_close`
(v1) and returns a NULL handle (`openDatabase`, `opendb_out`), which would leak the whole connection if
statements were outstanding.

**Functions registered with `ext_data` and no destructor** keep a pointer to it after a failure frees
it. On the auto-extension path that is harmless: the open has failed, SQLite has marked the handle
unusable, and the host can only close it, which calls no function. On the `load_extension` path the
connection is unusable after a failed load anyway (**Failed Load Unmapped Callback**), and the
registrations cannot be removed under `SELECT load_extension(...)`. So the fix does not try to delete
them; the README says what a host must do instead.

The fault-injection tests below cover each of these points, and must pass under the strict valgrind
target (**Fault injection**, **What to run**).

### Failed Load Unmapped Callback — nothing the library owns runs after it is unloaded

1. **Pass SQLite's own `sqlite3_free` as `crsql_internal_sync_bit`'s destructor**, not the
   `crsql_sqlite_free` wrapper. The `int` is `sqlite3_malloc`'d, so `sqlite3_free` is the correct
   destructor, and its code belongs to SQLite, which outlives any extension. On the loadable path take
   the pointer from the API routines table SQLite passed in (`pApi->free`), which is what the
   `sqlite3_free` name resolves to there.
2. **Register no destructor that lives in this library before the last fallible step.** After the
   sync-bit change the only such destructor is `x_free_connection_ext_data`, and the ownership handoff
   makes it the last fallible step. If the handoff itself fails, SQLite runs the destructor
   immediately, while the library is still mapped.
3. **README:** a failed `load_extension` of cr-sqlite leaves the connection unusable. The host must
   close it and must not call any `crsql_*` function or use `crsql_changes` on it: those registrations
   point into a library SQLite has already unloaded. (Closing is safe after this fix, because no
   library-resident destructor remains registered.)
4. **Tests that fail a load on purpose must not be able to segfault** — today, closing their connection
   calls `crsql_sqlite_free` in an unloaded library. Redesign `init_race_host_txn_preserved` and
   `test_init_race_heal_case_sensitive_like` (both in `py/correctness`) to open a **keeper connection**
   first, load the extension on it successfully (on `:memory:`), and close it only after the failing
   connection has closed. `dlopen` reference-counts the library, so SQLite's `dlclose` after the failed
   load does not unmap it while the keeper holds it. Add one test that exercises the fix itself without
   a keeper: `test_init_race_failed_load_close` (below).

### Init Residue Heal — scope ruled broad

The **Atomic Init Transaction** prevents new wedges. It does not rescue a file wedged by an earlier
build: `has_schema_table` is true and there is no version row, so the version-0 refusal still fires.

**The heal predicate.** Inside init's transaction, treat the database as a blank slate
— `is_blank_slate = true` — when **all** of these hold. The predicate runs in `maybe_update_db_inner`,
so it runs wherever the sequence does: on the slow path, under `BEGIN IMMEDIATE` and so under the write
lock; and on the `SAVEPOINT crsql_init` path (`load_extension` inside a host transaction), which holds
**no** write lock. That second case is safe: a competing writer makes the heal's first write fail to
upgrade the lock, the open fails, and the savepoint rolls back cleanly, leaving the file as it was.

1. `crsql_master` exists and has **zero rows** — not merely "no `crsqlite_version` row". (`crsql_master`
   also stores `config.*` rows written by `crsql_config_set`; a file with any row was opened
   successfully at least once.)
2. `crsql_tracked_peers` and `crsql_db_versions`, if they exist, have zero rows.
3. **No other cr-sqlite object exists.** No table, index, trigger or view in `sqlite_master` whose name
   contains `crsql`, other than those three tables and their `sqlite_autoindex_*` indexes. The
   exemption is by type **and** name — objects of type `table` with exactly those names — so a user
   trigger called `crsql_master` still blocks the heal (`init_race_heal_refuses_crsql_named_trigger`). Match with
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

### Init Busy Timeout — ruled

Init cannot borrow the host's busy handler on the auto-extension path, because it runs before the host
can install one.

**The mechanism.** Add a second exported entry point to `core/src/crsqlite.c`,
**`sqlite3_crsqlite_autoext_init`** (final name), with the same signature as `sqlite3_crsqlite_init`,
that:

1. calls `SQLITE_EXTENSION_INIT2(pApi)`;
2. installs init's busy handler with a per-open budget of `CRSQL_INIT_BUSY_TIMEOUT_MS`
   (**The per-open budget**) **before any init step**;
3. calls `sqlite3_crsqlite_init(db, pzErrMsg, pApi)` and keeps its result;
4. clears the handler — `sqlite3_busy_handler(db, 0, 0)`, which is what `sqlite3_busy_timeout(db, 0)`
   does — on success **and** on failure, and returns the result.

**Declarations and registration.**

- Declare the new symbol in `core/src/ext.h` next to `sqlite3_crsqlite_init`. `core_init.c` includes
  only `ext.h`, and is appended to the amalgamation (`dist/sqlite3-extra.c`), so it sees nothing else.
  ⚠️ `ext.h` and `crsqlite.h` share the include guard `CRSQLITE_H`, so a file that includes both gets
  only the first; `core_init.c` includes only `ext.h`, which is fine, but do not add `ext.h` to a file
  that already includes `crsqlite.h`.
- Register it **only** as an auto-extension: `core_init.c` changes its one call to
  `sqlite3_auto_extension((void *)sqlite3_crsqlite_autoext_init)`, and `silicon_brain`'s startup shim
  (`monkeypatched_deps/exqlite_crsqlite_init.c`), which that app links instead of `core_init.c`, does
  the same. `sqlite3_crsqlite_init` itself is unchanged and stays the `load_extension` entry point.
- **`#ifdef LIBSQL`.** Under libSQL, `sqlite3_crsqlite_init` takes a fourth argument
  (`const libsql_api_routines *pLibsqlApi`). Give the new entry point the same conditional parameter
  list and forward the fourth argument; make the `ext.h` declarations conditional the same way (today
  `ext.h` declares the three-argument form unconditionally, which is already wrong under libSQL). ⚠️
  Untested: this fork has no libSQL build (the `-DLIBSQL=1` line in `core/Makefile` is commented out),
  and how libSQL invokes auto-extensions was not checked. Say so in the comment.

**Why clearing is safe there.** An auto-extension runs inside `sqlite3_open_v2`, before the open
returns a handle, so the host cannot have installed a busy handler yet; installing one and then clearing
it restores the exact prior state. (A different auto-extension registered **earlier** could in principle
have installed one, which clearing would remove. Neither this repo's builds nor `silicon_brain`'s
register any.) On the `load_extension` path the host may already have a handler, and SQLite has
**no API to read it back**, so that path must not touch it — which is why this is a separate entry point
and not a change to `sqlite3_crsqlite_init`.

**What it covers.** Every lock wait of an auto-extension init: the fast path's read transaction (the
**Init Completeness Check** and the other fast-path reads, **Why opens of initialized files fail too**),
`BEGIN IMMEDIATE`, and `COMMIT`.

#### The per-open budget

⚠️ **`sqlite3_busy_timeout` bounds each wait, not the open.** SQLite resets the busy count
(`db->busyHandler.nBusy = 0`) at the start of every `sqlite3VdbeExec` — that is, every
`sqlite3_step` — and of every prepare (`sqlite3LockAndPrepare`); checked in both the bundled 3.42.0 and
Exqlite's 3.54.1 amalgamations. The default handler then measures its timeout from that count, so each
lock-taking step may wait the full timeout afresh. Today's init has well over half a dozen lock-taking
points on a fresh file (each read, each separately committed DDL statement, each prepare that loads the
schema), and the plan's slow path still has at least three: the fast path's read transaction, `BEGIN IMMEDIATE`
and `COMMIT` — four when the re-check sends it back to the fast path. A 2 s `sqlite3_busy_timeout` would
therefore bound an open at 6 to 8 s, not 2 s — enough to break the callers' arguments below.

Three ways to state or restore a bound were considered:

- **Per-wait timeout, worded honestly.** Keep `sqlite3_busy_timeout(db, 2000)` and document "2 s per
  wait, 6 to 8 s worst case per open". Simplest, but every caller argument below would have to be
  re-made at 6 s, and the worst case grows whenever init gains a lock-taking step.
- **One read transaction for the fast path.** Already recommended (**Atomic Init Transaction**), for
  snapshot consistency; it reduces the fast path to one wait, but not the slow path, so on its own it
  does not give a per-open bound.
- ⭐ **Recommended: a per-open budget handler.** Install, with `sqlite3_busy_handler`, a small handler
  whose argument is a struct on `sqlite3_crsqlite_autoext_init`'s stack: `{ budget_ms, slept_ms,
  n_waits }`. On each call it ignores SQLite's per-statement count, picks the next back-off delay from a
  table like SQLite's own (1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100 ms, then 100 ms), shortens the
  last delay to fit, sleeps with `sqlite3_sleep`, adds the delay to `slept_ms`, and returns 0 once
  `slept_ms` reaches `budget_ms`. That bounds
  **the total time init spends waiting for locks, across every step of the open**, at the budget — whatever init's step count is now or becomes. It counts
  time slept, not wall time, so it adds no clock dependency; the work between waits (a fresh init's few
  DDL statements and one `fsync`) is on top. The struct lives on the entry point's stack and the handler
  is cleared before the entry point returns, so nothing dangles. About twenty lines of C, no Rust
  change, no new binding.

**The budget, `CRSQL_INIT_BUSY_TIMEOUT_MS` = 2000**, of lock waiting per open. A compile-time constant.
Correctness does not depend on it — an open that runs out fails cleanly and leaves the file untouched —
so it only has to cover real contention while staying well inside every caller's deadline:

- **Real waits are milliseconds**: a fresh init is a handful of DDL statements and one commit, and a
  pool-mate's WAL switch is one more. 2 s leaves room for a slow `fsync` or a Windows antivirus scan
  of a new file.
- **`silicon_brain` callers**: 2000 ms matches Exqlite's own default `busy_timeout`; with the per-open
  budget, init's waiting stays below the spoke boot's 3 s per-project floor (`Boot`'s
  `@per_project_budget_floor_ms`) and far below the hub's 90 s `ProjectDbManager.ensure_repo/1` call and
  60 s provisioning lock. (Exqlite's own pragmas after the open wait under Exqlite's handler, per wait,
  as they do today; that is not init's budget.) Burrito's own unpacking happens before the BEAM starts
  and is not affected.
- **Exqlite runs `open` as a dirty I/O NIF** (`ERL_NIF_DIRTY_JOB_IO_BOUND`, `deps/exqlite/c_src/
  sqlite3_nif.c`), so a waiting open occupies a dirty I/O scheduler, never a normal one. Ten opens
  waiting at once could occupy the default ten dirty I/O schedulers for up to the budget — 2 s with the
  per-open budget; a per-wait timeout, or a larger budget, would make that stall longer.

**Sleep granularity.** SQLite's sleep and default busy callback depend on compile options, and the
two amalgamations differ:

- **Exqlite's SQLite 3.54.1** (what `silicon_brain` runs): the default busy callback sleeps in fractions
  of a second unless `HAVE_NANOSLEEP` is defined as 0, and Windows always does. So the app's waits are
  fine-grained regardless of the `-DHAVE_USLEEP=1` its builds pass.
- **This fork's bundled SQLite 3.42.0**: the default busy callback is fractional only under
  `SQLITE_OS_WIN || HAVE_USLEEP`; otherwise it sleeps in whole seconds and gives up at once when the
  timeout is under 1 s. ⚠️ **This fork's own builds lack `HAVE_USLEEP`.** Add `-DHAVE_USLEEP=1` to the
  flags of the test binary and `dist/sqlite3`, so this repo's tests and stress script — whose other
  processes rely on the shell's `.timeout`, i.e. the default callback — measure the same waiting
  behaviour hosts get, and so `sqlite3_sleep` can fall back to `usleep` rather than `sleep`.

**Test-only override.** Under `UNIT_TEST` (already defined for the test binary), export
`void crsql_test_set_init_busy_budget_ms(int ms)`: `ms >= 0` sets the budget the entry point uses,
`ms < 0` restores `CRSQL_INIT_BUSY_TIMEOUT_MS`. It sets a file-static in `crsqlite.c` that the entry
point reads at each open; declare it in `crsqlite.h` under `#ifdef UNIT_TEST`. Tests that block on
purpose set a short budget (for example 200 ms) and restore it in their teardown. It compiles to nothing
in release builds.

**Why not an internal retry.** An earlier version of this plan retried `BEGIN IMMEDIATE` and `COMMIT`
in a bounded loop with `sqlite3_sleep`, precisely to avoid touching the busy handler. It protected only
those two points — not the fast-path reads, which the measurements showed failing on initialized files.
A busy handler installed by a separate auto-extension entry point covers every read and write of init.

### Init Failure Code and Message — what a host sees when init fails

**The rule.** A failed init returns its **real** result code — `SQLITE_BUSY` when it could not get
its lock — and always leaves a message. Before this rule, `sqlite3_crsqlite_init` returned
`SQLITE_ERROR` whenever the Rust half failed and set no message, so a host saw rc 1 and a message that
ended in a colon with nothing after it.

**How the code travels.** The Rust half returns a pointer (the ext data), so the code travels in an
out-parameter: `sqlite3_crsqlrustbundle_init` and `sqlite3_crsqlcore_init` take an `int *rc_out`
(internal symbols; only `crsqlite.c` calls them). `sqlite3_crsqlite_init` seeds it with
`SQLITE_ERROR`; the Rust half overwrites it with the code of the step that failed — the
**Atomic Init Transaction**'s `BEGIN`, read, `COMMIT`, or any step of the sequence, each of which
now passes its own code up instead of collapsing to `SQLITE_ERROR` — and with the fractional-index
init's code if that fails. A registration failure keeps the seeded `SQLITE_ERROR`.

**The message.** `crsql_init_failed` in `crsqlite.c` sets `*pzErrMsg` to
`cr-sqlite init: <sqlite3_errstr(rc)>` — `cr-sqlite init: database is locked` for a lock failure —
unless a step already left a more specific message (the version-0 refusal does). It uses
`sqlite3_mprintf`, which on the loadable path is the host SQLite's own allocator
(`SQLITE_EXTENSION_INIT2` routes it through the API routines), because both of SQLite's callers free
the message with `sqlite3_free`. Checked in this fork's SQLite 3.42.0 and Exqlite's 3.54.1: the two
`sqlite3AutoLoadExtensions` and `sqlite3LoadExtension` are identical in this respect. The Rust
version-0 message is allocated by the Rust global allocator, which is `sqlite3_malloc`
(`SQLite3Allocator`), so it is freed correctly too.

**What the host sees, per path:**

| path | return code | message |
|---|---|---|
| auto-extension (`sqlite3_open_v2`; Exqlite's) | `SQLITE_BUSY` (5): `sqlite3AutoLoadExtensions` stores the entry point's code with `sqlite3ErrorWithMsg`, and `openDatabase` returns `sqlite3_errcode(db)` | `automatic extension loading failed: cr-sqlite init: database is locked` |
| `sqlite3_load_extension` / `SELECT load_extension(...)` | ⚠️ **always `SQLITE_ERROR` (1)**: `sqlite3LoadExtension` discards a failed entry point's code | `error during initialization: cr-sqlite init: database is locked` — on this path the message is the host's only signal |

The entry point itself returns `SQLITE_BUSY` on both paths; it is SQLite's `load_extension` wrapper
that turns it into `SQLITE_ERROR`. As with any failed open, the auto-extension path leaves a handle the
host must close.

### Error paths, teardown and the rest of the extension

- **Every early return** in `sqlite3_crsqlcore_init` and `sqlite3_crsqlite_init` follows the ownership
  rules in **Init Error-Path Frees**, and the `InitTxn` guard adds the rollback. Rolling back with
  prepared-but-not-stepped persistent statements (`crsql_newExtData`'s) outstanding is fine.
- **`crsql_finalize` / teardown and the E1 close hook** (`crsql_close_trace_hook`) are installed after
  the handoff, on success only, as today. Because no failure path leaves an unfinalized statement, a
  failed open needs neither.
- **`crsql_automigrate`** is a SQL function a host calls after open. It does not run during init.
  Unaffected.
- **STRICT tables.** `crsql_tracked_peers` and `crsql_db_versions` are `STRICT`. DDL inside a
  transaction behaves identically, and the heal's row-count reads work on STRICT tables.
- **Static vs dynamic loading.** Static (auto-extension): init runs in `sqlite3_open_v2`, no host
  transaction possible, read-transaction fast path and `BEGIN IMMEDIATE` slow path, under the
  **Init Busy Timeout** when registered through `sqlite3_crsqlite_autoext_init`. Dynamic
  (`load_extension`): the host may be in a transaction, so the `SAVEPOINT` path is used, under the
  host's handler; a failed load leaves the connection unusable (**Failed Load Unmapped Callback**). The
  host's double-init guard (static plus dynamic on one connection) is a separate matter and unchanged.
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
- **libSQL build (`#ifdef LIBSQL`).** Same Rust path; the new entry point's signature is covered under
  **Init Busy Timeout**.

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
- `make valgrind` runs the same binary under valgrind with **no options**, so it reports but can never
  fail. Add a strict target (**What to run**). `make asan` needs nightly (`STABLE_RUST_PORT.md`).
- The test binary is compiled with **`-DSQLITE_THREADSAFE=0`** and `-DUNIT_TEST=1`, with cr-sqlite
  registered as an auto-extension via `-DSQLITE_EXTRA_INIT=core_init`. So:
  - every `sqlite3_open` in a test runs init — through `sqlite3_crsqlite_autoext_init`, so under the
    **Init Busy Timeout** — and a test that needs a connection **without** cr-sqlite must call
    `sqlite3_reset_auto_extension()`, open, then re-register the entry point with
    `sqlite3_auto_extension`. That is easiest from a C suite (`src/crsqlite.test.c` or a new
    `src/init-race.test.c`);
  - **threads are not allowed** in this binary. Contention must be built from
    **several connections on one thread**, which SQLite's unix VFS locks against each other as it would
    across threads. A test that blocks an open on purpose waits out the budget, so it sets the test-only
    override.
- `make vanilla` builds `dist/vanilla-sqlite3`, a shell **without** cr-sqlite; the stress script uses
  it to inspect files cr-sqlite-free.
- `py/correctness` (pytest, against `make loadable`) is the other harness; it loads the extension with
  Python's `Connection.load_extension` (the C API, not `SELECT load_extension`). Its pins
  (`requirements.txt`) need **Python ≥ 3.10**, because pytest 9 does. Run it as
  `cd py/correctness && PYTHONPATH=src python -m pytest tests` from a Python 3.10+ venv. ⚠️ Its prior-version
  test, `py/correctness/tests/test_prior_versions.py`, is **entirely commented out** ("We no longer
  support these versions"), so `pytest -k prior` runs nothing. The prior-version refusals are covered by
  the C tests below instead.

### Deterministic tests — in `make test`

Name them so a grep finds them all: prefix `init_race_`.

| test | setup | assertion |
|---|---|---|
| `init_race_heals_field_sample` | copy `core/src/fixtures/halfinit.halfinit-db` (the preserved Half-Initialized Database, 28,672 bytes) to a temp file; open | open succeeds; version row = `CRSQLITE_VERSION`; one ordinal-0 site id; three triggers; unique index present. ⚠️ Fails today with the version-0 error — the red half of red→green |
| `init_race_heals_synthesized_residue` | plain connection writes the three tables with the **exact** DDL from `bootstrap.rs`, empty; close; open with cr-sqlite | same as above |
| `init_race_heal_refuses_v0_12` | copy `py/correctness/prior-dbs/v0.12.0.prior-db` | open still fails with the version-0 message; file bytes unchanged |
| `init_race_heal_refuses_v0_13_v0_15` | copy the other two fixtures | unchanged error (version 130000 / 150000); file bytes unchanged |
| `init_race_heal_refuses_nonempty` | residue plus one row in `crsql_tracked_peers`; then separately one `config.x` row in `crsql_master` | open fails, as today |
| `init_race_heal_refuses_crsql_object` | residue plus an empty `foo__crsql_clock` table; then separately an empty `Foo__CRSQL_Clock` table | open fails, as today. The mixed-case name pins the case-insensitive match; the `case_sensitive_like` half needs `load_extension`, so it lives in `py/correctness` (`test_init_race_heal_case_sensitive_like`, below) |
| `init_race_heal_app_tables` | residue plus an application table with rows | heals (broad scope); app rows untouched |
| `init_race_atomic_at_each_step` | fault injection (**Fault injection**) fails init at each persistent step, from `crsql_init_peer_tracking_table` through `create_site_id_triggers`, and at `COMMIT` | open fails; afterwards `sqlite_master` contains **no** `crsql` object; next open succeeds |
| `init_race_error_paths_free_once` | the same injection at every registration and at the `crsql_changes` module, before and after the commit, including the handoff itself; each open is closed with `sqlite3_close_v2`, as Exqlite does | open fails, close returns `SQLITE_OK`; under the strict valgrind target: no leak, no invalid free, no use after free |
| `init_race_late_failure_no_zombie` | for every injection point from `crsql_newExtData` on (including `crsql_changes` and the handoff): count the process's open file descriptors (`/proc/self/fd` on Linux, `/dev/fd` on macOS and the BSDs, each used only if opening one more file adds exactly one entry; reported as **SKIPPED**, never ok, where no directory qualifies — Windows, and FreeBSD without `fdescfs`, whose `/dev/fd` lists only 0–2), open a file-backed database with the injection set, `sqlite3_close_v2` it | the descriptor count is back to its value before the open — a zombie keeps the database descriptor open. Pins the **Init Error-Path Frees** zombie |
| `init_race_loser_writes_nothing` | plain, cr-sqlite-free connection P holds `BEGIN IMMEDIATE` on a fresh file; budget overridden to 200 ms; open C with cr-sqlite; time the open with a monotonic clock | C's open fails; ⏱ **elapsed ≥ 150 ms** (it waited, rather than failing at once) and **elapsed < 200 ms + 1 s** (it gave up within the budget); after P rolls back the file is still empty. ⚠️ Before the fix C fails at once and the file is also empty, so **only the lower timing bound** distinguishes the fix |
| `init_race_commit_busy` | P is a **plain, cr-sqlite-free** connection that holds `SHARED` (`BEGIN; SELECT count(*) FROM sqlite_master;`) on a fresh file; budget 200 ms; open C with cr-sqlite; time it | C's `COMMIT` waits out the budget (P cannot release on one thread) and rolls back: ⏱ **elapsed ≥ 150 ms** and **< 200 ms + 1 s**; the file is still empty; after P ends, C's next open succeeds. ⚠️ Before the fix C's first separately committed `CREATE TABLE` fails at once and rolls back, leaving the file empty too, so again **only the lower timing bound** is red before the fix |
| `init_race_reopen_writes_nothing` | fully initialized file; open it read-write while P holds `BEGIN IMMEDIATE`; separately open it read-only (`SQLITE_OPEN_READONLY`) | both opens succeed at once (elapsed well under the budget). ⭐ The read-write half is the one that proves the fast path takes no write lock; the read-only half cannot detect an over-strict check (**Keeping the re-open path read-only**) |
| `init_race_config_busy_fails` | initialized file with `merge-equal-values` set; set the `UNIT_TEST` hook `crsql_test_force_config_step_rc = SQLITE_BUSY` (below); open; reset the hook | open fails rather than succeeding with the default |
| `init_race_heal_refuses_crsql_named_trigger` | residue plus an application table and a **trigger** named `crsql_master` | open fails with the version-0 message; file bytes unchanged. Pins that heal rule 3 exempts the three names only as tables |
| `init_race_busy_reports_code_and_message` | P holds `BEGIN IMMEDIATE` on a fresh file; budget 100 ms. (a) auto-extension open; (b) a plain connection calls `sqlite3_crsqlite_init` directly, as `sqlite3_load_extension` does; (c) the same inside a host transaction (`BEGIN; SELECT …`), i.e. the `SAVEPOINT crsql_init` path | (a) `SQLITE_BUSY` and `automatic extension loading failed: cr-sqlite init: database is locked`; (b) and (c) `SQLITE_BUSY` and `cr-sqlite init: database is locked`, freed with `sqlite3_free`; no cr-sqlite object written. Under the strict valgrind target: no leak or mismatched free (**Init Failure Code and Message**) |
| `init_race_other_failure_reports_code_and_message` | an empty file (created by a plain open); open it read-only with cr-sqlite | `SQLITE_READONLY` and `automatic extension loading failed: cr-sqlite init: attempt to write a readonly database` |
| `init_race_version_read_is_64_bit` | initialized file whose recorded version is set to 2^31 | the open succeeds and leaves the value alone. A 32-bit read in the sequence refused it as "version -2147483648" while the completeness check accepted it |
| `test_init_race_busy_load_message` (`py/correctness`, two cases: autocommit, and inside a host transaction) | keeper connection; a holder holds `BEGIN IMMEDIATE` on a fresh file; a connection with no busy timeout calls `load_extension` | `OperationalError` with exactly `error during initialization: cr-sqlite init: database is locked` (SQLite's `load_extension` always reports `SQLITE_ERROR`); the host's transaction state is unchanged |
| `init_race_host_txn_preserved` | not testable here — the test binary sets `SQLITE_OMIT_LOAD_EXTENSION`. In `py/correctness`: open a **keeper** connection and load the extension on it (`:memory:`); then on a copy of `v0.12.0.prior-db`, `BEGIN`, insert a row into a host table, `load_extension` (fails: v0.12 is refused); close the keeper only after this connection | the host's row is still there; the host's transaction is still open |
| `test_init_race_heal_case_sensitive_like` (`py/correctness`) | keeper connection as above; on a residue file plus an empty `Foo__CRSQL_Clock` table, `PRAGMA case_sensitive_like = ON`, then `load_extension` | the load fails (not healed) |
| `test_init_race_failed_load_close` (`py/correctness`) | **no keeper**: in a **subprocess**, `load_extension` on a copy of `v0.12.0.prior-db` (fails), then close the connection and exit 0 | the subprocess exits 0. ⚠️ Red before the fix: closing calls `crsql_sqlite_free` in the unloaded library, so the subprocess crashes. The subprocess keeps a crash from taking pytest down |

**Why `init_race_config_busy_fails` needs a hook.** On one thread, any lock that blocks the
`config.%` read also blocks the earlier reads of init, which fail first, so a lock-based setup can never
reach the loop and the test would pass whether or not the loop is fixed. Instead, under `UNIT_TEST`,
`crsql_newExtData` reads a file-static `int crsql_test_force_config_step_rc` (0 = off); when it is
non-zero, the loop's step returns that code instead of calling `sqlite3_step`. A test VFS that fails
only that read would also work, but costs far more.

⭐ **A free red→green.** The "file bytes unchanged" assertions in `init_race_heal_refuses_v0_12` and
`init_race_heal_refuses_v0_13_v0_15` **fail today**, not only after a regression: none of the three
prior-version fixtures has `crsql_db_versions`, and `crsql_init_db_versions_table` commits it before
the version check refuses the file. Under the **Atomic Init Transaction** the refusal happens inside
the transaction, which rolls back, so the bytes stay unchanged.

### Fault injection

A `#[cfg(feature = "test")]` static in `test_exports.rs` holds the step to fail (0 = none). It is set
from C through a test-only export, `void crsql_test_fail_init_at(int step)`, and read by Rust and, for
the C-side steps, by `sqlite3_crsqlite_init` through `int crsql_test_init_fail_step(void)`.
`integration_check` already enables the `test` feature. It compiles to nothing in release builds. A
static is safe because the test binary is single-threaded.

⭐ **Every registration fails through SQLite's real failure path**, never by skipping the call: at the
injected step, call the real `create_function_v2` with the same name, flags, user data and destructor
but **`nArg = -2`**. `sqlite3CreateFunc` rejects `nArg < -1 || nArg > SQLITE_MAX_FUNCTION_ARG` with
`SQLITE_MISUSE`, and `createFunctionApi` then runs the destructor because the registration took no
reference — exactly what a real failure does. (Above the limit works too, but the limit is not in
`sqlite3.h` and differs: 127 in the bundled 3.42.0, 1000 in 3.54.1; `-2` is invalid in both.) Skipping
the call instead would leave the destructor un-run and fake a leak, or hide a double free. Module
registrations carry no destructor, and SQL steps none either, so those return the injected code without
calling.

The step numbering is a stable `#[repr(i32)]` enum, `InitStep`, in the order init runs. Each value is
a named point, not a position, so inserting a step later does not renumber the tests' expectations:

| value | step | injected result |
|---|---|---|
| 1 | early registrations (`crsql_set_debug` … `clset`), as one point | invalid `nArg` / module error |
| 2 | sync-bit registration (destructor `sqlite3_free`) | invalid `nArg` |
| 3 | fast-path read transaction `BEGIN` / the check | `SQLITE_BUSY` |
| 4 | `BEGIN IMMEDIATE` | `SQLITE_BUSY` |
| 5 | `crsql_init_peer_tracking_table` | `SQLITE_BUSY` |
| 6 | `crsql_init_db_versions_table` | `SQLITE_BUSY` |
| 7 | `crsql_create_schema_table_if_not_exists` | `SQLITE_BUSY` |
| 8 | the version write | `SQLITE_BUSY` |
| 9 | `crsql_newExtData` | `SQLITE_BUSY` |
| 10 | `crsql_update_site_id` registration | invalid `nArg` |
| 11 | `crsql_init_site_id` | `SQLITE_BUSY` |
| 12 | `crsql_initSiteIdExt` | `SQLITE_BUSY` |
| 13 | `create_site_id_triggers` | `SQLITE_BUSY` |
| 14 | `COMMIT` | `SQLITE_BUSY` |
| 15 | the post-commit registrations without a destructor, as one point | invalid `nArg` |
| 16 | `crsql_changes` module (`sqlite3_crsqlite_init`) | `SQLITE_ERROR` |
| 17 | the ownership handoff (`crsql_db_version`) | invalid `nArg` |

`InitStep::COUNT` (17) is asserted by a test, so adding a step without a value fails the build. The
tests loop over every value.

**Mutation-verify the important ones.** With the `InitTxn` commit moved back to before
`crsql_init_site_id`, `init_race_atomic_at_each_step` must fail. With the heal predicate weakened to the
naive rule, `init_race_heal_refuses_v0_12` must fail. With rule 3 written as `LIKE`,
`test_init_race_heal_case_sensitive_like` must fail. With a `crsql_freeExtData` call restored after the
handoff, `init_race_error_paths_free_once` must fail under the strict valgrind target. With a fallible
step added after the handoff that frees nothing, `init_race_late_failure_no_zombie` must fail. With the
`config.%` loop restored to `while (sqlite3_step(pStmt) == SQLITE_ROW)`, `init_race_config_busy_fails`
must fail. With `crsql_sqlite_free` restored as the sync-bit destructor, `test_init_race_failed_load_close`
must fail.

### Concurrency stress test — separate from `make test`

The real race needs genuine parallelism, which the single-threaded test binary cannot provide. Two
places to run it:

1. **In this fork: a multi-process harness.** `make sqlite3` builds `dist/sqlite3` with cr-sqlite
   statically linked and auto-loaded through `core_init.c`; `make vanilla` builds the cr-sqlite-free
   `dist/vanilla-sqlite3`. A Bash script, `tools/init_race_stress.sh`, takes rounds R and processes N.
   - **A barrier that cannot hang.** Per round: `mkfifo "$fifo"`, then hold it open read-write in the
     script with `exec 3<>"$fifo"`, so opening it never blocks either side. Launch N children, each
     wrapped in its own `timeout 30`, each of which reads exactly one line from fd 3 (`read -r -u 3`;
     Bash reads a FIFO byte by byte, so each child takes one line) and then `exec`s `dist/sqlite3
     "$db" 'SELECT crsql_site_id();'` with stderr to a per-process file. Release all of them at once
     with **one** write of N newlines (`printf '\n%.0s' $(seq "$N") >&3`). Close fd 3 and remove the
     FIFO after `wait`. A child that dies before reading cannot strand the others, because the script
     still holds both ends.
   - **Classify every process by its stderr, not its exit code.** The shell exits 1 for an open failure
     and for a failed statement alike. An open failure prints `Error: unable to open database "…": …`;
     an init failure adds `automatic extension loading failed`, and the wedge's refusal adds `version 0
     is not supported`. In the initialized-file mode, the switcher process runs `PRAGMA
     journal_mode=WAL` after a successful open and can fail with `database is locked`: that is a pragma
     failure, counted separately, not a failed open. A `timeout` kill (exit 124) is counted as a hang.
   - ⛔ **Inspect each file before anything with cr-sqlite opens it.** A cr-sqlite open after the fix
     runs the heal, which repairs a wedged file first, so "zero wedges" measured that way could never
     fail. After each round, copy the database and any `-journal`/`-wal` to a scratch path and inspect
     the **copy** with `dist/vanilla-sqlite3` (or the system `sqlite3`): read-write on the copy, so a hot
     journal is rolled back exactly as the next opener would. Classify it as **untouched** (no `crsql`
     object), **complete** (the four tables, the version row, an ordinal-0 site id, the three triggers)
     or **wedged/partial** (anything else; a wedge is `crsql_master` without a `crsqlite_version` row).
     Only then make the quiet cr-sqlite open, and count its result separately.
   - **Modes.** A fresh file; an initialized WAL file at pool start; an initialized rollback-journal
     file while one process switches it to WAL.
   - ⛔ **Run it on the current `main` first and confirm it reproduces** — failed opens in every mode,
     and at least one wedge in 1,500 rounds of 10 (the app's in-process probe found 5) — before trusting
     its "after" numbers. If it does not reproduce: run it on a disk-backed directory as well as tmpfs
     (`fsync` cost changes the window), raise N (20, then 50) and R (5,000), and check that the barrier
     actually releases the processes together (stderr timestamps). Record which setting reproduced.
   - Then after the fix: **zero wedges and zero failed opens** in every mode. Keep it out of `make test`
     (it is slow and timing-dependent); run it by hand and record the numbers in this doc.
2. **In `silicon_brain`:** the probe, folded into a durable app test that exercises the real NIF and
   Exqlite's threads, in both its builds. That belongs to the integration doc.

### What to run

Add a strict valgrind target to `core/Makefile`, leaving `make valgrind` as it is:

```
valgrind-strict: $(TARGET_TEST)
	valgrind --error-exitcode=1 --leak-check=full \
	  --errors-for-leak-kinds=definite,indirect --show-leak-kinds=definite,indirect \
	  $(prefix)/test
```

(`valgrind-strict: bundle = integration_check`, like the other test targets.) ⛔
**Record a baseline on `main` before changing any code:** run `make valgrind-strict` on the unmodified tree. If it is not clean,
write down each leak record here, then either fix it or add a suppressions file that names exactly those
records (`--suppressions=`), so that only new leaks fail. Leaked file descriptors are checked by
`init_race_late_failure_no_zombie`, not by valgrind.

```
cd core
make test                      # all suites; or: dist/test rust_integration / dist/test crsql
make valgrind-strict           # fails on definite or indirect leaks and on memory errors
make loadable && (cd ../py/correctness && pytest -k init_race)
make sqlite3 vanilla && ../tools/init_race_stress.sh 1500 10   # by hand, before and after
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
  busy handler is set from C.
- **The fixture** `core/src/fixtures/halfinit.halfinit-db` is committed (`00c3fae4`). It deliberately
  does not end in `.db`, which `.gitignore` ignores, and it follows the
  `py/correctness/prior-dbs/*.prior-db` convention.
- **Append `e4-init-atomic` to `BUILD_ID`** in `core/rs/core/src/sha.rs`, and document it there and
  in the README's token table:
  `e1-fd-close+e2-tbl-pushdown+e3-tbl-collate-gate+e4-init-atomic <sha>`. Hosts gate with a substring
  match, so existing gates on `e2-tbl-pushdown` keep working.
- **README:** add a change 4 under "Infocorder fork", with its benefit and its cost (opens through
  `sqlite3_crsqlite_autoext_init` can spend up to 2 s waiting for a competing connection; a
  Half-Initialized Database now opens). Say that static hosts must register
  `sqlite3_crsqlite_autoext_init` to get the wait, that it must never be used with `load_extension`, and
  that a failed `load_extension` leaves the connection unusable (**Failed Load Unmapped Callback**).
- **Coordination doc:** when built, append a **Pin this** section with SHA, verification and the
  downstream steps. The two copies of `docs/CRSQLITE_FORK_UPGRADE_COORDINATION.md` are reconciled:
  byte-identical, and tracked here since `1a6b0613` (**Coordination Doc Drift** is closed). ⚠️ This
  repo's `.gitignore` still lists the file on its second line; it no longer hides a tracked file, but it
  would hide a re-created one, so remove that entry. Append in one repo and copy to the other.
- **Verification to record:** the `main` valgrind baseline; `make test` and `make valgrind-strict`
  green; `py/correctness` green; the stress numbers before and after, in every mode, with the setting
  that reproduced; the `crsql_build_id()` output.

---

## Build record — what was built, and the choices the plan left open

Built 2026-11-13 on stable Rust 1.95.0 (asdf), gcc, Linux x86_64, against this fork's HEAD `8d153fc2`,
and committed as `578ef5d6`. The **Code review follow-up** below is committed in `f39caadd`; the
dependency bumps in `85faa868` and a test fix in `ba3c7ff5` followed. The fix for the release blocker
those bumps introduced (**Packed Blob Length Sign-Extension**, below) is committed in `b8c6ffb1` and
`75d42269`. The fixes for three older peer-reachable defects (**Merge Input Validation**, below) are in
the working tree, awaiting the operator's commit. The release SHA to pin is the commit that includes
them; the coordination doc's **Pin this** table carries a provisional pin until then.

### Where the code is

| part | location |
|---|---|
| **Atomic Init Transaction** | `lib.rs`: `init_persistent_state` (fast path, slow path, host-transaction savepoint), `init_sequence` (the existing sequence, in its existing order), `commit_or_free`. `bootstrap.rs`: `InitTxn` (the RAII guard) and `is_init_complete` (the **Init Completeness Check**) |
| **Init Residue Heal** | `bootstrap.rs`: `is_init_residue`, called from `maybe_update_db_inner` |
| **Init Busy Timeout** | `crsqlite.c`: `sqlite3_crsqlite_autoext_init`, `crsql_init_busy_handler`, `CRSQL_INIT_BUSY_TIMEOUT_MS` = 2000. Declared in `ext.h`; registered by `core_init.c` |
| **Init Error-Path Frees**, the **ownership handoff** | `lib.rs`: `crsql_register_db_version` (a new internal export, not API), called by `sqlite3_crsqlite_init` after `crsql_changes`; the comment there says nothing fallible may follow |
| **Failed Load Unmapped Callback** | `lib.rs`: `sqlite_free_destructor` (`api.free` on the loadable path, the `sqlite3_free` symbol statically) |
| **Init Host Rollback** | `bootstrap.rs`: `ROLLBACK TO <savepoint>; RELEASE <savepoint>` in both init savepoints; `InitTxn`'s savepoint form |
| `config.%` loop | `ext-data.c`: fails the open on any final step result but `SQLITE_DONE`; finalizes the statement on the broken-setting and prepare-failure paths |
| **Init Failure Code and Message** | `crsqlite.c`: `crsql_init_failed`, and `initRc` passed to `sqlite3_crsqlrustbundle_init`. `bundle/src/lib.rs` and `lib.rs` `sqlite3_crsqlcore_init`: the `rc_out` parameter. `lib.rs` `init_sequence` and `init_rc`, `bootstrap.rs` `crsql_maybe_update_db`, `crsql_create_schema_table_if_not_exists` and `crsql_init_site_id`: each passes its real code up |
| fault injection | `init_fault.rs` (`InitStep`, `fail_at`, `n_arg`), `test_exports.rs` (the static, `crsql_test_fail_init_at`, `crsql_test_init_fail_step`); C hooks declared in `crsqlite.h` under `UNIT_TEST` |
| tests | `core/rs/integration_check/src/t/init_race.rs`; `py/correctness/tests/test_init_race.py`; `tools/init_race_stress.sh` |

### Choices the plan left open (each the conservative one)

- **The `init_race_*` tests run as their own C suite**, `dist/test init_race` (`SUITE("init_race")` in
  `tests.c`), not inside `rust_integration`. Each test returns a result, so one run reports every
  failure; `CRSQL_INIT_RACE_ONLY=<name>` runs one test, and two diagnostic switches exist:
  `CRSQL_INIT_RACE_VERBOSE=1` and `CRSQL_INIT_RACE_SKIP_STEP=<n>`. That was needed to see every red test
  on the unfixed code, where step 17 aborts the process with a double free.
- **A connection without cr-sqlite** is opened by `sqlite3_reset_auto_extension()`, open, then
  `core_init(0)`, which re-registers whichever entry point the build uses.
- **Three tests beyond the plan's table:**
  - `init_race_step_count` asserts `InitStep::COUNT`.
  - `init_race_config_broken_setting_closes` pins the statement leak on the broken-setting path.
  - ⭐ `init_race_budget_is_per_open` is the only test that can tell the per-open budget from a per-wait
    timeout. On one thread, every other blocking test fails at its first exhausted wait, so a per-wait
    `sqlite3_busy_timeout` passes them all. This test installs a default-VFS shim whose `xSleep` counts
    the time slept. When the open has slept 500 ms at `BEGIN IMMEDIATE`, the lock holder swaps its write
    lock for a read lock, so the open reaches `COMMIT` and waits again. It asserts the total time slept,
    not wall time, so it is deterministic under valgrind.
- **Fault-injection placement:**
  - `InitStep` lives in its own module, `init_fault.rs`; the static is in `test_exports.rs`.
  - `ReadTxn` (3) fires after `BEGIN DEFERRED`, so the guard's rollback runs.
  - `NewExtData` (9) fires in C after allocation and the prepares, so the free path is exercised.
  - `InitSiteIdExt` (12) runs the real call and replaces its result, so the failure happens with the
    buffer already owned by the ext data.
  - `Commit` (14) is the `COMMIT` that ends the sequence's transaction, on either path. It is not the
    commit that only releases the read transaction before the slow path.
- **The heal predicate runs only when `crsql_master` has no `crsqlite_version` row**, inside
  `maybe_update_db_inner`, so it never runs on the fast path, where the check requires that row. Rule 2
  checks each table's existence first, as written.
- **The completeness check** compares `CAST(value AS INTEGER) >= CRSQLITE_VERSION`, a 64-bit read; the
  sequence reads the same value with `column_int64` (it read `column_int`, which truncated above 2^31,
  until the **Code review follow-up**).
- **The fast-path/slow-path loop is bounded at three passes.** A file that keeps flipping between
  complete and incomplete fails the open with `SQLITE_BUSY` rather than looping.
- **Site-ID Index Gap:** closed for every database initialized from now on. An existing file whose
  `crsql_site_id` already lacks the unique index is **not** repaired, and the check does not require the
  index. Creating a unique index on an existing table can fail on duplicates, which would turn a working
  open into a failing one.
- **The busy handler counts `max(requested, reported)` sleep.** On a VFS that rounds sleeps up to whole
  seconds, the budget then stays a bound on the time actually slept.
- **`-DHAVE_USLEEP=1`** is in the test binary, `dist/sqlite3` and the asan test binary, which runs the same
  timing tests.
- **The `py/correctness` tests run with `PYTHONPATH=src`.** The harness package is not installed in this
  environment. Each test that fails a load on purpose closes its connection in a `finally` block, before
  the keeper releases the library. Without that, an assertion failure left the connection to be closed
  after the keeper, which crashed pytest with exactly the defect under test.
- **Small fixes found on the way:** `crsql_newExtData` now checks its allocation, and finalizes the
  `config.%` statement when an earlier prepare failed. The site-id buffer allocation is checked.
- **Not changed:** `crsql_as_table` still recovers with a bare `ROLLBACK`. It is the same class of defect
  as **Init Host Rollback**, but it is not init, and this release keeps its diff to init.

### Verification

**Baseline on unchanged code (`8d153fc2`):**

| check | result |
|---|---|
| `make test` | green: 32 C test cases and 11 Rust suites |
| `make valgrind-strict` | 0 errors, 0 bytes in use at exit. No suppressions needed |
| `py/correctness` | 156 passed |

**Red before the fix.** The test plumbing was added at the existing call sites first, with no behaviour
change:

| test | how it failed on unchanged code |
|---|---|
| `heals_field_sample`, `heals_synthesized_residue`, `heal_app_tables` | the version-0 refusal |
| `heal_refuses_v0_12`, `heal_refuses_v0_13_v0_15` | the file bytes changed (the **free red→green**) |
| `atomic_at_each_step` | residue left at steps 2 and 6–13; steps 3, 4 and 14 do not exist yet |
| `error_paths_free_once` | a double free at step 17, which aborts the binary; with step 17 skipped, steps 3, 4 and 14 also fail |
| `late_failure_no_zombie` | zombie file descriptors at steps 13 and 16; a double free at 17 |
| `loser_writes_nothing`, `commit_busy` | gave up after 0–1 ms |
| `config_busy_fails` | the open succeeded |
| `config_broken_setting_closes` | leaked a file descriptor |
| py `host_txn_preserved` | the host transaction was rolled back |
| py `failed_load_close` | the subprocess died with SIGSEGV |

Green before the fix, as expected: `reopen_writes_nothing`, `heal_refuses_nonempty`,
`heal_refuses_crsql_object`, and py `heal_case_sensitive_like`. These guard against regressions rather
than show the defect.

**After the fix:**

| check | result |
|---|---|
| `make test` | green: 32 C test cases, 17 `init_race`, 11 Rust suites |
| `make valgrind-strict` | 0 errors, 0 bytes in use, all 17 `init_race` passing under valgrind |
| `py/correctness` | 159 passed, including the 3 `init_race` tests |

**Stress** (`tools/init_race_stress.sh 1500 10`, tmpfs, 10 processes, barrier spread ≤ 11 ms). It
reproduced on the first setting tried, so no fallback setting was needed.

| mode | before (`8d153fc2`) | after |
|---|---|---|
| fresh | 13,765 / 15,000 opens failed, 1500 / 1500 rounds; **57 files wedged** (every later open failed), 248 partial | 0 failed opens, 0 wedged, 0 partial, 0 hangs |
| initialized WAL | 76 failed opens in 61 rounds | 0 failed opens |
| rollback, switching to WAL | 1,167 failed opens in 456 rounds; 645 switcher `PRAGMA` lock failures | 0 failed opens; 1,048 switcher `PRAGMA` lock failures |

The switcher's `PRAGMA journal_mode=WAL` runs after a successful open, with no host busy timeout in the
shell, so its lock failures are counted separately and are not init's.

**Mutation verification.** Each part was reverted, built, tested and restored in one command. The tree was
then compared against a snapshot.

| guarantee | mutation | result |
|---|---|---|
| the transaction | `COMMIT; BEGIN IMMEDIATE` inserted before `crsql_init_site_id` | `atomic_at_each_step` red at steps 11–14 |
| heal rule 3 | rule 3 disabled | `heal_refuses_v0_12` and `heal_refuses_crsql_object` red |
| heal rule 3 via `LIKE` | `name LIKE '%crsql%'` | py `heal_case_sensitive_like` red |
| the busy timeout | handler not installed | `loser_writes_nothing`, `commit_busy` and `budget_is_per_open` red |
| the per-open budget | `sqlite3_busy_timeout(db, budget)` | `budget_is_per_open` red (slept 1528 ms against a 1000 ms budget); the plan's two timing tests stay green |
| error-path frees: the double free | `crsql_freeExtData` after a failed handoff | `error_paths_free_once` red under strict valgrind: 90 errors, `Invalid free` |
| error-path frees: the leak | the free removed from the `crsql_changes` failure path | `late_failure_no_zombie` red at step 16 |
| the ownership handoff | `crsql_db_version` registered right after `crsql_site_id` | double free at step 16 in `error_paths_free_once` and `late_failure_no_zombie` |
| the `config.%` loop | final step result ignored | `config_busy_fails` red |
| the sync-bit destructor | `crsql_sqlite_free` restored | py `failed_load_close` red |
| nothing fallible after the handoff | the step-16 failure moved after the handoff | `late_failure_no_zombie` red |

**`crsql_build_id()`:** `e1-fd-close+e2-tbl-pushdown+e3-tbl-collate-gate+e4-init-atomic <sha>`.

**Not verified here:** macOS and Windows builds; the libSQL build (this fork has none); a disk-backed
stress run (tmpfs reproduced, so the fallback was not needed).

### Code review follow-up

A code review of `578ef5d6` found five fixable defects. The operator approved fixing them on
2026-11-13; the fixes are committed in `f39caadd`. Built the same way (stable 1.95.0, the
populated cargo cache, offline). Each new test was red before its fix.

| finding | fix | test — how it failed before the fix |
|---|---|---|
| a lock failure returned rc 1 with an empty message | **Init Failure Code and Message**: the real code travels from the Rust init to the entry points through `rc_out`; every step of the sequence passes its own code up; `crsql_init_failed` sets `cr-sqlite init: <errstr>` with `sqlite3_mprintf` | `init_race_busy_reports_code_and_message`: rc 1, `automatic extension loading failed: `; `init_race_other_failure_reports_code_and_message`: the same; py `test_init_race_busy_load_message` (both cases): `error during initialization: ` |
| the spec said the heal runs only under the lock | spec text corrected (**Init Residue Heal**): it also runs on the `SAVEPOINT crsql_init` path, safely | — |
| status text out of date | this record, the **Status** line, and the coordination doc's section 14 | — |
| `init_race_late_failure_no_zombie` passed silently off Linux | `fd_count` falls back to `/dev/fd`; where neither exists the test, and the fd half of `init_race_config_broken_setting_closes`, report **SKIPPED**. The runner prints `ok` / `FAILED` / `SKIPPED` and a summary line (`init_race: N passed, N skipped, N failed`); a skip does not fail the suite | not a red-first fix; checked by mutation below |
| heal rule 3 exempted the three names whatever the object type | exempt only `type = 'table'` objects with those names, plus their `sqlite_autoindex_*` indexes | `init_race_heal_refuses_crsql_named_trigger`: the open succeeded (healed) |
| the sequence read the version with `column_int` | `column_int64`, matching the completeness check | `init_race_version_read_is_64_bit`: refused as "version -2147483648" |

**Verification after the follow-up:**

| check | result |
|---|---|
| `make test` | green: 32 C test cases, 21 `init_race` (21 passed, 0 skipped), 11 Rust suites |
| `make valgrind-strict` | 0 errors, 0 bytes in use at exit, all 21 `init_race` passing under valgrind |
| `py/correctness` | 161 passed (159 plus the two cases of `test_init_race_busy_load_message`) |
| `tools/init_race_stress.sh 200 10` (tmpfs, all three modes) | fresh: 0 failed opens, 0 wedged, 0 partial, 0 hangs; WAL: 0 failed; switching: 0 failed opens (132 switcher `PRAGMA` lock failures, which are not init's) |

**Mutations**, each bounded with `timeout`, restored in the same command and checked with `cmp`:

| guarantee | mutation | result |
|---|---|---|
| code propagation, C side | `crsql_init_failed(pzErrMsg, SQLITE_ERROR)` instead of `initRc` | `busy_reports_code_and_message` and `other_failure_reports_code_and_message` red (rc 1) |
| code propagation, Rust side | the `*rc_out` write removed | the same two red |
| code propagation inside the sequence | the peer-table step returns `SQLITE_ERROR` | `busy_reports_code_and_message` red on the host-transaction case; `other_failure_…` red |
| heal rule 3 by type | the `type = 'table'` condition removed | `heal_refuses_crsql_named_trigger` red |
| 64-bit version read | `column_int(0) as i64` | `version_read_is_64_bit` red |
| fd fallback and SKIPPED | both fd directories made nonexistent | suite passes with `2 skipped`, both tests printed `SKIPPED: no fd directory …`; with only `/dev/fd` available, all 21 pass |

**Not verified:** a real `load_extension` under valgrind (the test binary omits `load_extension`; the
C test calls the entry point directly, as `sqlite3LoadExtension` does, and the py test checks the real
path's message but not its memory); macOS (`/dev/fd`) and Windows (the SKIPPED path) were exercised
only by the mutation above, on Linux.

**Left as they were (recorded, not fixed):** the review's other findings — that the busy wait depends
on the app's startup shim registering `sqlite3_crsqlite_autoext_init` (documented under
**Init Busy Timeout** and in the coordination doc); coarse sleeps on builds without `usleep`, such as
WASM; the site-id state after a host `ROLLBACK`, which predates this fix; and `crsql_as_table`'s bare
`ROLLBACK` (**Not changed**, above).

### Packed Blob Length Sign-Extension — the release blocker in `85faa868`, and its fix

**Committed** in `b8c6ffb1` (the Rust fix, tests and docs) and `75d42269` (the C clamp in `crsqlite.c`,
`ext-data.c` and `ext-data.test.c`), 2026-11-13. A review of `578ef5d6..ba3c7ff5`
found that the Dependabot bump of `bytes` from 1.5.0 to 1.11.1 (`85faa868`) broke the decoding of
packed primary keys. The bytes written never changed; reading them did.

**What broke.** A packed column stores a blob's or a text's length in the fewest big-endian bytes that
hold it. `unpack_columns` (`core/rs/core/src/pack_columns.rs`) read a **blob** length with
`buf.get_int(n)`. In `bytes` 1.5.0 that zero-extended; 1.11.1 sign-extends it. So a blob length whose
top encoded byte has bit 7 set — 128–255, 32768–65535 and 2^23 to 2^24−1 — decoded as negative, became
a huge `usize`, and the decode returned ABORT. For a table with blob primary keys of those lengths,
`SELECT * FROM crsql_changes` failed with `query aborted`, `crsql_unpack_columns` with
`SQL logic error`, and merges failed. An empty blob key (`x''`) made `get_int(0)` shift by 64, which
panics in a debug build and aborts the process. Text lengths and integer values already used
`get_uint` and were not affected.

**The fix.** Every integer that follows a type byte is decoded **zero-extended** (`get_packed_uint`):
the encoder writes the low *n* bytes, and it uses fewer than 8 only for a value whose dropped bytes are
zero (a length, or a non-negative integer); a negative integer always takes all 8 and carries its own
sign. Zero bytes return 0 without calling into `bytes`. `bytes` stays at 1.11.1.

The same review of the decoder found that a **malformed** package aborted the host process, release
build included, because `bytes` panics and this crate is built `panic = "abort"`: an empty package, or a
type byte declaring more than 8 length bytes with enough bytes following. A package arrives from a peer
in every `crsql_changes` merge, so a peer could crash a node. This predates the bump: verified by
running a `578ef5d6` build, where release and debug both died with SIGILL on the 5 malformed cases, and
on NULL, text and real primary-key values. Both now return an error.

The same change carries the review's smaller findings:

| finding | fix | test — how it failed before the fix |
|---|---|---|
| blob lengths 128–255, 32768–65535, 2^23 to 2^24−1 undecodable; `x''` panics in debug | `get_packed_uint` / `get_packed_bytes`, zero-extended, no call for 0 bytes | Rust `pack_columns` suite, `test_boundary_lengths`: the debug panic `attempt to shift left with overflow` at length 0. py `test_pack_columns_boundaries.py` against the HEAD build: 12 failed (`pack_unpack_round_trip` and `changes_read_and_merge` for blob 128, 255, 32768, 65535, 2^23, 2^24−1) |
| a malformed package aborts the process | an empty package, more than 8 length bytes, or too few bytes is ABORT | Rust `test_malformed_packages`; py `test_malformed_package_is_an_error_not_a_crash`: 5 cases killed their child process (SIGILL) |
| `init_rc` passed sqlite_nostd's sentinel `ResultCode::NULL` (5000) through: an empty stored site id gave `cr-sqlite init: unknown error`, and an auto-extension open returned 5000 & 0xff = 136 | **Init Failure Code and Message** now clamps: `init_error_code` (Rust, applied in `init_rc` and at the `rc_out` write) and `crsql_init_failed` (C) pass a real primary or extended code through and turn anything else into `SQLITE_ERROR` | `init_race_corrupt_site_id_reports_valid_code`: rc 136, `cr-sqlite init: unknown error`. `init_race_init_error_code_is_valid` pins the rule |
| `crsql_initSiteIdExt` summed the codes of its two prepares | returns the first failure | C `InitSiteIdExtReportsFirstFailure` (no `crsql_site_id` table, so both prepares fail): rc 2, `SQLITE_INTERNAL` |
| `fd_count` trusted any `/dev/fd`; FreeBSD without `fdescfs` lists only 0–2, so the zombie test would pass vacuously | a directory counts only if opening one more file adds exactly one entry; otherwise **SKIPPED** | not red-first; mutation below |
| the test binary declared `sqlite3_crsqlite_init` with `const void *` and no LIBSQL argument | declared as `core/src/ext.h` does (`*const sqlite3_api_routines`, plus the LIBSQL fourth argument under the `libsql` feature). The declaration is Rust, so it cannot include `ext.h` | — |
| `test_unpack_columns` asserted `i == i` (shadowed), checking nothing | compares the unpacked value with the input | — |

**Verification** (stable 1.95.0, Linux x86_64):

| check | result |
|---|---|
| `make test` | green: 33 C test cases, 23 `init_race` (23 passed, 0 skipped), 11 Rust suites |
| `make valgrind-strict` | 0 errors, 0 bytes in use at exit, all 23 `init_race` passing under valgrind |
| `py/correctness` | 265 passed (161 plus 104 in `test_pack_columns_boundaries.py`) |
| `tools/init_race_stress.sh 100 8` (all three modes) | 0 failed opens, 0 wedged, 0 hangs (switch mode: 63 switcher `PRAGMA` lock failures, which are not init's) |
| `make static`, `make loadable`, `make loadable_dbg` | build; the static archive still exports `sqlite3_crsqlite_autoext_init` |
| the reviewer's probes (`blobpk.py`, `emptyblob.py`) on the release and the debug `.so` | every length reads, unpacks and merges; `x''` round-trips with no crash |

**Mutations**, each bounded with `timeout`, restored in the same command and checked with `cmp`:

| guarantee | mutation | result |
|---|---|---|
| zero-extended blob length | the blob arm decodes with `get_int` again (0 bytes still guarded) | `pack columns suite: ABORT` at blob 128 |
| zero-extension for every field | `get_packed_uint` uses `get_int` | the existing integer case red: `assertion failed: i == 10000000` |
| at most 8 length bytes | the `nbytes > 8` check removed | panic `size too large … nbytes is 9` (process abort) |
| empty package | the empty check removed | panic `advance out of bounds` (process abort) |
| both clamps | `init_error_code` and `crsql_init_failed` pass the code through | `corrupt_site_id_reports_valid_code` red (rc 136, `unknown error`); `init_error_code_is_valid` red |
| first failure | `crsql_initSiteIdExt` sums again | `InitSiteIdExtReportsFirstFailure` red (`rc == SQLITE_ERROR` assertion) |
| fd directory probe | both fd directories replaced by `/`, which does not track opens | with the probe: `late_failure_no_zombie` SKIPPED; with the old unprobed count: it passes, vacuously |

⚠️ **Two layers by design.** With only the C clamp removed, `corrupt_site_id_reports_valid_code` stays green: the Rust clamp
already delivers a valid code, and no C-side step returns an invalid one. The C clamp is a backstop and
is not independently tested. Likewise the zero-byte early return is a backstop: `get_uint(0)` is safe in
`bytes` 1.11.1, so removing it alone is not red.

**Not changed:** a stored site id of the wrong non-zero length (for example 15 bytes) is still reported
as `cr-sqlite init: column index out of range` (`SQLITE_RANGE`), a valid code with an unhelpful
message. Text in a package is still taken as UTF-8 unchecked, as before.

**Not verified:** macOS, Windows and FreeBSD (the `fdescfs` case was exercised only by the mutation
above, on Linux).

### Merge Input Validation — three peer-reachable defects, and their fixes

**In the working tree, awaiting the operator's commit** (2026-11-13; approved by the operator the same
day). A crash-safety review of the **Packed Blob Length Sign-Extension** fix fuzzed `crsql_changes`
merges and found no remaining crash, but it found three older defects that any peer can trigger, because
every row inserted into `crsql_changes` comes from another node. None is caused by the earlier fixes. All
three are fixed in `merge_insert` (`core/rs/core/src/changes_vtab_write.rs`), before the merge writes
anything.

**Packed PK Arity Check** — data loss. A change whose packed primary key had **fewer** columns than the
table's primary key was accepted. Every merge statement binds the package by position, so the missing key
columns bound as NULL and the merged value landed in a key column: for `t(id PRIMARY KEY, v)`, the change
`pk = x'00'` (zero columns), `val = 'X'` wrote the row `('X', 'X')`, recorded `id = NULL` in
`t__crsql_pks`, and `crsql_changes` then emitted a NULL key that every peer merged in turn. A package with
**more** columns failed with an unexplained `column index out of range`. Now a package whose column count
differs from the primary key's is refused with `SQLITE_ERROR` and
`crsql - primary key for table <t> has <n> columns but the change carries <m>`.
- The check runs once, right after the package is unpacked and before `get_or_create_key`, which writes
  the pks table. Every path that uses the peer's key goes through it: the key lookup and creation, column
  writes, the insert sentinel, deletes (the `-1` sentinel with an even causal length), resurrects, and
  the value lookup in `did_cid_win`.
- **The read path needs no check.** `crsql_changes` reads build the key with
  `crsql_pack_columns(<the table's primary-key columns>)` from the pks table
  (`changes_vtab_read.rs`), so its arity always matches; `changes_next` unpacks only that local value.
  The only other key binder, `backfill.rs`, reads the local table.
- ⚠️ **It does not repair a database the defect already reached.** A NULL key recorded before the fix has
  the right arity (one NULL column) and still syncs.

**Merge Counter Bound** — wrong behaviour. A merged `col_version` of `i64::MAX` was accepted, and every
later local update of that cell then failed with `unable to reset cached trigger stmt, code CONSTRAINT`,
because a local write stores `col_version + 1` (the reviewer's `dbvmax.py`). Now a merge is refused with
`SQLITE_ERROR` and `crsql - <counter> <value> is out of range (0 to 9007199254740992)` when its
`col_version`, `db_version`, causal length (`cl`) or `seq` is negative or above 2^53
(`MAX_MERGED_COUNTER`, `core/rs/core/src/consts.rs`). How each counter is used in this fork, checked
before choosing the rule:

| counter | how it grows | negatives? | why 2^53 is safe |
|---|---|---|---|
| `col_version` | +1 per local write of the cell (`col_version + 1` in the update and move statements); a merge stores the winner's value, so it is the maximum over all writers of that cell, plus one per write | no. A resurrect zeroes non-sentinel clocks to **0**, so 0 is legal and stays accepted | the cell would need 2^53 writes |
| `db_version` | +1 per local transaction (`next_db_version`). A merged value is the **peer's own** counter: it is stored for that peer's site id (`insert_db_version`, `crsql_db_versions`) and never advances this node's counter, so it does not accumulate across peers and does not jump | no. `-1` appears only as an internal "no version yet" value in `insert_db_version`, never in a change. The first change has 1 (`MIN_POSSIBLE_DB_VERSION` is 0) | the peer would need 2^53 transactions |
| `cl` (causal length) | +1 per delete, +1 or +2 per re-insert (`mark_locally_deleted`, `mark_locally_created`); stored as the sentinel's `col_version`; the larger one wins a merge | no. A local cl of 0 means "no row"; an incoming 0 is accepted and is a no-op (it matches an absent row's cl) | the row would need about 2^52 delete/insert cycles |
| `seq` | position of a change within one transaction (a per-connection `i32` counter, `bump_seq`); a merged value is stored, never used in arithmetic | no; the local counter starts at 0 | a single transaction would need 2^53 changes; the local counter is an `i32` |

2^53 also keeps every counter exact for a JavaScript or JSON consumer of `crsql_changes`. `seq` is
included although it cannot overflow anything here: a hostile value is just as meaningless, and a
consumer that orders by `(db_version, seq)` would otherwise inherit it. Values are read with
`sqlite3_value_int64`, so a non-integer argument is judged by its integer conversion (`NULL` and a blob
are 0, `'-1'` is -1, `1e308` saturates to `i64::MAX`).
- ⚠️ **A node that already holds an out-of-range counter** — only possible if a peer sent one before the
  fix — re-emits it, and fixed peers refuse that change. The refusal is per row, but a host that applies
  a batch in one transaction loses the whole batch.

**Error Message NUL Escape** — wrong error code. The table name in a change is the peer's. When it named
no table and contained a NUL byte (`'t1' || char(0)`), building the
`crsql - could not find the schema information for table …` message with `CString::new` failed, `?`
turned the `NulError` into `SQLITE_NOMEM` (sqlite_nostd's mapping), and the merge reported memory
exhaustion — which Python, for one, raises as `MemoryError`. Now `error_cstring` (`util.rs`) writes each
NUL as the two characters `\0`, so the message keeps the rest of the name and the merge returns
`SQLITE_ERROR`.
- Other sites, searched: in the merge path, `did_cid_win` and `did_site_id_win` quote the table name too,
  and now use `error_cstring`; they run only after the name matched a real table, so they could not see a
  NUL. The new arity and counter messages use it as well. The read path's two messages
  (`changes_vtab.rs`, `changes_next`) quote a table name from the local clock tables, not a peer's, and
  are unchanged. `crsql_unpack_columns`' messages carry no input text. Messages set through
  sqlite_nostd's `StrRef::set` or `result_error` already handle a NUL without `NOMEM`.
- A column name with a NUL was never affected: no message quotes it; the lookup fails with
  `SQLITE_ERROR`.

**Tests** — `py/correctness/tests/test_merge_input_validation.py`, 55 cases, each refusal checked to
write nothing (the table, its pks and clock tables, `crsql_changes` and `crsql_db_versions` are compared
before and after):

| fix | tests | how they failed before the fix |
|---|---|---|
| **Packed PK Arity Check** | `test_wrong_pk_arity_is_rejected` (zero, fewer and more key columns for a 1- and a 2-column primary key, each as a column write, an insert sentinel, a delete and a resurrect: 20 cases); `test_wrong_pk_arity_matches_existing_row_key_too`; `test_valid_merges_still_work` (inserts, updates, deletes and resurrects synced between two databases, both key shapes) | 21 red: 13 accepted (`DID NOT RAISE`); the 8 "more" cases failed with `column index out of range`, not the arity error |
| **Merge Counter Bound** | `test_out_of_range_counter_is_rejected` (-1, 2^53+1, `i64::MAX`, `i64::MIN` for each of the four counters); `test_in_range_counter_merges` (0, 1 and 2^53 for each, then local writes); `test_col_version_at_bound_then_local_updates`; `test_dbvmax_scenario_rejected_and_local_writes_keep_working` | 17 red, all `DID NOT RAISE` |
| **Error Message NUL Escape** | `test_nul_in_table_name_is_sqlite_error_not_nomem` (three names) | 3 red: `MemoryError` |

**Verification** (stable 1.95.0, Linux x86_64):

| check | result |
|---|---|
| `make test` | green: 33 C test cases, 23 `init_race` (23 passed, 0 skipped), 11 Rust suites |
| `make valgrind-strict` | 0 errors, 0 bytes in use at exit |
| `py/correctness` | 320 passed (265 plus the 55 new) |
| `tools/init_race_stress.sh 100 8` (all three modes) | 0 failed opens, 0 wedged, 0 hangs (switch mode: 66 switcher `PRAGMA` lock failures, which are not init's) |
| `make static`, `make loadable`, `make loadable_dbg` | build; the static archive still exports `sqlite3_crsqlite_autoext_init` |
| the reviewer's `zerocols.py`, `dbvmax.py` and `malformed.py`, release and debug `.so` | every wrong-arity change refused and nothing written; both `dbvmax` merges refused and every local write after them succeeds; no crash |
| the reviewer's fuzz drivers, 200,000 cases each, release and debug (`fuzz_driver.py` seeds 41 and 42, `fuzz_driver_valid.py` seeds 43 and 44, each child under `ulimit -v 2000000`) | **0 crashes** in all four runs. Every refusal by the new checks was replayed and classified: each one had a counter outside 0..2^53 (after integer conversion) or a key of the wrong arity, so **no in-range, right-arity merge was refused**. Replaying the seed-41 cases, the pre-fix build failed 11,607 merges with `SQLITE_NOMEM`, every one with a NUL in the table name; the fixed build fails none that way |

**Mutations**, each bounded with `timeout`, restored in the same command and checked with `cmp`:

| guarantee | mutation | result |
|---|---|---|
| arity | the arity check made `false &&` | all 21 arity tests red |
| counter bound | the bound check made `false &&` | all 17 refusal tests red |
| the bound is inclusive | `>` became `>=` | the four 2^53 cases of `test_in_range_counter_merges` and `test_col_version_at_bound_then_local_updates` red |
| `seq` is covered | `seq` removed from the checked list | the four `seq` refusal cases red |
| NUL escape at the call site | the schema message built with `CString::new(…)?` again | all three NUL tests red (`MemoryError`) |

**Not verified:** macOS and Windows; a merge carrying an out-of-range counter or a wrong-arity key
through the app's sync path (the app's sync tests send only well-formed changes). The two speculative
crash paths the review left open are recorded in the coordination doc's section 14
(**Crash surface after the merge fixes**).

---

## Risks

- **A too-strict Init Completeness Check** sends healthy databases down the slow path. Every read-write
  open then takes the write lock and waits on active writers. Guarded by the read-write half of
  `init_race_reopen_writes_nothing`.
- **A too-loose heal predicate** silently re-initializes a genuine database with a new site id. That
  forks its identity, and every peer would see a new site. Guarded by the prior-db refusal tests. The
  argument for safety rests on the predicate admitting only files with no cr-sqlite state; keep rule 3
  name-based and broad (`instr(lower(name),'crsql')`), not a list of known names.
- **Blocking opens.** Through `sqlite3_crsqlite_autoext_init`, an open can spend up to 2 s **in total**
  waiting for locks while another connection holds a conflicting one during its init — on a fresh file,
  or on an initialized one mid-switch to WAL — plus the time of init's own work. ⚠️ That bound holds
  only with the per-open budget handler; with a plain `sqlite3_busy_timeout` it would be
  **2 s per wait**, 6 to 8 s per open (**The per-open budget**).
- **A clobbered host busy handler** if the new entry point were ever used outside the auto-extension
  path. It is not meant for `load_extension` use; say so in its comment and the README.
- **A held `SHARED` lock across the slow path** — a completeness-check statement not finalized, or the
  fast path's read transaction not ended — makes the connection's own `COMMIT` wait on itself until the
  budget runs out. A code-review item, and `init_race_commit_busy` exercises the path.
- **A fallible step added after the ownership handoff** brings back the zombie connection. Guarded by
  the comment at the handoff and by `init_race_late_failure_no_zombie`.
- **Hosts that relied on the noise.** A host that treated a first failed open as normal changes
  nothing; one that parses the version-0 message to detect wedged files will stop seeing it. A host
  that compared a failed open's code with `SQLITE_ERROR` now sees `SQLITE_BUSY` for a lock failure on
  the auto-extension path, and every failure now carries a `cr-sqlite init: …` message
  (**Init Failure Code and Message**).
- **Hosts that do not adopt the new entry point** keep the atomic init and the heal, but not the wait:
  their pool starts still see failed opens, cleanly. ⚠️ Nothing in `crsql_build_id()` shows which entry
  point a host registered, so a host must check its own wiring.
- **Drift.** Another local change to init, which upstream will not have. `lib.rs` init is upstream's
  hottest file for merges; keep the diff confined to `bootstrap.rs`, the guard, the ordering and error
  paths in `lib.rs`, the `config.%` loop in `ext-data.c`, and the new entry point and handler in
  `crsqlite.c`.
- **Fleet mix.** A device still running an older build can wedge a new file until it upgrades. The
  heal then rescues that file the first time an upgraded build opens it. Rollout belongs to the
  integration doc.
- **Changed signature of two exported internal symbols** (a note; no code change). `sqlite3_crsqlcore_init`
  and `sqlite3_crsqlrustbundle_init` are exported, and both gained a fourth argument, `rc_out`
  (**Init Failure Code and Message**). Only `crsqlite.c` calls them, and no external caller is known. A
  host that called either directly with three arguments would pass garbage as `rc_out`; such a host must
  call `sqlite3_crsqlite_init` instead.
- **A dependency bump can change behaviour without changing the manifest.** `85faa868` changed no
  `Cargo.toml`, yet `bytes` 1.11 changed what `get_int` returns, which broke reading packed primary keys
  (**Packed Blob Length Sign-Extension**). Guarded now by the boundary tests in the Rust `pack_columns`
  suite and in `py/correctness/tests/test_pack_columns_boundaries.py`; run both on every dependency bump.

---

## Why an integration document is needed downstream

Adopting this release in `silicon_brain` is **not** a plain pin bump, so it has its own document,
`docs/CRSQLITE_INIT_RACE_FIX_INTEGRATION.md` in that repo. In brief, the reasons are:

- the app links its own startup shim instead of `core_init.c`, so the **Init Busy Timeout** needs a
  one-line change there, landed with the pin bump — and a pin bump **without** it links cleanly and
  passes every `e4-init-atomic` marker gate while shipping without the wait;
- the app ships cr-sqlite through **three** build routes with **separate** pins: the static NIF script
  (spoke and hub), the `sqlite3mc` (SQLite3 Multiple Ciphers) spike build script, which the operator
  ruled stays at `0eb2f84e` and so never carries this fix, and a superfly prebuilt runtime `.so`
  fallback, which will **never** carry it;
- the CI marker and the hub deploy gate both key on `e2-tbl-pushdown` and would accept a build without
  the fix, and the hub does not log its `crsql_build_id()`;
- one production path — the spoke's first-boot `parking.db` — **is** exposed;
- and the app hides the symptom: its test helpers filter these log lines, its test runner re-runs
  them as flakes, and several code comments call them spurious.

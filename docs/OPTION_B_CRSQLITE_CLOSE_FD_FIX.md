# Option B — make cr-sqlite release OS file descriptors on connection close

**Audience:** an engineer/AI working in the **`infocorder_cr-sqlite`** fork (our cr-sqlite),
and possibly the **Exqlite** NIF. This doc is self-contained; it does not assume access to the
`silicon_brain` app repo, though it references it for context.

**Status:** proposed / not implemented. This is the "proper fix" for a long-standing, documented
file-descriptor leak (`silicon_brain` `docs/CR-SQLITE-DATA-SYNC.md` §14.9 / §14.9.B). An Elixir-side
workaround exists (run the per-project pool at size 1 in tests, 5 in prod), so this is not urgent —
but it's the only way to make the leak actually **zero** on all teardown paths.

---

## 1. The goal, in one sentence

Make a cr-sqlite-loaded SQLite connection **release its OS file handles** (`*.db`, `-wal`, `-shm`)
when the connection is closed **without** the host having to call `SELECT crsql_finalize()` first —
i.e. cr-sqlite should finalize its *own* internal prepared statements as part of `sqlite3_close`, so
`sqlite3_close_v2` completes instead of deferring forever.

**Success test (isolated, in this repo):** open a stock-sqlite3 connection, load cr-sqlite, use a CRR
table (so cr-sqlite prepares+caches its internal statements), then call **`sqlite3_close(db)`** (the
v1 form) **without** calling `crsql_finalize` — and get `SQLITE_OK`. Today it returns `SQLITE_BUSY`.

---

## 2. Background — the leak and why it happens

### What leaks
A connection that used cr-sqlite (any CRR table) holds internal prepared statements in its
`crsql_ExtData` (`core/src/ext-data.c`): `pDbVersionStmt`, `pSetDbVersionStmt`,
`pPragmaSchemaVersionStmt`, `pPragmaDataVersionStmt`, `pSetSyncBitStmt`, `pClearSyncBitStmt`,
`pSetSiteIdOrdinalStmt`, `pSelectSiteIdOrdinalStmt`, `pSelectClockTablesStmt`, plus a per-table
statement cache. `crsql_finalize(pExtData)` (`ext-data.c:178`) finalizes all of these and NULLs them.

These statements are **outstanding `sqlite3_stmt` objects**. SQLite will not fully close a connection
(and release its file handles) while any prepared statement is outstanding.

### The exact chain (as observed in the host app; verified)
The host (Elixir/Exqlite/Ecto) closes connections via `sqlite3_close_v2`. Relevant Exqlite code (in the
app repo at `deps/exqlite/c_src/sqlite3_nif.c`):

- `exqlite_close` (~L510) → `sqlite3_close_v2(conn->db)` (~L559). Its own comment (~L556):
  *"v1 … will return error if any unfinalized statements, which we likely have, as we rely on the
  destructors to later run to clean those up."*
- `connection_type_destructor` (~L1349) also calls `sqlite3_close_v2(conn->db)` when the connection
  **resource is garbage-collected** (this is what runs when a pooled connection process is killed).
- `statement_type_destructor` (~L1392) calls `sqlite3_finalize` on each Exqlite-tracked prepared
  statement when *its* resource is GC'd.

So when a connection dies:

1. `close_v2(db)` is called → the connection is **busy** (cr-sqlite's `pExtData` statements **and**
   any not-yet-GC'd Exqlite statements are outstanding) → SQLite marks it a **zombie** and returns OK,
   **keeping the file handles open** until the last statement finalizes.
2. Exqlite's statement resources get GC'd → their destructors finalize Exqlite's statements. Those
   blockers go away.
3. **cr-sqlite's `pExtData` statements are never finalized** (nothing calls `crsql_finalize`), so the
   zombie never completes its close → the `*.db`/`-wal`/`-shm` FDs **leak until the OS process exits.**

`crsql_finalize` is the missing step. Exqlite already cleans up its own statements; cr-sqlite does not
clean up its internal ones on close.

### Why SQLite's normal cleanup hooks don't help
- **Module `xDestroy`** (the 5th arg to `sqlite3_create_module_v2` — `crsqlite.c:69`, currently `0`) runs
  **too late**: in the bundled `core/src/sqlite/sqlite3.c`, `sqlite3LeaveMutexAndCloseZombie` returns
  early while the connection is "busy" (has outstanding statements) — *before* it destroys modules or
  disconnects vtabs. cr-sqlite's cached statements are exactly what make it busy → chicken-and-egg: the
  destructor that would finalize them only runs after they're already gone.
- **vtab `xDisconnect`** (`changesDisconnect`, `changes-vtab.c:63`) explicitly does **not** free the
  ext-data (comment: *"ext data is freed by other registered extensions"*), and also runs in that late
  phase.
- The fork already has the right idea — `closeHook` → `crsql_finalize(pExtData)` (`crsqlite.c:33-36`),
  registered via `libsql_close_hook` (`crsqlite.c:74-76`) — but **only under `#ifdef LIBSQL`**.
  `libsql_close_hook` is a **libSQL-only** API; stock sqlite3 (what we build) has no close hook, so you
  **cannot** just remove the `#ifdef` (it won't link).

---

## 3. The fix

The core is: **finalize cr-sqlite's own `pExtData` statements at the start of a close, before SQLite's
"busy" check.** Two viable implementations; **A is recommended** (contained to our cr-sqlite fork, and
only ever touches cr-sqlite's *own* statements — safe).

### Approach A (recommended) — a pre-close hook in the bundled sqlite3 amalgamation
Mirror what libSQL did, but for stock sqlite3:

1. In `core/src/sqlite/sqlite3.c` (the bundled amalgamation), add a minimal **pre-close hook** mechanism:
   a registrable callback that `sqlite3_close`/`sqlite3_close_v2` invokes **at the very top**, before the
   "connection is busy?" check. (libSQL's `libsql_close_hook` is the reference shape. Keep it tiny: a
   single function pointer + user-data slot on the `sqlite3` struct, invoked once at the start of
   `sqlite3Close`.)
2. In `core/src/crsqlite.c`, register that hook (unconditionally, i.e. the stock path — not just
   `#ifdef LIBSQL`) to call `crsql_finalize(pExtData)` — reuse the existing `closeHook` body.
3. Rebuild.

Why this is safe: the hook finalizes **only cr-sqlite's own** statements (`crsql_finalize` knows exactly
which pointers are its). Exqlite's statements are finalized by Exqlite's own destructors (step 2 above).
Once both sets are gone, `close_v2` completes and the FDs release.

**Cost / risk:** you are now maintaining a small patch on top of the vendored SQLite amalgamation, which
must be re-applied whenever the amalgamation is upgraded. Keep the patch as small and well-commented as
possible (ideally a single, clearly-delimited block).

### Approach B (alternative) — finalize-all in Exqlite's close
In `deps/exqlite/c_src/sqlite3_nif.c`, before `sqlite3_close_v2` in `exqlite_close` (and in
`connection_type_destructor`), loop `sqlite3_next_stmt(conn->db, NULL)` + `sqlite3_finalize` to finalize
**every** outstanding statement, then close.

- **Pro:** one place; no amalgamation patch; handles cr-sqlite's *and* Exqlite's statements at once.
- **Con (important):** `sqlite3_next_stmt` returns statements owned by **Exqlite's own `statement_t`
  resources** too. Finalizing those out from under Exqlite leaves those resources with **dangling
  `sqlite3_stmt*`** → use-after-free / double-finalize when `statement_type_destructor` later runs. To do
  this safely you'd have to make Exqlite **track its prepared statements on the connection** and NULL
  them here (a larger Exqlite change), or otherwise guarantee no live `statement_t` still references a
  finalized handle. Because of this, prefer Approach A.

### Idempotency (both approaches)
`crsql_finalize` is already **idempotent**: it finalizes each statement and sets the pointer to `0`, and
`sqlite3_finalize(NULL)` is a harmless no-op. So it is safe to run via the close hook **and** via the
host's existing `before_disconnect: crsql_finalize` path. Verify this stays true.

---

## 4. How to test — inside this cr-sqlite repo (isolated, no app needed)

The fork already has a C test harness: `core/src/tests.c` (a `main` that runs named suites), and a
helper `crsql_close(db)` that does `SELECT crsql_finalize()` then `sqlite3_close(db)` and, on failure,
uses `sqlite3_next_stmt(db, NULL)` + `sqlite3_expanded_sql` to print the first outstanding statement.
See also `core/src/ext-data.test.c`, `core/src/crsqlite.test.c`, and the `Makefile` / `tools/` for how
tests are built and run.

**Add a focused test** (e.g. in `crsqlite.test.c` or a new suite) that proves close releases without an
explicit finalize:

```c
// PSEUDOCODE — adapt to the repo's test conventions / assert macros.
static void test_close_finalizes_crsql_stmts(void) {
  sqlite3 *db;
  assert(sqlite3_open(":memory:", &db) == SQLITE_OK);          // or a temp file so you can watch FDs
  // load the extension the way the suite does (statically linked in tests, or sqlite3_load_extension)
  assert(sqlite3_exec(db, "CREATE TABLE foo (id PRIMARY KEY NOT NULL, a);", 0,0,0) == SQLITE_OK);
  assert(sqlite3_exec(db, "SELECT crsql_as_crr('foo');", 0,0,0) == SQLITE_OK);
  // Exercise cr-sqlite so it prepares + caches its internal statements:
  assert(sqlite3_exec(db, "INSERT INTO foo VALUES (1, 'x');", 0,0,0) == SQLITE_OK);
  assert(sqlite3_exec(db, "SELECT count(*) FROM crsql_changes;", 0,0,0) == SQLITE_OK);
  assert(sqlite3_exec(db, "SELECT crsql_db_version();", 0,0,0) == SQLITE_OK);

  // THE ASSERTION: v1 close, WITHOUT calling crsql_finalize first.
  // v1 returns SQLITE_BUSY if ANY statement is outstanding; SQLITE_OK only if all finalized.
  int rc = sqlite3_close(db);   // NOT sqlite3_close_v2 — v1 is the strict proof
  if (rc != SQLITE_OK) {
    sqlite3_stmt *s = sqlite3_next_stmt(db, NULL);
    printf("close returned %d; first unfinalized: %s\n", rc, s ? sqlite3_expanded_sql(s) : "(none)");
  }
  assert(rc == SQLITE_OK);      // FAILS today (SQLITE_BUSY); PASSES with the fix.
}
```

- **Before the fix:** `sqlite3_close` (v1) returns `SQLITE_BUSY` because cr-sqlite's cached statements are
  outstanding. (You can confirm the diagnosis with the `sqlite3_next_stmt` print.)
- **After the fix:** the close hook runs `crsql_finalize` first, so no cr-sqlite statements remain, and
  `sqlite3_close` returns `SQLITE_OK`.

This is the clean, self-contained proof of the cr-sqlite half of the fix. Wire it into the existing test
runner and run whatever the repo's `make test` (or equivalent) is.

Optionally, add an FD-count variant on a real temp file (not `:memory:`) that opens, uses cr-sqlite, then
closes and checks the file's handles are gone (Linux: inspect `/proc/self/fd`), to prove the OS handles
actually release — but the `SQLITE_OK`-from-v1-close assertion above is the key signal.

---

## 5. End-to-end validation — in the `silicon_brain` app (after rebuilding the native lib)

Once the cr-sqlite test passes, rebuild the native library and validate against the app's existing FD
tests (these already exist and encode the expected numbers):

1. Rebuild cr-sqlite (the app has `build_crsqlite_static.sh`) and drop the artifact into the app's
   `priv/native/`.
2. Run, at the **prod** pool size (5), the app's FD-bound test — it currently asserts the leak is
   *bounded* (`~2×pool_size`); with the fix it should drop toward **~0**:
   - `mix test test/silicon_brain/spoke_project_manager_large_pool_test.exs`
     (the "terminate_repo FD leak is BOUNDED (~pool_size)…" test) — tighten its bound to ~0 once green.
   - `mix test test/silicon_brain/spoke_project_manager_fd_release_test.exs` (managed-pool + create!).
   Expected with the fix: `after_terminate ≈ base` (FDs return to baseline), not `after_terminate ≈
   after_create`.
3. If solid, the app can (a) restore `per_project_pool_size` to a single value / raise it freely, and
   (b) drop the "bounded leak" framing in `docs/CR-SQLITE-DATA-SYNC.md` §14.9.

---

## 6. Caveats & known limits

- **Windows `-shm` mmap.** Per the app's §14.9 notes, on native Windows the WAL `-shm` **memory-mapped**
  handle is released only at process exit regardless of finalize. So even a perfect Option B fixes
  Linux/macOS fully but may leave a residual `-shm` handle on Windows. Confirm/measure; document.
- **`close_v2` vs `close_v1`.** The app (Exqlite) uses `close_v2` (deferred). The fix makes the *deferred*
  close **complete** (once cr-sqlite's statements are finalized by the hook and Exqlite's by GC). The C
  test uses `close_v1` deliberately, as the strict "all statements finalized?" oracle.
- **Only finalize cr-sqlite's own statements** in Approach A (don't touch statements you don't own — see
  Approach B's caveat).
- **Amalgamation upgrade hygiene** (Approach A): keep the pre-close-hook patch minimal and clearly
  delimited so it's easy to re-apply on SQLite upgrades. Consider a small script/patch file under
  `tools/`.
- **Thread-safety.** The close path holds SQLite's connection mutex; ensure the hook (and `crsql_finalize`)
  is safe to run there (it only finalizes statements on the same connection — should be fine, but verify
  against the amalgamation's mutex expectations).

---

## 7. Definition of done

- [ ] A C test in this repo: open + use cr-sqlite + `sqlite3_close(db)` (v1) **without** `crsql_finalize`
      → `SQLITE_OK` (was `SQLITE_BUSY`). Wired into the test runner; passes.
- [ ] The fix touches **only cr-sqlite's own** statements (Approach A) — no dangling `sqlite3_stmt*` for
      any host-owned statements.
- [ ] `crsql_finalize` remains idempotent (safe to call via the hook *and* the host's `before_disconnect`).
- [ ] Rebuilt native lib: the app's `spoke_project_manager_large_pool_test` / `…_fd_release_test` show
      `after_terminate ≈ base` at pool_size 5 (FDs return to baseline).
- [ ] Windows `-shm` residual measured + documented (expected to persist until process exit).

---

## 8. Key references

**cr-sqlite fork (this repo, `core/src/`):**
- `crsqlite.c:33-36` — `closeHook` → `crsql_finalize(pExtData)` (currently `#ifdef LIBSQL`).
- `crsqlite.c:69` — `sqlite3_create_module_v2("crsql_changes", …, pExtData, 0)` — the `0` is the (too-late)
  module destructor slot.
- `crsqlite.c:74-76` — `libsql_close_hook(db, closeHook, pExtData)` registration (libSQL-only).
- `ext-data.c:178` — `crsql_finalize(crsql_ExtData*)` — finalizes + NULLs the cached statements (idempotent).
- `changes-vtab.c:63` — `changesDisconnect` (does NOT free ext-data).
- `sqlite/sqlite3.c` — bundled amalgamation; `sqlite3LeaveMutexAndCloseZombie` returns early while busy
  (why module/vtab destructors are too late). This is where Approach A's pre-close hook goes.
- `tests.c` — test `main` + `crsql_close` helper (uses `sqlite3_next_stmt` to inspect outstanding stmts);
  `ext-data.test.c`, `crsqlite.test.c` — suite patterns; `Makefile` / `tools/` — build+run.

**Exqlite (in the app repo at `deps/exqlite/c_src/sqlite3_nif.c`), for context / Approach B:**
- `exqlite_close` (~L510), `sqlite3_close_v2` (~L559), the "we rely on destructors later" comment (~L556).
- `connection_type_destructor` (~L1349) → `sqlite3_close_v2` on connection-resource GC.
- `statement_type_destructor` (~L1392) → finalizes Exqlite's own statements on statement-resource GC.
- `connection_t` (~L55) / `statement_t` (~L71) structs.

**Host app context:**
- `silicon_brain` `docs/CR-SQLITE-DATA-SYNC.md` §14.9 (the leak) and §14.9.B (this option).
- `lib/silicon_brain/spoke_project_manager.ex` — `terminate_repo/1` (graceful `Supervisor.stop`, no
  finalize fires) and `do_start_repo` (per-project pool).

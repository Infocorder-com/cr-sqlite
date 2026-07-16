# Option B — make cr-sqlite release OS file descriptors on connection close

**Audience:** an engineer/AI working in the **`infocorder_cr-sqlite`** fork (our cr-sqlite),
and possibly the **Exqlite** NIF. This doc is self-contained; it does not assume access to the
`silicon_brain` app repo, though it references it for context.

**Status:** **E1 + `crsql_build_id()` implemented & Tier-1-verified in this repo** (red→green in
`make test` + `make valgrind` clean — see §11.4); the chosen approach is **E1**, not §3's Approach A
(see §10–§11 for why). Only Tier-2 app-side validation (pool_size-5 FD test) remains.
This is the "proper fix" for a long-standing, documented file-descriptor leak (`silicon_brain`
`docs/CR-SQLITE-DATA-SYNC.md` §14.9 / §14.9.B). An Elixir-side workaround exists (run the per-project
pool at size 1 in tests, 5 in prod), so this is not urgent — but it's the only way to make the leak
actually **zero** on all teardown paths.

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

---

## 9. Expanded solution analysis (added after code-tracing this repo)

This section records a deeper brainstorm done directly against the checked-in sources
(`core/src/sqlite/sqlite3.c`, `core/src/crsqlite.c`, `core/src/changes-vtab.c`, `core/Makefile`).
It **adds two options the original doc didn't list**, surfaces a cross-cutting question that changes
the ranking, and gives an analytical evaluation. Treat §3's "A recommended" as **superseded by §9.6
below** pending the two verifications in §9.7.

### 9.1 Facts verified in this repo

- **Exact close path.** `sqlite3Close` (`sqlite3.c:176109`) does, in order: `sqlite3_mutex_enter`
  (176118) → **`SQLITE_TRACE_CLOSE` dispatch (176119–176121)** → `disconnectAllVtab(db)` (176124) →
  `sqlite3VtabRollback` (176133) → the fatal **`if(!forceZombie && connectionIsBusy(db))` gate (176138)**
  → returns `SQLITE_BUSY`. `connectionIsBusy` inspects `db->pVdbe`; `sqlite3_finalize` removes a
  statement from exactly that list. So **anything that finalizes cr-sqlite's statements before line
  176138 makes the busy check pass** — and both the trace dispatch and `disconnectAllVtab` sit before it.
- **`disconnectAllVtab` runs before the busy check** and calls `xDisconnect` on the *eponymous*
  `crsql_changes` module via `pMod->pEpoTab`. `changesDisconnect` (`changes-vtab.c:63`) already holds
  `p->pExtData` (set at `changes-vtab.c:44`) but deliberately does not finalize it.
- **Trace is compiled in** for our build (`SQLITE_OMIT_TRACE` only triggers under
  `SQLITE_OMIT_FLOATING_POINT`, which we never set); the 176119 dispatch is unconditional.
- **`sqlite3_trace_v2` is in the extension API routines struct** (`sqlite3ext.h:283`, remapped at
  line 620), next to the `commit_hook`/`rollback_hook` cr-sqlite already uses — so it is callable from
  **both** the loadable-extension and static builds.
- **Test harness makes the mechanism testable here.** `make test` compiles
  `dist/sqlite3-extra.c` (= vendored `sqlite3.c` + `core_init.c`) with `-DSQLITE_EXTRA_INIT=core_init`;
  `core_init` calls `sqlite3_auto_extension(sqlite3_crsqlite_init)`, so **every `sqlite3_open` in a test
  auto-loads cr-sqlite**. `sqlite3.c` is vendored/checked-in (no regeneration on normal builds).

### 9.2 The full candidate set

1. **A1** — pre-close hook via new `struct sqlite3` fields + registrar (libSQL-style amalgamation patch;
   this is §3's Approach A).
2. **A2** — pre-close hook as a single call into `crsqlite.c` + a global `db→pExtData` registry
   (smaller, better-delimited patch; adds a process-global map + mutex).
3. **B1** — `changesDisconnect` calls `crsql_finalize(p->pExtData)` (no patch; rides the vtab-disconnect
   that already runs before the busy check).
4. **E1 (new)** — register `sqlite3_trace_v2(db, SQLITE_TRACE_CLOSE, cb, pExtData)` in `crsqlite_init`;
   the callback calls `crsql_finalize(pExtData)`. **Zero amalgamation patch, standard public API.**
5. **C1** — stop caching `pExtData` statements / finalize eagerly (root-cause source fix).
6. **D** — host-side: make Exqlite run `crsql_finalize` on *every* teardown path. This is really the
   existing Elixir-side "Option A" workaround extended; out of scope for "make cr-sqlite self-clean."

### 9.3 The cross-cutting question that reshapes the ranking

Approaches **A1/A2 patch cr-sqlite's vendored `sqlite3.c`**. That only helps **if the `sqlite3_close`
executing at runtime is the one from that patched amalgamation.** In the silicon_brain integration,
Exqlite's NIF normally carries **its own** `sqlite3.c`. If cr-sqlite is loaded into Exqlite's SQLite
(loadable ext, or a static link where Exqlite's amalgamation wins the `sqlite3_close` symbol), then the
patched close **never runs and Approach A does nothing end-to-end** — even though the isolated C test in
this repo (which compiles cr-sqlite's own amalgamation) still passes. **That is a false-green trap.**

**B1 and E1 are immune to this**: they rely only on cr-sqlite's `init` running (it must, to provide the
CRR functions) plus a standard mechanism (`SQLITE_TRACE_CLOSE` / `xDisconnect`) present in *whatever*
SQLite actually runs the close. This is the single most important axis and it flips the original "A first"
recommendation.

### 9.4 Evaluation matrix

| Axis | A1 struct-hook | A2 global-registry | B1 changesDisconnect | **E1 trace_v2** | C1 no-cache |
|---|---|---|---|---|---|
| Fires on **every** close path | yes | yes | **only if `crsql_changes` was queried** | yes | yes |
| Works regardless of whose amalgamation runs | **no (patched-build only)** | **no (patched-build only)** | yes | yes | yes |
| Amalgamation patch / upgrade burden | 3 sites incl. struct layout | 1 site + global | none | **none** | none |
| Touches only cr-sqlite's own stmts | yes | yes | yes | yes | n/a |
| Host-callback conflict | none | none | none | **owns trace slot** | none |
| Perf | none | none | none | none (only CLOSE bit set) | **re-prepares hot stmts** |
| Testable in this repo's `make test` | yes | yes | yes | yes | yes |

### 9.5 Per-option reasoning

- **A1 (struct-field hook).** Semantically cleanest, mirrors the existing `#ifdef LIBSQL closeHook`. But a
  three-site patch including a `struct sqlite3` layout change is the worst upgrade hygiene, and — decisively
  — it is **patched-build-only** (see §9.3). Viable only if we control the amalgamation that runs the close.
- **A2 (call + global registry).** Smaller, better-delimited patch than A1; costs a process-global
  `db→pExtData` map + mutex and a lookup per close. Same fatal dependency on running the patched
  amalgamation. No advantage over E1 unless we specifically must avoid the trace slot *and* control the
  amalgamation.
- **B1 (changesDisconnect).** Tiny, no patch, `p->pExtData` already in hand. Killer weakness: **coverage
  gap** — `pMod->pEpoTab` exists only if `crsql_changes` was *queried* on that connection, but the
  `pExtData` statements are prepared by ordinary CRR writes / the commit hook with no `crsql_changes`
  query. A write-only pooled connection would still leak. Unreliable as the *sole* fix, but free and
  self-owned → a reasonable **defense-in-depth complement** to E1.
- **E1 (trace_v2 / `SQLITE_TRACE_CLOSE`).** Fires unconditionally at the top of every
  `sqlite3_close`/`close_v2`, before the busy check, in whatever SQLite runs — so it is robust across
  teardown paths **and** across the integration model, with **zero amalgamation patch**. Finalize runs
  under `db->mutex` (same context Approach A's hook would use; `sqlite3_finalize` re-enters the recursive
  mutex fine), and setting **only** the `CLOSE` mask means no per-statement trace overhead. Two honest
  caveats: (1) cr-sqlite would own the single trace slot — if the host later calls `sqlite3_trace_v2`
  (**Exqlite exposes an optional trace feature**), it silently clobbers the close hook and the leak
  returns. cr-sqlite already makes this same "we own the per-connection callback" assumption for
  commit/rollback hooks (`crsqlite.c:77` TODO), but trace is more likely to be contended. (2) Using an
  observability callback to *do work* (finalize) is slightly off-label; the C test plus `asan`/`valgrind`
  should confirm no debug-build assertion fires when finalizing inside the trace-close callback.
- **C1 (no-cache).** The only option that removes the root cause, but it re-prepares hot statements
  (`db_version` on the commit path) on every use — a measurable regression for the exact statements
  cr-sqlite caches deliberately. Largest, riskiest change. Not recommended.
- **D (host-side).** The cleanest host-only fix is making Exqlite run `crsql_finalize` on the connection
  destructor path so it fires on *all* teardowns (incl. GC), touching only cr-sqlite's statements via the
  SQL function. But it lives in the app/Exqlite repo and couples Exqlite to cr-sqlite — it is the existing
  Option-A workaround, not this "make cr-sqlite self-clean" task.

### 9.6 Updated recommendation

The original "A1 first" ranking was wrong on the axis that matters most (§9.3): it silently assumes
cr-sqlite's amalgamation runs the close, which is likely false in the Exqlite integration. Revised order:

1. **Primary: E1 (`SQLITE_TRACE_CLOSE`)** — provided Exqlite's trace feature is not (and won't be) enabled
   on these connections. Only option that is simultaneously zero-patch, fires on every teardown, and works
   regardless of whose SQLite runs the close. Register it beside the existing hooks in `crsqlite_init`,
   reusing the `closeHook` body.
2. **Optional complement: B1** — a two-line safety net for connections that *did* query `crsql_changes`;
   free and independent of E1.
3. **Fallback: A1** — only if (a) cr-sqlite's amalgamation genuinely provides the runtime `sqlite3_close`
   **and** (b) the trace-slot ownership is judged unacceptable. Then the libSQL-style patch is the
   "blessed" shape, at the cost of amalgamation maintenance.
4. **Drop C1**; treat **D** as the existing Option-A workaround, not this task.

### 9.7 Two facts to verify before writing code

1. **Does the app build make cr-sqlite's amalgamation the runtime `sqlite3_close`,** or does Exqlite's
   own `sqlite3.c` win that symbol? (Inspect `build_crsqlite_static.sh` and how the artifact is linked
   into the Exqlite NIF.) This decides whether A1/A2 are even viable end-to-end.
2. **Is Exqlite's trace feature ever enabled on these pools?** This decides whether E1's one weakness
   (trace-slot ownership) can bite.

### 9.8 Test plan implication (important)

The isolated C test **cannot distinguish A from E/B on the integration axis** — it compiles cr-sqlite's
own amalgamation, so A1/A2 pass there even if they are a no-op in production. So testing needs two tiers:

- **Tier 1 (this repo — oracle for the *mechanism*):** the v1-`sqlite3_close` → `SQLITE_OK` test in
  `crsqlite.test.c` (per §4), run under `make test` + `make asan` + `make valgrind`. Red on unpatched,
  green after. This proves the finalize-on-close mechanism works; it does **not** prove the chosen
  registration path is actually wired into the SQLite that runs in prod.
- **Tier 2 (app repo — must-do for A, nice-to-have for E/B):** the FD test at pool size 5
  (§5). Only this exercises the *real* `sqlite3_close` and would expose an "A-is-a-no-op" false green.

---

## 10. §9.7 resolved — verified against the `silicon_brain` build (decision is now settled)

§9 left two facts open (§9.7). Both have now been **verified directly against the app's build
scripts and the vendored Exqlite NIF sources**, and they settle the ranking. Net result: **E1 is
the fix, B1 is the complement, and A1/A2 are provably no-ops in our integration — do not spend
effort on them.**

### 10.1 §9.7 Q1 — whose `sqlite3_close` runs at runtime? → **Exqlite's, always.** (A1/A2 are dead on arrival.)

Our NIF is assembled by `scripts/build_crsqlite_static.sh`. The decisive lines:

- **`sqlite3.c` compiled is Exqlite's own** amalgamation, not cr-sqlite's:
  ```
  # build_crsqlite_static.sh
  echo "==> Compiling sqlite3.c with -DSQLITE_EXTRA_INIT=core_init..."
  "$CC" … -DSQLITE_EXTRA_INIT=core_init -I"$EXQLITE_SRC_DIR" \
      -o "$TMP_BUILD/sqlite3.o" "$EXQLITE_SRC_DIR/sqlite3.c"   # deps/exqlite/c_src/sqlite3.c
  ```
- **cr-sqlite is linked in as a Rust *extension* archive only** (`crsqlite.a`, built
  `--features static,omit_load_extension`), whole-archived alongside Exqlite's objects:
  ```
  "$CC" -o "$OUT_SO" \
      "$TMP_BUILD/sqlite3_nif.o" \
      "$TMP_BUILD/sqlite3.o" \            # ← Exqlite's SQLite core (owns sqlite3_close)
      "$TMP_BUILD/exqlite_crsqlite_init.o" \
      -Wl,--whole-archive "$CRSQLITE_A" -Wl,--no-whole-archive \   # ← cr-sqlite ext only
      …
  ```
- **cr-sqlite's *own* bundled `core/src/sqlite/sqlite3.c` is never compiled into our NIF at all.**
  cr-sqlite auto-registers via the standard hook: the overlay `monkeypatched_deps/exqlite_crsqlite_init.c`
  defines `core_init` → `sqlite3_auto_extension(sqlite3_crsqlite_init)`, invoked from Exqlite's
  `sqlite3_initialize()` because Exqlite's `sqlite3.c` was compiled with `-DSQLITE_EXTRA_INIT=core_init`.

**Consequence:** the `sqlite3_close` / `sqlite3_close_v2` that executes in production is **Exqlite's
amalgamation, every connection, every teardown.** §9.3's "false-green trap" is therefore **certain for
our build**, not hypothetical:

- **A1/A2 (patch cr-sqlite's `core/src/sqlite/sqlite3.c`)** would patch code we do not link. The Tier-1
  C test in this repo would go green (it compiles cr-sqlite's amalgamation), while our shipped NIF is
  byte-for-byte unaffected. **A1/A2 cannot fix the app leak.**
- **E1 and B1 depend only on `sqlite3_crsqlite_init` running** — which it must, because that's what
  provides the CRR functions — plus a standard mechanism (`SQLITE_TRACE_CLOSE` / vtab `xDisconnect`)
  present in Exqlite's SQLite. Both are immune to the linking question. ✅

### 10.2 §9.7 Q2 — is Exqlite's trace slot in use? → **No. The `SQLITE_TRACE_CLOSE` slot is free for E1.**

Exqlite's NIF (`deps/exqlite/c_src/sqlite3_nif.c`) **never calls `sqlite3_trace_v2`** — the only
occurrences of `trace_v2` in the tree are the API-struct declarations in `sqlite3ext.h` / `sqlite3.h`.
So E1 can own the single per-connection trace callback with **zero contention today**.

One caveat to carry forward (it belongs in E1's own comment): `sqlite3_trace_v2` has exactly **one
callback slot per connection**. If the app ever enables Exqlite query tracing / `EXPLAIN` telemetry on
these pools, it would clobber E1's close hook and the leak would silently return. That single-owner risk
is precisely why **B1 is worth shipping alongside E1** — B1 rides vtab `xDisconnect`, independent of the
trace slot, so it survives a future trace user (for the subset of connections that queried
`crsql_changes`).

### 10.3 Final decision (supersedes §9.6's "pending verification")

With both §9.7 facts confirmed, §9.6's conditional ranking becomes unconditional:

1. **E1 (`sqlite3_trace_v2` + `SQLITE_TRACE_CLOSE`) — the fix.** Register in `sqlite3_crsqlite_init`
   beside the existing commit/rollback hooks; the callback calls `crsql_finalize(pExtData)` (reuse the
   `closeHook` body). Set **only** the `SQLITE_TRACE_CLOSE` mask (no per-statement trace overhead).
2. **B1 (`changesDisconnect` → `crsql_finalize(p->pExtData)`) — ship as a two-line complement**, for
   defense-in-depth against a future trace-slot owner. Accept its coverage gap (only connections that
   queried `crsql_changes`); E1 covers the rest.
3. **A1/A2 — do not implement.** Proven no-ops in this integration (§10.1).
4. **C1 dropped; D is the existing Elixir-side workaround, not this task.**

Idempotency (§3.Idempotency) matters more now: `crsql_finalize` may run via **E1's trace callback,
B1's xDisconnect, *and* the host's existing `before_disconnect: crsql_finalize`** — up to three times per
connection. It is already idempotent (finalize + NULL, `sqlite3_finalize(NULL)` is a no-op); keep it so,
and add a test that calls it twice in a row.

### 10.4 Test plan, concretized for this integration

Both tiers are required here (not "nice-to-have"), because Tier 1 alone cannot see the linking axis:

- **Tier 1 — mechanism oracle (this repo):** the §4 test — use cr-sqlite, then `sqlite3_close(db)` (v1,
  no explicit `crsql_finalize`) → `SQLITE_OK` (was `SQLITE_BUSY`). Run under `make test` + `make asan` +
  `make valgrind` (asan/valgrind specifically to prove finalizing inside a trace-close callback trips no
  debug assertion). **Add a second variant** that exercises E1's path with a **write-only** connection
  (CRR writes but *no* `crsql_changes` query) to prove E1 fires where B1 would not.
- **Tier 2 — end-to-end truth (app repo):** rebuild the NIF (§10.5) and run at **pool_size 5**:
  - `test/silicon_brain/spoke_project_manager_large_pool_test.exs` — the "terminate_repo FD leak is
    BOUNDED" test. **This is the acceptance gate.** Today it asserts `after_terminate - base <=
    2*pool_size + 4`; with E1 wired in it should collapse to `after_terminate ≈ base`. If it does **not**
    move, E1 did not reach the real `sqlite3_close` — the exact false-green Tier 1 can't detect.
  - `test/silicon_brain/spoke_project_manager_fd_release_test.exs` — managed-pool + create! paths.
  - Only after both are green at pool_size 5: tighten those bounds toward ~0 and drop the "bounded leak"
    framing in `docs/CR-SQLITE-DATA-SYNC.md` §14.9 / §14.9.B.

**Please also add a build-provenance marker — `crsql_build_id()`.** The app cannot otherwise tell our
E1-carrying static NIF apart from the upstream downloaded `priv/native/crsqlite.dll` fallback: **both
answer `crsql_db_version()` identically**, so "cr-sqlite works" is not evidence that E1 shipped. A trivial
scalar SQL function returning a compiled-in string (e.g. the fork commit + `"E1"`) makes the live build
identifiable at runtime on every OS — indispensable on Windows, where the packaged Burrito `.exe` can't be
exercised by `mix test` and the two artifacts ship side-by-side. Register it beside the CRR functions in
`sqlite3_crsqlite_init` so it rides both the static auto-extension path and the loadable-extension path.

The app side is **already wired to consume it** (forward-compatible — lights up automatically once the
function exists), which is how the fix is verified in the field without a Windows harness:

- **Boot provenance log** — `SiliconBrain.Application.log_crsqlite_static_linkage_status/0` opens a raw
  `:memory:` connection (no `:load_extensions`) and now logs
  `[boot] cr-sqlite linkage active (static … | runtime load_extension): crsql_db_version()=<v>
  build_id=<crsql_build_id() | absent> fallback_ext=<present|absent> crsqlite_path=<resolved>`.
  On Windows this single line, read from the field `infocorder.log.win.*`, answers: did the NIF load,
  is it the static build, **which build**, and is the wrong (fallback) path in play (§10.5).
- **Teardown handle-release log** — `SiliconBrain.SpokeProjectManager.terminate_repo/1` logs, per project
  stop, the BEAM's open handle count on that project's `infocorder.db`/`-wal` files before vs after the
  pool teardown (Linux `/proc`; read-only, never mutates the DB). Pre-E1 → `released≈0` (the known leak);
  post-E1 → `after≈0`. This is the Layer-D behavior signal *as a field log* on Linux. On Windows (no
  `/proc`, and `-shm` persists until process exit regardless — §6) E1's close behavior is proven by the
  Layer-D **file-lock test** below, not this field probe.

Windows E1 acceptance test (Layer D, since `/proc` is unavailable): after `terminate_repo`, on a throwaway
`/tmp`-equivalent DB, attempt to delete/rename the closed `.db` and `-wal` — Windows refuses to remove a
file with an open handle, so a successful delete proves the zombie handle released (E1 worked); assert on
`.db`/`-wal` only, never `-shm`.

### 10.5 What changes in build / integration / deployment — across all OSes

Modifying our cr-sqlite means bumping the pinned ref and **rebuilding the static archive on every
target** (the change is source-in-`crsqlite.a`, so no app-side Elixir change is required to consume it):

- **Pin bump.** Land E1+B1 in `Infocorder-com/cr-sqlite`, then bump `CRSQLITE_REF` in
  `scripts/build_crsqlite_static.sh` (currently `4d9aa60487a44780e56166970dd1ba4fe7140dec`). The ref (and
  the overlay) feed the build **stamp**, so bumping it forces a clean rebuild everywhere and won't serve a
  stale cached NIF — but *verify* the stamp actually rebuilt; a stale artifact is itself a false green.
- **Per-OS build** (already wired in `.github/workflows/build.yml` as
  `build_crsqlite_static.sh --env prod --target <rust-triple>` per matrix entry):
  - **Linux x86_64 / aarch64 — LOW risk** (gcc, ELF `.so`, `--whole-archive`). Build & validate here first.
  - **macOS x86_64 / aarch64 — MEDIUM** (clang/xcrun, Mach-O, `-force_load`; cross-arch caveats).
  - **Windows x86_64 — HIGHEST.** The MinGW + bindgen path (`build.yml` ~L848) is the fragile one — clang 22
    + bindgen opaque-struct shim. E1/B1 add **no new struct/API surface** (a trace registration + a vtab
    call), so they should not re-trigger the opaque-struct bindgen failure — confirm during the Windows
    build. Also: `build.yml` still has a **prebuilt-`crsqlite.dll` download fallback** (~L813,
    `download_crsqlite.sh`) pointing at an upstream `superfly/cr-sqlite` release that will **not** contain
    E1 — ensure prod loads the **statically-linked** NIF, not that fallback, or the fix silently won't ship
    on Windows.
  - **`-shm` residual (Windows):** unchanged by this fix — the WAL `-shm` mmap releases only at process
    exit regardless of finalize (§6). E1 fixes the `.db`/`-wal` FDs on all platforms; document the Windows
    `-shm` ceiling.
- **Deployment.** Burrito desktop builds package whatever `sqlite3_nif.<ext>` sits in
  `_build/prod/lib/exqlite/priv/` — so a rebuilt artifact per OS is the *only* deploy change; no packaging
  or config edits. The build script also mirrors to `_build_hub/…` for hub-mode peers, so the hub picks up
  the same fixed NIF automatically.
- **Rollout order (lowest risk):** (1) E1+B1 in the fork → (2) Tier-1 `make test`/asan/valgrind green →
  (3) bump `CRSQLITE_REF`, build **Linux** locally, run Tier-2 FD tests and confirm the bound collapses to
  ~0 → (4) only then fan out to macOS + Windows CI. This confirms the mechanism end-to-end on the *real*
  Exqlite close before spending the Windows build's risk budget.

### 10.6 TL;DR for the cr-sqlite-repo implementer

Implement **E1**: in `sqlite3_crsqlite_init` (`core/src/crsqlite.c`), after `pExtData` is created,
`sqlite3_trace_v2(db, SQLITE_TRACE_CLOSE, <cb>, pExtData)` where `<cb>` calls
`crsql_finalize(pExtData)` and returns 0 — reusing the existing `#ifdef LIBSQL` `closeHook` body.
Add **B1**: one line in `changesDisconnect` (`core/src/changes-vtab.c`) — `crsql_finalize(p->pExtData)`.
**Skip A1/A2 entirely** — in the silicon_brain build, Exqlite's `sqlite3.c` (not cr-sqlite's) runs every
close, so amalgamation patches never execute in production (§10.1). Prove it with the §10.4 two-tier
tests; the app's pool_size-5 FD test is the real acceptance gate.

---

## 11. Implementation plan & status (re-analysis of §10, then build)

This section is the actual work log for landing the fix in this repo. It starts from an
**independent re-verification** of §10's claims against the sources, records **one deliberate divergence
from §10 (B1 is deferred, not shipped alongside E1)**, and tracks implementation status.

### 11.1 Re-verification of §10 (done — all confirmed, plus one new hazard)

Re-checked directly against the checked-in code:

- **E1 mechanism — confirmed.** `sqlite3Close` dispatches `SQLITE_TRACE_CLOSE` at `sqlite3.c:176119`,
  *before* `disconnectAllVtab` (176124) and the `connectionIsBusy` gate (176138). `connectionIsBusy`
  reads `db->pVdbe`; `sqlite3_finalize` unlinks from exactly that list. Trace is compiled in;
  `sqlite3_trace_v2` + `SQLITE_TRACE_CLOSE` are reachable from `crsqlite.c` (via `sqlite3ext.h`), and
  `crsql_finalize` is declared in `ext-data.h` (already included). ✅
- **Q2 (trace slot free) — consistent with §10.2.** cr-sqlite registers the CLOSE-only mask; setting a
  non-CLOSE-free `mTrace` adds no per-statement cost because the STMT/PROFILE/ROW bits stay clear. ✅
- **NEW hazard found for B1 (this is the divergence).** The `pExtData` statements are prepared **once,
  eagerly**, in `crsql_newExtData` (`ext-data.c:19`), and consumers use them **without a lazy re-prepare
  guard** — e.g. `crsql_fetchPragmaSchemaVersion` (`ext-data.c:210`) steps `pPragmaSchemaVersionStmt`
  directly. So `crsql_finalize` is **only safe to call when the connection is truly going away** (its own
  header comment says as much). E1 satisfies this — `SQLITE_TRACE_CLOSE` fires *only* from
  `sqlite3_close`. **B1 does not obviously satisfy it:** `changesDisconnect` runs on *every* vtab
  disconnect. For the *eponymous* `crsql_changes`, disconnect is close-only (`disconnectAllVtab`'s
  `pMod->pEpoTab` loop at `sqlite3.c:176080`; eponymous connect is created-once at `152359`), so B1 is
  *probably* safe **today**. But it silently depends on the invariant "no one ever does
  `CREATE VIRTUAL TABLE … USING crsql_changes` (a schema-resident instance, which disconnects
  mid-session on schema changes) and the module is never re-registered." If that invariant is ever
  violated, B1 finalizes live statements mid-session → next CRR op steps a NULL stmt → `SQLITE_MISUSE`
  / silent corruption — **worse than the leak it guards.**

### 11.2 Decision (divergence from §10.3/§10.6, with reason)

- **Ship E1 now.** Unambiguously safe (fires only at real close), covers **all** teardown paths
  regardless of whether `crsql_changes` was queried, and needs no amalgamation patch. This is the fix.
- **Defer B1.** Its only marginal benefit over E1 is surviving a *future* Exqlite trace-slot owner, and
  only for connections that queried `crsql_changes` — a hypothetical. Against that sits the mid-session
  finalize hazard in §11.1. Net: not worth shipping blind. **Revisit B1 only if** (a) the app actually
  enables Exqlite tracing on these pools, *and* (b) we first make the statement consumers re-prepare
  lazily (guard each use with `if (stmt == 0) prepare`), which would make `crsql_finalize` safe to call
  any time and retire the hazard. That lazy-re-prepare change is the more robust underlying fix and is
  the right prerequisite for B1.
- **A1/A2 stay dropped** (§10.1 — patch code that doesn't run in prod). **C1 dropped. D is the existing
  Elixir workaround.**
- **`crsql_build_id()` provenance marker (§10.4): planned as the immediate follow-up** to E1, once the
  E1 mechanism is green here. Low risk (a scalar SQL function returning a compile-time string); it's how
  the app confirms the E1-carrying NIF actually shipped, especially on Windows.

### 11.3 Implementation checklist

- [x] **E1** — `crsql_close_trace_hook` + `sqlite3_trace_v2(db, SQLITE_TRACE_CLOSE, …)` registration in
      `sqlite3_crsqlite_init` (`core/src/crsqlite.c:56` + `:104`).
- [x] **Tier-1 tests** (`core/src/crsqlite.test.c`, wired into `crsqlTestSuite`):
      (a) `testCloseReleasesCrsqlStmts` — CRR + query `crsql_changes` → `sqlite3_close` (v1) → `SQLITE_OK`;
      (b) `testCloseReleasesWithoutChangesQuery` — **write-only** (CRR writes + `crsql_db_version`, *no*
      `crsql_changes` query) → v1 close → `SQLITE_OK` (proves E1 fires where B1 would not);
      (c) `testCloseAfterExplicitFinalizeIsIdempotent` — explicit `SELECT crsql_finalize()` *then* close →
      `SQLITE_OK` (idempotency / double-finalize).
- [x] Proved **red→green**: `make test` with tests but no E1 → aborts on `SQLITE_BUSY`
      (`testCloseReleasesCrsqlStmts` assertion); with E1 → all three green + full suite passes.
- [x] **Memory-safety check via `make valgrind`** → `ERROR SUMMARY: 0 errors from 0 contexts`.
      (`make asan` was **not** usable: it panics in the cr-sqlite **Rust** bundle
      [`slice::from_raw_parts` unsafe-precondition, flagged only by the asan build's debug-std] across
      *unrelated* suites — `rows_impacted`, `sandbox`, `rust_integration` — and reproduces identically on
      a **clean tree with E1 stashed**. So it is a pre-existing latent issue in the vendored Rust bundle,
      unrelated to E1; valgrind stands in as the mem-safety oracle. Worth filing separately.)
- [x] **`crsql_build_id()`** scalar function — registered in the Rust bundle beside `crsql_sha` /
      `crsql_version` (`rs/core/src/lib.rs`; marker string in `rs/core/src/sha.rs`), so it rides both the
      static auto-extension and loadable-extension paths. Returns `"e1-fd-close <commit-sha>"`. Its mere
      presence proves the fork build (upstream has no such function → app reads "absent"); the `e1-fd-close`
      tag confirms the FD-close feature set. Test `testBuildIdReportsFeatureMarker` asserts the tag is
      present. **Verified:** returns e.g. `e1-fd-close 192d4807…`; `make test` green; `make valgrind` clean.
- [ ] Hand off to app repo for Tier-2 (pool_size-5 FD test — the real acceptance gate, §10.4). The app is
      already wired to consume `crsql_build_id()` (§10.4) — it lights up automatically now the function
      exists.

### 11.4 Verification results (this repo)

| Check | Command | Result |
|---|---|---|
| Red baseline (no E1) | `make test` | **`SQLITE_BUSY`** — `testCloseReleasesCrsqlStmts` assertion aborts ✔ (bug reproduced) |
| Green (with E1) | `make test` | All 3 close tests **Success**; full suite passes ✔ |
| Memory safety | `make valgrind` | **0 errors from 0 contexts** ✔ |
| asan | `make asan` | Pre-existing Rust-bundle panic, **unrelated to E1** (reproduced with E1 stashed) — tracked separately |

Toolchain note for future builds: the Rust bundle pins `nightly-2023-10-05` via `rust-toolchain.toml`
(uses `#![feature(concat_idents)]`, removed in Rust 1.90). Build with real `rustup` on `PATH`
(`export PATH="$HOME/.cargo/bin:$PATH"`) so the pin is honored; `asdf`'s `cargo` shim ignores the pin and
fails. First-time setup also needs `git submodule update --init --recursive` (for
`core/rs/sqlite-rs-embedded`) and, for asan, `rustup component add rust-src --toolchain nightly-2023-10-05`.

### 11.4 App-side review of §11 (reviewed against `silicon_brain` usage)

**Verdict: §11 is endorsed. Ship E1, defer B1.** The B1 mid-session-finalize hazard is real and its three
load-bearing assumptions were verified against actual app usage:

1. **No schema-resident `crsql_changes` vtab** anywhere (`grep`: no `CREATE VIRTUAL TABLE … USING
   crsql_changes`) — we use only the eponymous form. So B1 would be safe *today*, but the silent-invariant
   argument for deferring stands.
2. **Exqlite SQLite tracing is never enabled** on these pools (no `sqlite3_trace_v2` call anywhere in app
   or Exqlite NIF; the only "trace" is Erlang process tracing in `unbound_repo_tracer.ex`). So E1's sole
   weakness (trace-slot ownership) is uncontended — E1 alone is sufficient, and B1's only marginal benefit
   doesn't apply.
3. **Host finalize is close-time only** (`before_disconnect: crsql_finalize_on_disconnect/2` on both
   pools). With B1 gone, `crsql_finalize` runs at most **2×** — host `before_disconnect` + E1's trace hook,
   both at genuine teardown — so checklist test (c) covers the idempotency fully.

**One thing §11 (correctly) leaves out of scope, flagged so Tier-2 isn't misread — the Exqlite
co-blocker.** E1 fixes the *cr-sqlite* half. But the app's own §14.9 records (and I re-confirmed in
`deps/exqlite/c_src/sqlite3_nif.c:559`) that **`exqlite_close` calls `sqlite3_close_v2` without first
finalizing Exqlite's OWN cached prepared statements** (comment at :555-557: "we rely on the destructors to
later run to clean those up"). So at close the connection is a zombie held by **both** cr-sqlite's
statements *and* Exqlite's. Crucially, the two differ in fate:

- cr-sqlite's statements — **never** finalized today → this is what makes the leak *permanent* (until BEAM
  exit). **E1 removes exactly this blocker.**
- Exqlite's statements — finalized by the NIF `statement_type_destructor` on **GC** of each statement
  resource (async, but it *does* happen once the torn-down pool's processes become garbage).

**Therefore E1 alone converts the *permanent* leak into a *GC-latency-bounded transient*:** the zombie
completes (FDs release) once Exqlite's last statement resource GC-finalizes — which will happen, unlike
cr-sqlite's. **Implication for the Tier-2 FD gate (§10.4):** after `terminate_repo`, expect baseline only
**after a forced `:erlang.garbage_collect` + settle** (the existing FD tests already do GC+settle). Do
**not** read a synchronous post-`terminate` residual as "E1 failed" — re-measure post-GC.

**Open question Tier-2 answers:** does E1 + GC/settle reliably return to baseline, or do Exqlite's
statement resources not collect promptly/deterministically enough — in which case we *also* need the
Exqlite-side finalize-before-close (finalize all `sqlite3_next_stmt` tracked statements in `exqlite_close`
/ the connection destructor before `close_v2`; handoff §3 "Approach B", with its dangling-`statement_t`
caveat) to get **deterministic** release. That is an **Exqlite NIF change, not a cr-sqlite one** — out of
this repo's scope, but it's the known complement if the Tier-2 gate doesn't fully close on E1 alone.

**Landmine recorded app-side** (`CR-SQLITE-DATA-SYNC.md` §14.9.B): E1's `SQLITE_TRACE_CLOSE` hook is now
the *sole* FD-leak protection, so enabling Exqlite query tracing on these pools would silently clobber it
and resurrect the leak. Documented so it isn't discovered the hard way.

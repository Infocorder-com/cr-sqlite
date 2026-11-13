#include "crsqlite.h"
SQLITE_EXTENSION_INIT1
#ifdef LIBSQL
LIBSQL_EXTENSION_INIT1
#endif

#include <assert.h>
#include <ctype.h>
#include <limits.h>
#include <stdint.h>
#include <string.h>

#include "changes-vtab.h"
#include "consts.h"
#include "ext-data.h"
#include "rust.h"

// see
// https://github.com/chromium/chromium/commit/579b3dd0ea41a40da8a61ab87a8b0bc39e158998
// & https://github.com/rust-lang/rust/issues/73632 &
// https://sourcegraph.com/github.com/chromium/chromium/-/commit/579b3dd0ea41a40da8a61ab87a8b0bc39e158998?visible=1
#ifdef CRSQLITE_WASM
unsigned char __rust_no_alloc_shim_is_unstable;
#endif

int crsql_compact_post_alter(sqlite3 *db, const char *tblName,
                             crsql_ExtData *pExtData, char **errmsg);

int crsql_commit_hook(void *pUserData);
void crsql_rollback_hook(void *pUserData);

#ifdef LIBSQL
static void closeHook(void *pUserData, sqlite3 *db) {
  crsql_ExtData *pExtData = (crsql_ExtData *)pUserData;
  crsql_finalize(pExtData);
}
#endif

// Finalize cr-sqlite's own cached prepared statements as soon as the connection
// begins to close, so the OS file handles (*.db / -wal / -shm) are released
// without the host having to call `SELECT crsql_finalize()` first.
//
// SQLITE_TRACE_CLOSE fires at the very top of sqlite3_close / sqlite3_close_v2,
// *before* SQLite's "connection is busy?" check. Finalizing the pExtData
// statements there removes the outstanding-statement blockers cr-sqlite owns, so
// a v1 close returns SQLITE_OK and a deferred v2 close can complete instead of
// leaking the handles until the process exits.
//
// Unlike libsql_close_hook this uses only the stock, public sqlite3_trace_v2
// API, so it works whether cr-sqlite is a loadable extension or statically
// linked, and regardless of which SQLite amalgamation actually runs the close
// (e.g. the host's copy, not cr-sqlite's bundled one).
//
// Preconditions (silent no-op / FDs leak as before if not met):
//   - the SQLite running the close must have tracing compiled in (the default;
//     disabled only by SQLITE_OMIT_TRACE), and be >= 3.14 for SQLITE_TRACE_CLOSE
//     -- already implied by cr-sqlite's use of sqlite3_prepare_v3 (>= 3.20);
//   - no other code registers a sqlite3_trace_v2 callback on the same
//     connection -- there is ONE trace slot per connection, so a later host
//     trace registration would clobber this hook.
//
// It only ever touches cr-sqlite's own statements (crsql_finalize is idempotent
// and NULLs each pointer), runs under the connection mutex sqlite3_close already
// holds, and requests ONLY the CLOSE event, so there is no per-statement trace
// overhead. See docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md (E1).
static int crsql_close_trace_hook(unsigned traceType, void *pCtx, void *p,
                                  void *x) {
  if (traceType == SQLITE_TRACE_CLOSE) {
    crsql_finalize((crsql_ExtData *)pCtx);
  }
  return 0;
}

void *sqlite3_crsqlrustbundle_init(sqlite3 *db, char **pzErrMsg,
                                   const sqlite3_api_routines *pApi);
// The ownership handoff (core/rs/core/src/lib.rs): registers crsql_db_version
// with the destructor that frees pExtData. If it fails, SQLite has already
// run that destructor.
int crsql_register_db_version(sqlite3 *db, crsql_ExtData *pExtData);

#ifdef _WIN32
__declspec(dllexport)
#endif
    int sqlite3_crsqlite_init(sqlite3 *db, char **pzErrMsg,
                              const sqlite3_api_routines *pApi
#ifdef LIBSQL
                              ,
                              const libsql_api_routines *pLibsqlApi
#endif
    ) {
  int rc = SQLITE_OK;

  SQLITE_EXTENSION_INIT2(pApi);
#ifdef LIBSQL
  LIBSQL_EXTENSION_INIT2(pLibsqlApi);
#endif

  // TODO: should be moved lower once we finish migrating to rust.
  // RN it is safe here since the rust bundle init is largely just reigstering
  // function pointers. we need to init the rust bundle otherwise sqlite api
  // methods are not isntalled when we start calling rust
  //
  // On success the Rust init has committed the Atomic Init Transaction and
  // returns the ext data, which this function still owns until the handoff
  // below. On failure it has freed everything of its own.
  crsql_ExtData *pExtData = sqlite3_crsqlrustbundle_init(db, pzErrMsg, pApi);
  if (pExtData == 0) {
    return SQLITE_ERROR;
  }

#ifdef UNIT_TEST
  if (crsql_test_init_fail_step() == CRSQL_INIT_STEP_CHANGES_MODULE) {
    rc = SQLITE_ERROR;
  }
#endif
  if (rc == SQLITE_OK) {
    rc = sqlite3_create_module_v2(db, "crsql_changes", &crsql_changesModule,
                                  pExtData, 0);
  }
  if (rc != SQLITE_OK) {
    // Before the handoff: this function owns the ext data. Freeing it
    // finalizes its statements, so the host's sqlite3_close_v2 can deallocate
    // the connection (Init Error-Path Frees).
    crsql_freeExtData(pExtData);
    return rc;
  }

  // The ownership handoff -- init's LAST fallible step. crsql_db_version's
  // destructor (x_free_connection_ext_data) frees the ext data when the
  // connection closes. If the registration fails, SQLite has already run that
  // destructor, so do NOT free here. It is also the only destructor of this
  // library that init registers, and being last means a failed
  // load_extension never leaves it registered (Failed Load Unmapped Callback).
  rc = crsql_register_db_version(db, pExtData);
  if (rc != SQLITE_OK) {
    return rc;
  }

  // NOTHING FALLIBLE MAY FOLLOW. A failure after the handoff would have to
  // call crsql_finalize(pExtData) -- statements only; the destructor frees the
  // struct -- or the host's sqlite3_close_v2 leaves a zombie connection whose
  // file descriptors and -shm mapping leak until the process exits
  // (docs/CRSQLITE_INIT_RACE_FIX.md, Init Error-Path Frees). The hooks below
  // cannot fail.
#ifdef LIBSQL
  libsql_close_hook(db, closeHook, pExtData);
#endif
  // Release OS file handles on connection close without requiring the host to
  // call crsql_finalize() first. See crsql_close_trace_hook above (E1).
  sqlite3_trace_v2(db, SQLITE_TRACE_CLOSE, crsql_close_trace_hook, pExtData);
  // TODO: get the prior callback so we can call it rather than replace
  // it? (Applies to the trace/commit/rollback hooks: cr-sqlite currently
  // assumes it owns these per-connection callback slots.)
  sqlite3_commit_hook(db, crsql_commit_hook, pExtData);
  sqlite3_rollback_hook(db, crsql_rollback_hook, pExtData);

  return SQLITE_OK;
}

// ---------------------------------------------------------------------------
// Init Busy Timeout (docs/CRSQLITE_INIT_RACE_FIX.md).
//
// Init runs inside sqlite3_open_v2 when cr-sqlite is an auto-extension, before
// the host can install a busy handler, so without one any lock conflict during
// init is an immediate SQLITE_BUSY. sqlite3_crsqlite_autoext_init installs a
// handler for the duration of init and clears it afterwards.
//
// The handler enforces a PER-OPEN budget: the total time init spends sleeping
// on locks, across every step of the open. sqlite3_busy_timeout would bound
// each wait instead -- SQLite resets its busy count at every sqlite3_step and
// every prepare -- so an open could wait the full timeout at each of its
// lock-taking points.
// ---------------------------------------------------------------------------

#ifndef CRSQL_INIT_BUSY_TIMEOUT_MS
#define CRSQL_INIT_BUSY_TIMEOUT_MS 2000
#endif

typedef struct crsql_InitBusyBudget {
  int budget_ms;
  int slept_ms;
  int n_waits;
} crsql_InitBusyBudget;

#ifdef UNIT_TEST
static int crsql_test_init_busy_budget_ms = -1;
void crsql_test_set_init_busy_budget_ms(int ms) {
  crsql_test_init_busy_budget_ms = ms >= 0 ? ms : -1;
}
#endif

static int crsql_init_busy_budget_ms(void) {
#ifdef UNIT_TEST
  if (crsql_test_init_busy_budget_ms >= 0) {
    return crsql_test_init_busy_budget_ms;
  }
#endif
  return CRSQL_INIT_BUSY_TIMEOUT_MS;
}

// Ignores SQLite's per-statement count (nPrior). Backs off like SQLite's own
// default handler, shortens the last delay to fit the budget, and gives up
// (returns 0) once the budget is spent. Counts time slept, not wall time.
static int crsql_init_busy_handler(void *pArg, int nPrior) {
  static const int delays[] = {1, 2, 5, 10, 15, 20, 25, 25, 25, 50, 50, 100};
  const int nDelays = (int)(sizeof(delays) / sizeof(delays[0]));
  crsql_InitBusyBudget *pBudget = (crsql_InitBusyBudget *)pArg;
  int remaining = pBudget->budget_ms - pBudget->slept_ms;
  int delay;
  int slept;
  (void)nPrior;

  if (remaining <= 0) {
    return 0;
  }
  delay = pBudget->n_waits < nDelays ? delays[pBudget->n_waits] : 100;
  if (delay > remaining) {
    delay = remaining;
  }
  slept = sqlite3_sleep(delay);
  // A VFS without sub-second sleep rounds up (to whole seconds); count what it
  // reports, so the budget stays a bound on the time actually slept.
  pBudget->slept_ms += slept > delay ? slept : delay;
  pBudget->n_waits++;
  return 1;
}

// The auto-extension entry point. Register THIS with sqlite3_auto_extension
// (core_init.c does; hosts with their own startup shim must too) to get the
// Init Busy Timeout. Never use it with load_extension: the host may already
// have a busy handler there, SQLite has no API to read one back, and this
// clears it. sqlite3_crsqlite_init stays the load_extension entry point.
//
// Clearing restores the exact prior state on the auto-extension path, because
// init runs inside sqlite3_open_v2 before the host has a handle to install a
// handler on. (An auto-extension registered EARLIER that installed its own
// handler would lose it; this repo's builds register none.)
//
// LIBSQL: same conditional fourth argument as sqlite3_crsqlite_init. Untested:
// this fork has no libSQL build, and how libSQL invokes auto-extensions was
// not checked.
#ifdef _WIN32
__declspec(dllexport)
#endif
    int sqlite3_crsqlite_autoext_init(sqlite3 *db, char **pzErrMsg,
                                      const sqlite3_api_routines *pApi
#ifdef LIBSQL
                                      ,
                                      const libsql_api_routines *pLibsqlApi
#endif
    ) {
  int rc;
  crsql_InitBusyBudget budget;

  SQLITE_EXTENSION_INIT2(pApi);
#ifdef LIBSQL
  LIBSQL_EXTENSION_INIT2(pLibsqlApi);
#endif

  // The budget lives on this stack frame and the handler is cleared before
  // the frame returns, so nothing dangles.
  budget.budget_ms = crsql_init_busy_budget_ms();
  budget.slept_ms = 0;
  budget.n_waits = 0;
  sqlite3_busy_handler(db, crsql_init_busy_handler, &budget);

  rc = sqlite3_crsqlite_init(db, pzErrMsg, pApi
#ifdef LIBSQL
                             ,
                             pLibsqlApi
#endif
  );

  // On success AND on failure. Same as sqlite3_busy_timeout(db, 0).
  sqlite3_busy_handler(db, 0, 0);
  return rc;
}

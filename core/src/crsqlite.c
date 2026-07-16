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
// API, so it works in every build (loadable extension, static link, WASM) and
// regardless of which SQLite amalgamation actually runs the close. It only ever
// touches cr-sqlite's own statements (crsql_finalize is idempotent and NULLs
// each pointer), runs under the connection mutex sqlite3_close already holds,
// and requests ONLY the CLOSE event, so there is no per-statement trace
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
  crsql_ExtData *pExtData = sqlite3_crsqlrustbundle_init(db, pzErrMsg, pApi);
  if (pExtData == 0) {
    return SQLITE_ERROR;
  }

  if (rc == SQLITE_OK) {
    rc = sqlite3_create_module_v2(db, "crsql_changes", &crsql_changesModule,
                                  pExtData, 0);
  }

  if (rc == SQLITE_OK) {
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
  }

  return rc;
}
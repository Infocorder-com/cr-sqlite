#ifndef CRSQLITE_H
#define CRSQLITE_H
// NOTE: this include guard is shared with crsqlite.h, so a file that includes
// both gets only the first. core_init.c includes only this file; do not add
// it to a file that already includes crsqlite.h.

/**
 * Extension initialization routine that is run once per connection.
 * This is the load_extension entry point.
 */
int sqlite3_crsqlite_init(sqlite3 *db, char **pzErrMsg,
                          const sqlite3_api_routines *pApi
#ifdef LIBSQL
                          ,
                          const libsql_api_routines *pLibsqlApi
#endif
);

/**
 * The auto-extension entry point: sqlite3_crsqlite_init run under the Init
 * Busy Timeout, a busy handler with a per-open lock-wait budget that is
 * cleared before it returns (docs/CRSQLITE_INIT_RACE_FIX.md). Register it ONLY
 * with sqlite3_auto_extension, never with load_extension.
 */
int sqlite3_crsqlite_autoext_init(sqlite3 *db, char **pzErrMsg,
                                  const sqlite3_api_routines *pApi
#ifdef LIBSQL
                                  ,
                                  const libsql_api_routines *pLibsqlApi
#endif
);

#endif

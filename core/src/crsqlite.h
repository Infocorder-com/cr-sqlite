#ifndef CRSQLITE_H
#define CRSQLITE_H

#include "sqlite3ext.h"
SQLITE_EXTENSION_INIT3

#include <stdint.h>

#ifndef UNIT_TEST
#define STATIC static
#else
#define STATIC
#endif

#ifdef UNIT_TEST
// Test-only hooks for extension init (docs/CRSQLITE_INIT_RACE_FIX.md, "Tests").
// They compile to nothing in release builds.

// Sets the per-open lock-wait budget that sqlite3_crsqlite_autoext_init uses:
// ms >= 0 overrides it, ms < 0 restores CRSQL_INIT_BUSY_TIMEOUT_MS.
void crsql_test_set_init_busy_budget_ms(int ms);
// When non-zero, crsql_newExtData's `config.%` loop gets this code from its
// step instead of calling sqlite3_step.
extern int crsql_test_force_config_step_rc;
// The init step to fail (an InitStep value, core/rs/core/src/init_fault.rs;
// 0 = none). Defined in Rust (test_exports.rs).
void crsql_test_fail_init_at(int step);
int crsql_test_init_fail_step(void);
#define CRSQL_INIT_STEP_NEW_EXT_DATA 9
#define CRSQL_INIT_STEP_CHANGES_MODULE 16
#endif

#endif

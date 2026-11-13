pub use crate::bootstrap;
pub use crate::c;
pub use crate::db_version;
pub use crate::init_fault;
pub use crate::pack_columns;
pub use crate::tableinfo;

use core::ffi::c_int;
use core::sync::atomic::{AtomicI32, Ordering};

// The init step to fail (an `InitStep` value; 0 = none). The test binary is
// single-threaded, so a process-wide static is safe; an atomic only avoids
// `static mut`.
static INIT_FAIL_STEP: AtomicI32 = AtomicI32::new(0);

pub fn init_fail_step() -> i32 {
    INIT_FAIL_STEP.load(Ordering::Relaxed)
}

/// Test-only: make the next opens fail init at `step` (0 = no injection).
#[no_mangle]
pub extern "C" fn crsql_test_fail_init_at(step: c_int) {
    INIT_FAIL_STEP.store(step, Ordering::Relaxed);
}

/// Test-only: read by the C side of init (`crsql_newExtData`,
/// `sqlite3_crsqlite_init`) for the steps it owns.
#[no_mangle]
pub extern "C" fn crsql_test_init_fail_step() -> c_int {
    init_fail_step()
}

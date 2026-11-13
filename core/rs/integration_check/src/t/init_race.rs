//! `init_race_*` -- deterministic tests for the **Atomic Init Transaction**, the
//! **Init Residue Heal**, the **Init Busy Timeout** and **Init Error-Path Frees**
//! (docs/CRSQLITE_INIT_RACE_FIX.md, "Deterministic tests").
//!
//! Run as their own suite: `dist/test init_race`. One test alone:
//! `CRSQL_INIT_RACE_ONLY=init_race_commit_busy dist/test init_race`.
//!
//! Every test returns a `Result` so one run reports every failure; the suite
//! aborts at the end if any failed. Files live under `$TMPDIR` (only when it is
//! under `/tmp`) or `/tmp`, and every deletion is guarded to that directory.
//!
//! The test binary is single-threaded (`SQLITE_THREADSAFE=0`), so contention is
//! built from several connections on one thread. A connection WITHOUT
//! cr-sqlite is opened by resetting the auto-extension list, opening, and
//! calling `core_init` again, which re-registers whichever entry point this
//! build uses.
extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_long, c_void, CStr};
use core::ptr::null_mut;
use core::sync::atomic::{AtomicU32, Ordering};
use crsql_bundle::test_exports::init_fault::InitStep;
use libc_print::std_name::println;
use sqlite_nostd::bindings as b;

type TResult = Result<(), String>;

macro_rules! check {
    ($cond:expr, $($fmt:tt)+) => {
        if !$cond {
            return Err(format!($($fmt)+));
        }
    };
}

#[repr(C)]
struct Timespec {
    tv_sec: c_long,
    tv_nsec: c_long,
}

extern "C" {
    fn getenv(name: *const c_char) -> *const c_char;
    fn getpid() -> c_int;
    fn unlink(path: *const c_char) -> c_int;
    fn fopen(path: *const c_char, mode: *const c_char) -> *mut c_void;
    fn fread(ptr: *mut c_void, size: usize, n: usize, f: *mut c_void) -> usize;
    fn fwrite(ptr: *const c_void, size: usize, n: usize, f: *mut c_void) -> usize;
    fn fclose(f: *mut c_void) -> c_int;
    fn clock_gettime(clk: c_int, ts: *mut Timespec) -> c_int;
    #[cfg(unix)]
    fn opendir(path: *const c_char) -> *mut c_void;
    #[cfg(unix)]
    fn readdir(dir: *mut c_void) -> *mut c_void;
    #[cfg(unix)]
    fn closedir(dir: *mut c_void) -> c_int;

    // core/src/crsqlite.c: the load_extension entry point. This binary is
    // built with SQLITE_OMIT_LOAD_EXTENSION, so SQLITE_EXTENSION_INIT2
    // ignores the API routines pointer and a null one is correct here.
    fn sqlite3_crsqlite_init(
        db: *mut b::sqlite3,
        pz_err_msg: *mut *mut c_char,
        api: *const c_void,
    ) -> c_int;

    // core/src/core_init.c: registers this build's auto-extension entry point.
    fn core_init(dummy: *const c_char) -> c_int;
    // UNIT_TEST hooks (core/src/crsqlite.h).
    fn crsql_test_set_init_busy_budget_ms(ms: c_int);
    fn crsql_test_fail_init_at(step: c_int);
    static mut crsql_test_force_config_step_rc: c_int;
}

const OPEN_RW: c_int = (b::SQLITE_OPEN_READWRITE | b::SQLITE_OPEN_CREATE) as c_int;
const OPEN_RO: c_int = b::SQLITE_OPEN_READONLY as c_int;
const SQLITE_OK: c_int = 0;
const SQLITE_BUSY: c_int = 5;
const SQLITE_READONLY: c_int = 8;
/// What `sqlite3AutoLoadExtensions` prefixes to an auto-extension's message.
const AUTOEXT_PREFIX: &str = "automatic extension loading failed: ";
/// cr-sqlite's own message for an init that ran out of lock budget.
const LOCKED_MSG: &str = "cr-sqlite init: database is locked";
/// A test that cannot run here returns `Err(skipped(reason))`; the runner
/// reports it as SKIPPED, never as ok.
const SKIP_PREFIX: &str = "SKIPPED: ";
const CRSQLITE_VERSION: i64 = 170000;

#[cfg(target_os = "macos")]
const CLOCK_MONOTONIC: c_int = 6;
#[cfg(not(target_os = "macos"))]
const CLOCK_MONOTONIC: c_int = 1;

// Same DDL as core/rs/core/src/bootstrap.rs: the three tables a
// Half-Initialized Database holds.
const RESIDUE_DDL: &str = "CREATE TABLE IF NOT EXISTS crsql_tracked_peers (\"site_id\" BLOB NOT NULL, \"version\" INTEGER NOT NULL, \"seq\" INTEGER DEFAULT 0, \"tag\" INTEGER, \"event\" INTEGER, PRIMARY KEY (\"site_id\", \"tag\", \"event\")) STRICT;
CREATE TABLE IF NOT EXISTS crsql_db_versions (\"site_id\" BLOB NOT NULL PRIMARY KEY, \"db_version\" INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS \"crsql_master\" (\"key\" TEXT PRIMARY KEY, \"value\" ANY);";

const FIELD_SAMPLE: &str = "src/fixtures/halfinit.halfinit-db";
const PRIOR_V0_12: &str = "../py/correctness/prior-dbs/v0.12.0.prior-db";
const PRIOR_V0_13: &str = "../py/correctness/prior-dbs/v0.13.0.prior-db";
const PRIOR_V0_15: &str = "../py/correctness/prior-dbs/v0.15.0.prior-db";

// ---------------------------------------------------------------- helpers ---

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn tmp_base() -> String {
    let env = unsafe { getenv(cstr("TMPDIR").as_ptr()) };
    if !env.is_null() {
        if let Ok(s) = unsafe { CStr::from_ptr(env) }.to_str() {
            let s = s.trim_end_matches('/');
            if s == "/tmp" || s.starts_with("/tmp/") {
                return s.to_string();
            }
        }
    }
    "/tmp".to_string()
}

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A fresh, unique database path under the temp base. Nothing exists there.
fn tmp_db(tag: &str) -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = format!(
        "{}/crsql_init_race_{}_{}_{}.db",
        tmp_base(),
        unsafe { getpid() },
        n,
        tag
    );
    remove_db(&path);
    path
}

/// Remove a test database and its side files. Refuses any path outside the
/// temp base or not minted by `tmp_db`.
fn remove_db(path: &str) {
    let base = tmp_base();
    assert!(
        path.starts_with(&format!("{}/", base)) && path.contains("/crsql_init_race_"),
        "refusing to delete {}",
        path
    );
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let p = cstr(&format!("{}{}", path, suffix));
        unsafe { unlink(p.as_ptr()) };
    }
}

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    let f = unsafe { fopen(cstr(path).as_ptr(), cstr("rb").as_ptr()) };
    if f.is_null() {
        return Err(format!("cannot open {} for reading", path));
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe { fread(buf.as_mut_ptr() as *mut c_void, 1, buf.len(), f) };
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    unsafe { fclose(f) };
    Ok(out)
}

/// Byte copy of a fixture to a temp path. The fixture itself is only read.
fn copy_fixture(src: &str, tag: &str) -> Result<String, String> {
    let bytes = read_file(src)?;
    let dst = tmp_db(tag);
    let f = unsafe { fopen(cstr(&dst).as_ptr(), cstr("wb").as_ptr()) };
    if f.is_null() {
        return Err(format!("cannot create {}", dst));
    }
    let n = unsafe { fwrite(bytes.as_ptr() as *const c_void, 1, bytes.len(), f) };
    unsafe { fclose(f) };
    check!(n == bytes.len(), "short write copying {}", src);
    Ok(dst)
}

fn now_ms() -> i64 {
    let mut ts = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as i64) * 1000 + (ts.tv_nsec as i64) / 1_000_000
}

/// Where the open file descriptors of this process are listed: Linux, then
/// macOS and the BSDs.
#[cfg(unix)]
const FD_DIRS: [&str; 2] = ["/proc/self/fd", "/dev/fd"];

/// Open file descriptors of this process, or None where no fd directory
/// exists (Windows). Tests that need it report SKIPPED then, never ok.
#[cfg(unix)]
fn fd_count() -> Option<usize> {
    for dir in FD_DIRS {
        let d = unsafe { opendir(cstr(dir).as_ptr()) };
        if d.is_null() {
            continue;
        }
        let mut n = 0;
        while !unsafe { readdir(d) }.is_null() {
            n += 1;
        }
        unsafe { closedir(d) };
        return Some(n);
    }
    None
}
#[cfg(not(unix))]
fn fd_count() -> Option<usize> {
    None
}

fn skipped(reason: &str) -> String {
    format!("{}{}", SKIP_PREFIX, reason)
}

const NO_FD_DIR: &str = "no fd directory (/proc/self/fd or /dev/fd) to count file descriptors";

struct Opened {
    rc: c_int,
    db: *mut b::sqlite3,
    err: String,
}

fn errmsg(db: *mut b::sqlite3) -> String {
    if db.is_null() {
        return "(null handle)".to_string();
    }
    unsafe { CStr::from_ptr(b::sqlite3_errmsg(db)) }
        .to_str()
        .unwrap_or("?")
        .to_string()
}

/// Open with cr-sqlite (this build's auto-extension). The caller closes.
fn open_cr(path: &str, flags: c_int) -> Opened {
    let mut db: *mut b::sqlite3 = null_mut();
    let rc = unsafe { b::sqlite3_open_v2(cstr(path).as_ptr(), &mut db, flags, core::ptr::null()) };
    let err = if rc == SQLITE_OK {
        String::new()
    } else {
        errmsg(db)
    };
    Opened { rc, db, err }
}

/// Open WITHOUT cr-sqlite.
fn open_plain(path: &str, flags: c_int) -> Result<*mut b::sqlite3, String> {
    let mut db: *mut b::sqlite3 = null_mut();
    unsafe { b::sqlite3_reset_auto_extension() };
    let rc = unsafe { b::sqlite3_open_v2(cstr(path).as_ptr(), &mut db, flags, core::ptr::null()) };
    unsafe { core_init(core::ptr::null()) };
    if rc != SQLITE_OK {
        let e = errmsg(db);
        close_v2(db);
        return Err(format!("plain open of {} failed: {} {}", path, rc, e));
    }
    Ok(db)
}

fn close_v2(db: *mut b::sqlite3) -> c_int {
    if db.is_null() {
        return SQLITE_OK;
    }
    unsafe { b::sqlite3_close_v2(db) }
}

fn exec(db: *mut b::sqlite3, sql: &str) -> Result<(), String> {
    let rc = unsafe { b::sqlite3_exec(db, cstr(sql).as_ptr(), None, null_mut(), null_mut()) };
    if rc != SQLITE_OK {
        return Err(format!("exec `{}` failed: {} {}", sql, rc, errmsg(db)));
    }
    Ok(())
}

/// First column of the first row as an integer (None if no row).
fn query_i64(db: *mut b::sqlite3, sql: &str) -> Result<Option<i64>, String> {
    let mut stmt: *mut b::sqlite3_stmt = null_mut();
    let rc = unsafe { b::sqlite3_prepare_v2(db, cstr(sql).as_ptr(), -1, &mut stmt, null_mut()) };
    if rc != SQLITE_OK {
        return Err(format!("prepare `{}` failed: {} {}", sql, rc, errmsg(db)));
    }
    let step = unsafe { b::sqlite3_step(stmt) };
    let out = if step == b::SQLITE_ROW as c_int {
        Ok(Some(unsafe { b::sqlite3_column_int64(stmt, 0) }))
    } else if step == b::SQLITE_DONE as c_int {
        Ok(None)
    } else {
        Err(format!("step `{}` failed: {} {}", sql, step, errmsg(db)))
    };
    unsafe { b::sqlite3_finalize(stmt) };
    out
}

fn count(db: *mut b::sqlite3, sql: &str) -> Result<i64, String> {
    Ok(query_i64(db, sql)?.unwrap_or(-1))
}

/// Number of cr-sqlite objects in a file, read WITHOUT cr-sqlite (so nothing
/// heals or initializes the file while it is inspected).
fn crsql_objects(path: &str) -> Result<i64, String> {
    let db = open_plain(path, OPEN_RW)?;
    let n = count(
        db,
        "SELECT count(*) FROM sqlite_master WHERE instr(lower(name), 'crsql') > 0",
    );
    close_v2(db);
    n
}

/// A fully initialized database, checked through an open connection.
fn assert_complete(db: *mut b::sqlite3) -> TResult {
    let v = query_i64(
        db,
        "SELECT value FROM crsql_master WHERE key = 'crsqlite_version'",
    )?;
    check!(v == Some(CRSQLITE_VERSION), "version row is {:?}", v);
    let n = count(db, "SELECT count(*) FROM crsql_site_id WHERE ordinal = 0")?;
    check!(n == 1, "ordinal-0 site ids: {}", n);
    let n = count(
        db,
        "SELECT count(*) FROM sqlite_master WHERE type = 'trigger' AND tbl_name = 'crsql_site_id'",
    )?;
    check!(n == 3, "site-id triggers: {}", n);
    let n = count(
        db,
        "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'crsql_site_id_site_id'",
    )?;
    check!(n == 1, "site-id unique index: {}", n);
    let n = count(
        db,
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('crsql_tracked_peers', 'crsql_db_versions', 'crsql_master', 'crsql_site_id')",
    )?;
    check!(n == 4, "bookkeeping tables: {}", n);
    let n = count(db, "SELECT length(crsql_site_id())")?;
    check!(n == 16, "crsql_site_id() length {}", n);
    Ok(())
}

/// Open with cr-sqlite, expect success and a complete file, close.
fn open_expect_complete(path: &str) -> TResult {
    let o = open_cr(path, OPEN_RW);
    if o.rc != SQLITE_OK {
        close_v2(o.db);
        return Err(format!("open failed: {} {}", o.rc, o.err));
    }
    let r = assert_complete(o.db);
    let crc = close_v2(o.db);
    r?;
    check!(crc == SQLITE_OK, "close returned {}", crc);
    Ok(())
}

/// Open with cr-sqlite, expect failure whose message contains `needle`.
fn open_expect_failure(path: &str, needle: &str) -> TResult {
    let o = open_cr(path, OPEN_RW);
    let crc = close_v2(o.db);
    check!(o.rc != SQLITE_OK, "open unexpectedly succeeded");
    check!(
        o.err.contains(needle),
        "error `{}` does not contain `{}`",
        o.err,
        needle
    );
    check!(crc == SQLITE_OK, "close after failed open returned {}", crc);
    Ok(())
}

/// A synthesized Half-Initialized Database plus `extra` SQL.
fn residue_with(tag: &str, extra: &str) -> Result<String, String> {
    let path = tmp_db(tag);
    let db = open_plain(&path, OPEN_RW)?;
    let r = exec(db, RESIDUE_DDL).and_then(|_| exec(db, extra));
    close_v2(db);
    r?;
    Ok(path)
}

/// A fully initialized file.
fn initialized(tag: &str) -> Result<String, String> {
    let path = tmp_db(tag);
    open_expect_complete(&path)?;
    Ok(path)
}

/// `CRSQL_INIT_RACE_SKIP_STEP=n`: diagnostic only -- skip one injection step in
/// the fault loops (for example a step that crashes an unfixed build).
fn skip_step(step: i32) -> bool {
    let s = unsafe { getenv(cstr("CRSQL_INIT_RACE_SKIP_STEP").as_ptr()) };
    if s.is_null() {
        return false;
    }
    unsafe { CStr::from_ptr(s) }
        .to_str()
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        == Some(step)
}

fn verbose() -> bool {
    !unsafe { getenv(cstr("CRSQL_INIT_RACE_VERBOSE").as_ptr()) }.is_null()
}

struct FailAt;
impl FailAt {
    fn set(step: i32) -> FailAt {
        unsafe { crsql_test_fail_init_at(step) };
        FailAt
    }
}
impl Drop for FailAt {
    fn drop(&mut self) {
        unsafe { crsql_test_fail_init_at(0) };
    }
}

struct Budget;
impl Budget {
    fn set(ms: c_int) -> Budget {
        unsafe { crsql_test_set_init_busy_budget_ms(ms) };
        Budget
    }
}
impl Drop for Budget {
    fn drop(&mut self) {
        unsafe { crsql_test_set_init_busy_budget_ms(-1) };
    }
}

// ------------------------------------------------------------------ tests ---

fn init_race_step_count() -> TResult {
    check!(
        InitStep::COUNT == 17,
        "InitStep::COUNT is {}",
        InitStep::COUNT
    );
    check!(
        InitStep::OwnershipHandoff as i32 == InitStep::COUNT,
        "the last InitStep is not COUNT"
    );
    Ok(())
}

fn init_race_heals_field_sample() -> TResult {
    let path = copy_fixture(FIELD_SAMPLE, "field")?;
    let r = open_expect_complete(&path);
    remove_db(&path);
    r
}

fn init_race_heals_synthesized_residue() -> TResult {
    let path = residue_with("synth", "SELECT 1")?;
    let r = open_expect_complete(&path);
    remove_db(&path);
    r
}

fn refuses_unchanged(fixture: &str, tag: &str, needle: &str) -> TResult {
    let path = copy_fixture(fixture, tag)?;
    let before = read_file(&path)?;
    let r = open_expect_failure(&path, needle);
    let after = read_file(&path)?;
    remove_db(&path);
    r?;
    check!(
        before == after,
        "{}: file bytes changed by a refused open",
        fixture
    );
    Ok(())
}

fn init_race_heal_refuses_v0_12() -> TResult {
    refuses_unchanged(PRIOR_V0_12, "v012", "version 0 is not supported")
}

fn init_race_heal_refuses_v0_13_v0_15() -> TResult {
    refuses_unchanged(PRIOR_V0_13, "v013", "version 130000 is not supported")?;
    refuses_unchanged(PRIOR_V0_15, "v015", "version 150000 is not supported")
}

fn init_race_heal_refuses_nonempty() -> TResult {
    let path = residue_with(
        "peer",
        "INSERT INTO crsql_tracked_peers (site_id, version, tag, event) VALUES (x'01', 1, 0, 0)",
    )?;
    let r = open_expect_failure(&path, "version 0 is not supported");
    remove_db(&path);
    r?;
    let path = residue_with(
        "config",
        "INSERT INTO crsql_master (key, value) VALUES ('config.x', 1)",
    )?;
    let r = open_expect_failure(&path, "version 0 is not supported");
    remove_db(&path);
    r
}

fn init_race_heal_refuses_crsql_object() -> TResult {
    let path = residue_with("clock", "CREATE TABLE foo__crsql_clock (a)")?;
    let r = open_expect_failure(&path, "version 0 is not supported");
    remove_db(&path);
    r?;
    // Mixed case: the match must be case-insensitive.
    let path = residue_with("Clock", "CREATE TABLE Foo__CRSQL_Clock (a)")?;
    let r = open_expect_failure(&path, "version 0 is not supported");
    remove_db(&path);
    r
}

/// Heal rule 3 exempts the three bookkeeping TABLES by name, not any object
/// that happens to carry one of their names: a user trigger called
/// `crsql_master` is a cr-sqlite-named object and must block the heal.
fn init_race_heal_refuses_crsql_named_trigger() -> TResult {
    let path = residue_with(
        "trig",
        "CREATE TABLE app (a); CREATE TRIGGER crsql_master AFTER INSERT ON app BEGIN SELECT 1; END;",
    )?;
    let before = read_file(&path)?;
    let r = open_expect_failure(&path, "version 0 is not supported");
    let after = read_file(&path)?;
    remove_db(&path);
    r?;
    check!(before == after, "file bytes changed by a refused open");
    Ok(())
}

fn init_race_heal_app_tables() -> TResult {
    let path = residue_with(
        "app",
        "CREATE TABLE app (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO app (v) VALUES ('a'), ('b'), ('c')",
    )?;
    let o = open_cr(&path, OPEN_RW);
    let r = (|| -> TResult {
        check!(o.rc == SQLITE_OK, "open failed: {} {}", o.rc, o.err);
        assert_complete(o.db)?;
        let n = count(o.db, "SELECT count(*) FROM app WHERE v IN ('a', 'b', 'c')")?;
        check!(n == 3, "app rows: {}", n);
        Ok(())
    })();
    close_v2(o.db);
    remove_db(&path);
    r
}

/// Fails init at every persistent step, and at the COMMIT, on a fresh file.
/// The file must be left with NO cr-sqlite object, and open cleanly next time.
fn init_race_atomic_at_each_step() -> TResult {
    let mut failures = Vec::new();
    for step in 1..=(InitStep::Commit as i32) {
        let path = tmp_db("atomic");
        let r = (|| -> TResult {
            {
                let _f = FailAt::set(step);
                let o = open_cr(&path, OPEN_RW);
                close_v2(o.db);
                check!(o.rc != SQLITE_OK, "open succeeded although step failed");
            }
            let n = crsql_objects(&path)?;
            check!(n == 0, "{} cr-sqlite objects left behind", n);
            open_expect_complete(&path)
        })();
        remove_db(&path);
        if let Err(e) = r {
            failures.push(format!("step {}: {}", step, e));
        }
    }
    check!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

/// Every injection point, on a fresh file and on an initialized file. Each
/// open is closed with sqlite3_close_v2 (as Exqlite does). Under
/// `make valgrind-strict` this also proves no leak, double free or use after
/// free. On an initialized file the fast path must NOT reach `BEGIN IMMEDIATE`
/// or the version write -- which also pins that the completeness check is not
/// too strict.
fn init_race_error_paths_free_once() -> TResult {
    let mut failures = Vec::new();
    let init_path = initialized("freeonce_init")?;
    for step in 1..=InitStep::COUNT {
        if skip_step(step) {
            continue;
        }
        for fresh in [true, false] {
            let path = if fresh {
                tmp_db("freeonce")
            } else {
                init_path.clone()
            };
            if verbose() {
                println!("  free_once step {} fresh={}", step, fresh);
            }
            let (rc, crc) = {
                let _f = FailAt::set(step);
                let o = open_cr(&path, OPEN_RW);
                (o.rc, close_v2(o.db))
            };
            let expect_fail = fresh
                || !(step == InitStep::BeginImmediate as i32
                    || step == InitStep::VersionWrite as i32);
            if expect_fail && rc == SQLITE_OK {
                failures.push(format!("step {} fresh={}: open succeeded", step, fresh));
            }
            if !expect_fail && rc != SQLITE_OK {
                failures.push(format!(
                    "step {} fresh={}: an initialized file reached this step",
                    step, fresh
                ));
            }
            if crc != SQLITE_OK {
                failures.push(format!(
                    "step {} fresh={}: close_v2 returned {}",
                    step, fresh, crc
                ));
            }
            if fresh {
                remove_db(&path);
            }
        }
    }
    remove_db(&init_path);
    check!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

/// A late failure must not leave a zombie connection: after close_v2 the
/// process holds no more file descriptors than before the open.
fn init_race_late_failure_no_zombie() -> TResult {
    if fd_count().is_none() {
        return Err(skipped(NO_FD_DIR));
    }
    let mut failures = Vec::new();
    let init_path = initialized("zombie_init")?;
    for step in (InitStep::NewExtData as i32)..=InitStep::COUNT {
        if skip_step(step) {
            continue;
        }
        for fresh in [true, false] {
            let path = if fresh {
                tmp_db("zombie")
            } else {
                init_path.clone()
            };
            if verbose() {
                println!("  no_zombie step {} fresh={}", step, fresh);
            }
            let before = fd_count().unwrap();
            {
                let _f = FailAt::set(step);
                let o = open_cr(&path, OPEN_RW);
                close_v2(o.db);
            }
            let after = fd_count().unwrap();
            if after != before {
                failures.push(format!(
                    "step {} fresh={}: {} fds before, {} after",
                    step, fresh, before, after
                ));
            }
            if fresh {
                remove_db(&path);
            }
        }
    }
    remove_db(&init_path);
    check!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

/// P (no cr-sqlite) holds BEGIN IMMEDIATE on a fresh file. C's open must WAIT
/// for the budget, then fail cleanly, writing nothing. Before the fix C fails
/// at once, so only the lower timing bound tells the two apart.
fn init_race_loser_writes_nothing() -> TResult {
    let path = tmp_db("loser");
    let r = (|| -> TResult {
        let _b = Budget::set(200);
        let p = open_plain(&path, OPEN_RW)?;
        exec(p, "BEGIN IMMEDIATE")?;
        let t0 = now_ms();
        let o = open_cr(&path, OPEN_RW);
        let elapsed = now_ms() - t0;
        close_v2(o.db);
        let _ = exec(p, "ROLLBACK");
        close_v2(p);
        check!(
            o.rc != SQLITE_OK,
            "open succeeded while P held the write lock"
        );
        check!(
            elapsed >= 150,
            "open gave up after {} ms: it did not wait",
            elapsed
        );
        check!(elapsed < 1200, "open took {} ms: over budget", elapsed);
        let n = crsql_objects(&path)?;
        check!(n == 0, "{} cr-sqlite objects written by the loser", n);
        Ok(())
    })();
    remove_db(&path);
    r
}

/// P (no cr-sqlite) holds SHARED on a fresh file, so C's COMMIT cannot get
/// EXCLUSIVE. C must wait out the budget, roll back and leave the file empty;
/// once P ends, the next open succeeds.
fn init_race_commit_busy() -> TResult {
    let path = tmp_db("commitbusy");
    let r = (|| -> TResult {
        let _b = Budget::set(200);
        let p = open_plain(&path, OPEN_RW)?;
        exec(p, "BEGIN; SELECT count(*) FROM sqlite_master;")?;
        let t0 = now_ms();
        let o = open_cr(&path, OPEN_RW);
        let elapsed = now_ms() - t0;
        close_v2(o.db);
        let _ = exec(p, "COMMIT");
        close_v2(p);
        check!(o.rc != SQLITE_OK, "open succeeded while P held SHARED");
        check!(
            elapsed >= 150,
            "open gave up after {} ms: it did not wait",
            elapsed
        );
        check!(elapsed < 1200, "open took {} ms: over budget", elapsed);
        let n = crsql_objects(&path)?;
        check!(n == 0, "{} cr-sqlite objects left after a failed COMMIT", n);
        open_expect_complete(&path)
    })();
    remove_db(&path);
    r
}

/// Re-opening an initialized file takes no write lock: it succeeds at once
/// while P holds BEGIN IMMEDIATE. Only the read-write half can detect an
/// over-strict completeness check.
fn init_race_reopen_writes_nothing() -> TResult {
    let path = initialized("reopen")?;
    let r = (|| -> TResult {
        let _b = Budget::set(2000);
        let p = open_plain(&path, OPEN_RW)?;
        exec(p, "BEGIN IMMEDIATE")?;
        let result = (|| -> TResult {
            for (flags, label) in [(OPEN_RW, "read-write"), (OPEN_RO, "read-only")] {
                let t0 = now_ms();
                let o = open_cr(&path, flags);
                let elapsed = now_ms() - t0;
                let complete = if o.rc == SQLITE_OK {
                    assert_complete(o.db)
                } else {
                    Ok(())
                };
                close_v2(o.db);
                check!(
                    o.rc == SQLITE_OK,
                    "{} open failed: {} {}",
                    label,
                    o.rc,
                    o.err
                );
                complete?;
                check!(elapsed < 1000, "{} open waited {} ms", label, elapsed);
            }
            Ok(())
        })();
        let _ = exec(p, "ROLLBACK");
        close_v2(p);
        result
    })();
    remove_db(&path);
    r
}

/// SQLITE_BUSY from the `config.%` loop must fail the open, not silently
/// fall back to the default setting.
fn init_race_config_busy_fails() -> TResult {
    let path = tmp_db("config");
    let r = (|| -> TResult {
        let o = open_cr(&path, OPEN_RW);
        check!(o.rc == SQLITE_OK, "first open failed: {}", o.err);
        let set = exec(o.db, "SELECT crsql_config_set('merge-equal-values', 1)");
        close_v2(o.db);
        set?;

        unsafe { crsql_test_force_config_step_rc = SQLITE_BUSY };
        let o = open_cr(&path, OPEN_RW);
        unsafe { crsql_test_force_config_step_rc = 0 };
        close_v2(o.db);
        check!(
            o.rc != SQLITE_OK,
            "open succeeded although the config read was busy"
        );

        let o = open_cr(&path, OPEN_RW);
        let v = if o.rc == SQLITE_OK {
            query_i64(o.db, "SELECT crsql_config_get('merge-equal-values')")
        } else {
            Err(o.err.clone())
        };
        close_v2(o.db);
        check!(
            v == Ok(Some(1)),
            "merge-equal-values after re-open: {:?}",
            v
        );
        Ok(())
    })();
    remove_db(&path);
    r
}

/// A broken `merge-equal-values` setting fails the open; the statement of the
/// `config.%` read must be finalized, so the connection closes (no zombie).
fn init_race_config_broken_setting_closes() -> TResult {
    let path = initialized("broken")?;
    let r = (|| -> TResult {
        let p = open_plain(&path, OPEN_RW)?;
        let w = exec(
            p,
            "INSERT OR REPLACE INTO crsql_master (key, value) VALUES ('config.merge-equal-values', 'x')",
        );
        close_v2(p);
        w?;
        let before = fd_count();
        let o = open_cr(&path, OPEN_RW);
        let crc = close_v2(o.db);
        let after = fd_count();
        check!(o.rc != SQLITE_OK, "open with a broken setting succeeded");
        check!(crc == SQLITE_OK, "close_v2 returned {}", crc);
        // The checks above ran; without an fd directory the leak half cannot.
        if before.is_none() {
            return Err(skipped(NO_FD_DIR));
        }
        check!(
            before == after,
            "fds {:?} before, {:?} after",
            before,
            after
        );
        Ok(())
    })();
    remove_db(&path);
    r
}

/// Calls `sqlite3_crsqlite_init` as `sqlite3_load_extension` calls an
/// extension's entry point: no busy handler of init's own, and a message the
/// caller frees with `sqlite3_free`. This binary is built with
/// SQLITE_OMIT_LOAD_EXTENSION, so the real `load_extension` (which always
/// returns SQLITE_ERROR, prefixing the message) is covered by py
/// `test_init_race_busy_load_message`.
fn load_path_init(db: *mut b::sqlite3) -> (c_int, Option<String>) {
    let mut err: *mut c_char = null_mut();
    let rc = unsafe { sqlite3_crsqlite_init(db, &mut err, core::ptr::null()) };
    let msg = if err.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(err) }
                .to_str()
                .unwrap_or("?")
                .to_string(),
        )
    };
    unsafe { b::sqlite3_free(err as *mut c_void) };
    (rc, msg)
}

/// A lock failure reports SQLITE_BUSY and says so, on every path: the
/// auto-extension open (under the **Init Busy Timeout**), the load_extension
/// entry point in autocommit mode, and inside a host transaction (the
/// `SAVEPOINT crsql_init` path, whose first write is refused). Before the
/// fix all three returned SQLITE_ERROR with an empty message. Under
/// `make valgrind-strict` this also proves the message is freed by SQLite's
/// allocator without a leak or a mismatched free.
fn init_race_busy_reports_code_and_message() -> TResult {
    let path = tmp_db("busymsg");
    let r = (|| -> TResult {
        let _b = Budget::set(100);
        let p = open_plain(&path, OPEN_RW)?;
        exec(p, "BEGIN IMMEDIATE")?;
        let result = (|| -> TResult {
            let o = open_cr(&path, OPEN_RW);
            close_v2(o.db);
            check!(
                o.rc == SQLITE_BUSY,
                "auto-extension open returned {} `{}`",
                o.rc,
                o.err
            );
            let want = format!("{}{}", AUTOEXT_PREFIX, LOCKED_MSG);
            check!(
                o.err == want,
                "auto-extension message `{}`, want `{}`",
                o.err,
                want
            );

            let l = open_plain(&path, OPEN_RW)?;
            let (rc, msg) = load_path_init(l);
            let crc = close_v2(l);
            check!(rc == SQLITE_BUSY, "load path returned {} {:?}", rc, msg);
            check!(
                msg.as_deref() == Some(LOCKED_MSG),
                "load path message {:?}",
                msg
            );
            check!(crc == SQLITE_OK, "load path close_v2 returned {}", crc);

            let l = open_plain(&path, OPEN_RW)?;
            let begun = exec(l, "BEGIN; SELECT count(*) FROM sqlite_master;");
            let (rc, msg) = if begun.is_ok() {
                load_path_init(l)
            } else {
                (-1, None)
            };
            let _ = exec(l, "ROLLBACK");
            let crc = close_v2(l);
            begun?;
            check!(
                rc == SQLITE_BUSY,
                "load path in a host transaction returned {} {:?}",
                rc,
                msg
            );
            check!(
                msg.as_deref() == Some(LOCKED_MSG),
                "load path in a host transaction: message {:?}",
                msg
            );
            check!(crc == SQLITE_OK, "close_v2 returned {}", crc);
            Ok(())
        })();
        let _ = exec(p, "ROLLBACK");
        close_v2(p);
        result?;
        let n = crsql_objects(&path)?;
        check!(n == 0, "{} cr-sqlite objects written by failed inits", n);
        Ok(())
    })();
    remove_db(&path);
    r
}

/// Other init failures carry their real code and a message too: a read-only
/// open of an empty file fails at init's first write with SQLITE_READONLY.
fn init_race_other_failure_reports_code_and_message() -> TResult {
    let path = tmp_db("romsg");
    let r = (|| -> TResult {
        // An empty file: a plain open creates it and writes nothing.
        close_v2(open_plain(&path, OPEN_RW)?);
        let o = open_cr(&path, OPEN_RO);
        close_v2(o.db);
        check!(
            o.rc == SQLITE_READONLY,
            "read-only open returned {} `{}`",
            o.rc,
            o.err
        );
        let want = format!(
            "{}cr-sqlite init: attempt to write a readonly database",
            AUTOEXT_PREFIX
        );
        check!(o.err == want, "message `{}`, want `{}`", o.err, want);
        Ok(())
    })();
    remove_db(&path);
    r
}

/// The completeness check and the init sequence read the recorded version
/// the same, 64-bit way. A version above 2^31 is a (future) version the
/// check accepts; a 32-bit read in the sequence truncated it to a negative
/// number and refused the file as "version -2147483648".
fn init_race_version_read_is_64_bit() -> TResult {
    let path = initialized("v64")?;
    let r = (|| -> TResult {
        let p = open_plain(&path, OPEN_RW)?;
        let w = exec(
            p,
            "UPDATE crsql_master SET value = 2147483648 WHERE key = 'crsqlite_version'",
        );
        close_v2(p);
        w?;
        let o = open_cr(&path, OPEN_RW);
        let v = if o.rc == SQLITE_OK {
            query_i64(
                o.db,
                "SELECT value FROM crsql_master WHERE key = 'crsqlite_version'",
            )
        } else {
            Ok(None)
        };
        close_v2(o.db);
        check!(o.rc == SQLITE_OK, "open failed: {} {}", o.rc, o.err);
        check!(
            v? == Some(2147483648),
            "the recorded version was rewritten by an open"
        );
        Ok(())
    })();
    remove_db(&path);
    r
}

// ------------------------------------------------- per-open budget probe ---
//
// A default-VFS shim whose xSleep counts the microseconds slept. Both
// cr-sqlite's init busy handler (sqlite3_sleep, which uses the default VFS)
// and SQLite's own busy_timeout callback (the connection's VFS, which is the
// default when the connection opened) sleep through it, so the count is the
// time an open spent waiting on locks -- whichever handler did the waiting.
// The shim can also run one "release" action once the count passes a mark:
// on one thread that is the only way to let an open get past one lock wait
// and reach the next.

use core::sync::atomic::{AtomicBool, AtomicI64};

static SLEPT_US: AtomicI64 = AtomicI64::new(0);
static RELEASE_AT_US: AtomicI64 = AtomicI64::new(-1);
static RELEASED: AtomicBool = AtomicBool::new(false);
static mut ORIG_VFS: *mut b::sqlite3_vfs = core::ptr::null_mut();
static mut RELEASE_DB: *mut b::sqlite3 = core::ptr::null_mut();
static mut SHIM_VFS: core::mem::MaybeUninit<b::sqlite3_vfs> = core::mem::MaybeUninit::uninit();

unsafe extern "C" fn shim_sleep(_vfs: *mut b::sqlite3_vfs, us: c_int) -> c_int {
    let orig = ORIG_VFS;
    let r = ((*orig).xSleep.unwrap())(orig, us);
    let total = SLEPT_US.fetch_add(us as i64, Ordering::Relaxed) + us as i64;
    let mark = RELEASE_AT_US.load(Ordering::Relaxed);
    if mark >= 0 && total >= mark {
        RELEASE_AT_US.store(-1, Ordering::Relaxed);
        // The lock holder swaps its write lock for a read lock: the waiting
        // open can now take RESERVED, but its COMMIT will wait again.
        let p = RELEASE_DB;
        if exec(p, "ROLLBACK").is_ok()
            && exec(p, "BEGIN; SELECT count(*) FROM sqlite_master;").is_ok()
        {
            RELEASED.store(true, Ordering::Relaxed);
        }
    }
    r
}

struct SleepShim;
impl SleepShim {
    fn install() -> SleepShim {
        unsafe {
            ORIG_VFS = b::sqlite3_vfs_find(core::ptr::null());
            let mut v = *ORIG_VFS;
            v.pNext = null_mut();
            v.zName = b"crsql_init_race_sleep\0".as_ptr() as *const c_char;
            v.xSleep = Some(shim_sleep);
            let shim = core::ptr::addr_of_mut!(SHIM_VFS) as *mut b::sqlite3_vfs;
            shim.write(v);
            b::sqlite3_vfs_register(shim, 1);
        }
        SLEPT_US.store(0, Ordering::Relaxed);
        RELEASE_AT_US.store(-1, Ordering::Relaxed);
        RELEASED.store(false, Ordering::Relaxed);
        SleepShim
    }
}
impl Drop for SleepShim {
    fn drop(&mut self) {
        unsafe {
            b::sqlite3_vfs_register(ORIG_VFS, 1);
            b::sqlite3_vfs_unregister(core::ptr::addr_of_mut!(SHIM_VFS) as *mut b::sqlite3_vfs);
            RELEASE_DB = null_mut();
        }
        RELEASE_AT_US.store(-1, Ordering::Relaxed);
    }
}

/// The **Init Busy Timeout** budget bounds the TOTAL lock waiting of one open,
/// not each wait. P holds BEGIN IMMEDIATE on a fresh file; budget 1000 ms.
/// After C has slept 500 ms at `BEGIN IMMEDIATE`, P swaps to a read lock, so C
/// gets RESERVED, runs the sequence, and waits again at `COMMIT`, which P's
/// read lock blocks. A per-open budget gives C only the ~500 ms left; a
/// per-wait timeout (`sqlite3_busy_timeout`) would give it a fresh 1000 ms.
/// Counts time slept, not wall time, so it is deterministic under valgrind.
fn init_race_budget_is_per_open() -> TResult {
    let path = tmp_db("budget");
    let r = (|| -> TResult {
        let _shim = SleepShim::install();
        let _b = Budget::set(1000);
        let p = open_plain(&path, OPEN_RW)?;
        exec(p, "BEGIN IMMEDIATE")?;
        unsafe { RELEASE_DB = p };
        SLEPT_US.store(0, Ordering::Relaxed);
        RELEASE_AT_US.store(500_000, Ordering::Relaxed);
        let o = open_cr(&path, OPEN_RW);
        let slept_ms = SLEPT_US.load(Ordering::Relaxed) / 1000;
        let released = RELEASED.load(Ordering::Relaxed);
        RELEASE_AT_US.store(-1, Ordering::Relaxed);
        close_v2(o.db);
        let _ = exec(p, "ROLLBACK");
        close_v2(p);
        check!(
            released,
            "the open never waited 500 ms at BEGIN IMMEDIATE (slept {} ms)",
            slept_ms
        );
        check!(
            o.rc != SQLITE_OK,
            "open succeeded although P's read lock blocked its COMMIT"
        );
        check!(
            slept_ms <= 1100,
            "the open slept {} ms in total: the 1000 ms budget is not per open",
            slept_ms
        );
        check!(
            slept_ms >= 900,
            "the open slept only {} ms: it did not use the rest of its budget at COMMIT",
            slept_ms
        );
        let n = crsql_objects(&path)?;
        check!(n == 0, "{} cr-sqlite objects left after a failed COMMIT", n);
        Ok(())
    })();
    remove_db(&path);
    r
}

// ----------------------------------------------------------------- runner ---

const TESTS: &[(&str, fn() -> TResult)] = &[
    ("init_race_step_count", init_race_step_count),
    ("init_race_heals_field_sample", init_race_heals_field_sample),
    (
        "init_race_heals_synthesized_residue",
        init_race_heals_synthesized_residue,
    ),
    ("init_race_heal_refuses_v0_12", init_race_heal_refuses_v0_12),
    (
        "init_race_heal_refuses_v0_13_v0_15",
        init_race_heal_refuses_v0_13_v0_15,
    ),
    (
        "init_race_heal_refuses_nonempty",
        init_race_heal_refuses_nonempty,
    ),
    (
        "init_race_heal_refuses_crsql_object",
        init_race_heal_refuses_crsql_object,
    ),
    ("init_race_heal_app_tables", init_race_heal_app_tables),
    (
        "init_race_atomic_at_each_step",
        init_race_atomic_at_each_step,
    ),
    (
        "init_race_error_paths_free_once",
        init_race_error_paths_free_once,
    ),
    (
        "init_race_late_failure_no_zombie",
        init_race_late_failure_no_zombie,
    ),
    (
        "init_race_loser_writes_nothing",
        init_race_loser_writes_nothing,
    ),
    ("init_race_commit_busy", init_race_commit_busy),
    ("init_race_budget_is_per_open", init_race_budget_is_per_open),
    (
        "init_race_reopen_writes_nothing",
        init_race_reopen_writes_nothing,
    ),
    ("init_race_config_busy_fails", init_race_config_busy_fails),
    (
        "init_race_config_broken_setting_closes",
        init_race_config_broken_setting_closes,
    ),
    (
        "init_race_heal_refuses_crsql_named_trigger",
        init_race_heal_refuses_crsql_named_trigger,
    ),
    (
        "init_race_busy_reports_code_and_message",
        init_race_busy_reports_code_and_message,
    ),
    (
        "init_race_other_failure_reports_code_and_message",
        init_race_other_failure_reports_code_and_message,
    ),
    (
        "init_race_version_read_is_64_bit",
        init_race_version_read_is_64_bit,
    ),
];

/// Runs every test (or `CRSQL_INIT_RACE_ONLY`), printing `ok`, `FAILED` or
/// `SKIPPED` for each and a summary line. A SKIPPED test passes the suite
/// but is never printed as ok: it could not run on this platform.
#[no_mangle]
pub extern "C" fn crsql_init_race_check() {
    let only = unsafe { getenv(cstr("CRSQL_INIT_RACE_ONLY").as_ptr()) };
    let only = if only.is_null() {
        None
    } else {
        unsafe { CStr::from_ptr(only) }
            .to_str()
            .ok()
            .map(|s| s.to_string())
    };
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    for (name, f) in TESTS {
        if let Some(o) = &only {
            if o != name {
                continue;
            }
        }
        match f() {
            Ok(()) => {
                passed += 1;
                println!("{} ... ok", name);
            }
            Err(e) if e.starts_with(SKIP_PREFIX) => {
                skipped += 1;
                println!("{} ... {}", name, e);
            }
            Err(e) => {
                failed += 1;
                println!("{} ... FAILED: {}", name, e);
            }
        }
    }
    println!(
        "init_race: {} passed, {} skipped, {} failed",
        passed, skipped, failed
    );
    if failed > 0 {
        panic!("{} init_race test(s) failed", failed);
    }
}

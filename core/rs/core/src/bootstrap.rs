use core::ffi::{c_char, c_int};

use crate::{consts, tableinfo::TableInfo};
use alloc::{ffi::CString, format};
use core::slice;
use sqlite::{sqlite3, Connection, Destructor, ResultCode};
use sqlite_nostd as sqlite;

fn uuid() -> [u8; 16] {
    let mut blob: [u8; 16] = [0; 16];
    sqlite::randomness(&mut blob);
    blob[6] = (blob[6] & 0x0f) + 0x40;
    blob[8] = (blob[8] & 0x3f) + 0x80;
    blob
}

#[no_mangle]
pub extern "C" fn crsql_init_site_id(db: *mut sqlite3, ret: *mut u8) -> c_int {
    let buffer: &mut [u8] = unsafe { slice::from_raw_parts_mut(ret, 16) };
    match init_site_id(db) {
        Ok(site_id) => {
            buffer.copy_from_slice(&site_id);
            ResultCode::OK as c_int
        }
        // The real code, so a lock failure reaches the host as SQLITE_BUSY
        // (**Init Failure Code and Message**).
        Err(code) => code as c_int,
    }
}

fn insert_site_id(db: *mut sqlite3) -> Result<[u8; 16], ResultCode> {
    let stmt = db.prepare_v2(&format!(
        "INSERT INTO \"{tbl}\" (site_id, ordinal) VALUES (?, 0)",
        tbl = consts::TBL_SITE_ID
    ))?;

    let site_id = uuid();
    stmt.bind_blob(1, &site_id, Destructor::STATIC)?;
    stmt.step()?;

    Ok(site_id)
}

fn create_site_id_and_site_id_table(db: *mut sqlite3) -> Result<[u8; 16], ResultCode> {
    db.exec_safe(&format!(
        "CREATE TABLE \"{tbl}\" (site_id BLOB NOT NULL, ordinal INTEGER PRIMARY KEY);
        CREATE UNIQUE INDEX {tbl}_site_id ON \"{tbl}\" (site_id);",
        tbl = consts::TBL_SITE_ID
    ))?;

    insert_site_id(db)
}

pub fn create_site_id_triggers(db: *mut sqlite3) -> Result<ResultCode, ResultCode> {
    db.exec_safe(&format!(
        "CREATE TRIGGER IF NOT EXISTS {tbl}_insert_trig AFTER INSERT ON \"{tbl}\"
        WHEN NEW.ordinal != 0
        BEGIN
          VALUES (crsql_update_site_id(NEW.site_id, NEW.ordinal));
        END;
        CREATE TRIGGER IF NOT EXISTS {tbl}_update_trig AFTER UPDATE ON \"{tbl}\"
        WHEN NEW.ordinal != 0
        BEGIN
          VALUES (crsql_update_site_id(NEW.site_id, NEW.ordinal));
        END;
        CREATE TRIGGER IF NOT EXISTS {tbl}_delete_trig AFTER DELETE ON \"{tbl}\"
        WHEN OLD.ordinal != 0
        BEGIN
          VALUES (crsql_update_site_id(OLD.site_id, -1));
        END;",
        tbl = consts::TBL_SITE_ID
    ))
}

#[no_mangle]
pub extern "C" fn crsql_init_peer_tracking_table(db: *mut sqlite3) -> c_int {
    match db.exec_safe("CREATE TABLE IF NOT EXISTS crsql_tracked_peers (\"site_id\" BLOB NOT NULL, \"version\" INTEGER NOT NULL, \"seq\" INTEGER DEFAULT 0, \"tag\" INTEGER, \"event\" INTEGER, PRIMARY KEY (\"site_id\", \"tag\", \"event\")) STRICT;") {
      Ok(_) => ResultCode::OK as c_int,
      Err(code) => code as c_int
    }
}

// #[no_mangle]
pub extern "C" fn crsql_init_db_versions_table(db: *mut sqlite3) -> c_int {
    match db.exec_safe("CREATE TABLE IF NOT EXISTS crsql_db_versions (\"site_id\" BLOB NOT NULL PRIMARY KEY, \"db_version\" INTEGER NOT NULL) STRICT;") {
      Ok(_) => ResultCode::OK as c_int,
      Err(code) => code as c_int
    }
}

fn has_table(db: *mut sqlite3, table_name: &str) -> Result<bool, ResultCode> {
    let stmt =
        db.prepare_v2("SELECT 1 FROM sqlite_master WHERE type = 'table' AND tbl_name = ?")?;
    stmt.bind_text(1, table_name, Destructor::STATIC)?;
    let tbl_exists_result = stmt.step()?;
    Ok(tbl_exists_result == ResultCode::ROW)
}

/**
 * Loads the siteId into memory. If a site id
 * cannot be found for the given database one is created
 * and saved to the site id table.
 */
fn init_site_id(db: *mut sqlite3) -> Result<[u8; 16], ResultCode> {
    if !has_table(db, consts::TBL_SITE_ID)? {
        return create_site_id_and_site_id_table(db);
    }

    let stmt = db.prepare_v2(&format!(
        "SELECT site_id FROM \"{}\" WHERE ordinal = 0",
        consts::TBL_SITE_ID
    ))?;
    let result_code = stmt.step()?;

    let ret = if result_code == ResultCode::DONE {
        insert_site_id(db)?
    } else {
        let site_id_from_table = stmt.column_blob(0)?;
        site_id_from_table.try_into()?
    };

    Ok(ret)
}

fn crsql_create_schema_table_if_not_exists(db: *mut sqlite3) -> Result<ResultCode, ResultCode> {
    if crate::init_fault::fail_at(crate::init_fault::InitStep::SchemaTable) {
        return Err(ResultCode::BUSY);
    }
    db.exec_safe("SAVEPOINT crsql_create_schema_table;")?;

    match db.exec_safe(&format!(
        "CREATE TABLE IF NOT EXISTS \"{}\" (\"key\" TEXT PRIMARY KEY, \"value\" ANY);",
        consts::TBL_SCHEMA
    )) {
        Ok(_) => db.exec_safe("RELEASE crsql_create_schema_table;"),
        Err(code) => {
            // Roll back to OUR savepoint only. A bare ROLLBACK would abort a
            // transaction that belongs to the host (**Init Host Rollback**).
            let _ = db.exec_safe(
                "ROLLBACK TO crsql_create_schema_table; RELEASE crsql_create_schema_table;",
            );
            Err(code)
        }
    }
}

#[no_mangle]
pub extern "C" fn crsql_maybe_update_db(db: *mut sqlite3, err_msg: *mut *mut c_char) -> c_int {
    // No schema table? First time this DB has been opened with this extension.
    // Every failure returns its real code, so a lock failure reaches the
    // host as SQLITE_BUSY (**Init Failure Code and Message**).
    match has_table(db, consts::TBL_SCHEMA) {
        Ok(has_schema_table) => {
            if let Err(code) = crsql_create_schema_table_if_not_exists(db) {
                return code as c_int;
            }
            let r = db.exec_safe("SAVEPOINT crsql_maybe_update_db;");
            if let Err(code) = r {
                return code as c_int;
            }
            match maybe_update_db_inner(db, has_schema_table == false, err_msg) {
                Ok(_) => {
                    let _ = db.exec_safe("RELEASE crsql_maybe_update_db;");
                    ResultCode::OK as c_int
                }
                Err(code) => {
                    // Roll back to OUR savepoint only (**Init Host
                    // Rollback**); the caller's init transaction rolls back
                    // the rest.
                    let _ = db.exec_safe(
                        "ROLLBACK TO crsql_maybe_update_db; RELEASE crsql_maybe_update_db;",
                    );
                    code as c_int
                }
            }
        }
        Err(code) => code as c_int,
    }
}

fn maybe_update_db_inner(
    db: *mut sqlite3,
    is_blank_slate: bool,
    err_msg: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    // 64-bit, matching the **Init Completeness Check**'s `CAST(value AS INTEGER)`:
    // a 32-bit read truncated a version above 2^31 and refused the file.
    let mut recorded_version: i64 = 0;
    let mut is_blank_slate = is_blank_slate;

    // Completely new DBs need no migrations.
    // We can set them to the current version.
    if is_blank_slate {
        recorded_version = consts::CRSQLITE_VERSION as i64;
    } else {
        let stmt =
            db.prepare_v2("SELECT value FROM crsql_master WHERE key = 'crsqlite_version'")?;
        let step_result = stmt.step()?;
        if step_result == ResultCode::ROW {
            recorded_version = stmt.column_int64(0);
        } else if is_init_residue(db)? {
            // **Init Residue Heal**: a Half-Initialized Database holds no
            // cr-sqlite state at all, so initialize it as if it were new.
            is_blank_slate = true;
            recorded_version = consts::CRSQLITE_VERSION as i64;
        }
    }
    // libc_print::libc_println!("recorded_version: {}", recorded_version);

    if recorded_version < consts::CRSQLITE_VERSION_0_17_0 as i64 && !is_blank_slate {
        let cstring = CString::new(format!("Opening a db created with cr-sqlite version {} is not supported. Upcoming release 0.17.0 is a breaking change.", recorded_version))?;
        unsafe {
            (*err_msg) = cstring.into_raw();
            return Err(ResultCode::ERROR);
        }
    }

    // write the db version if we migrated to a new one or we are a blank slate db
    if recorded_version < consts::CRSQLITE_VERSION as i64 || is_blank_slate {
        if crate::init_fault::fail_at(crate::init_fault::InitStep::VersionWrite) {
            return Err(ResultCode::BUSY);
        }
        let stmt =
            db.prepare_v2("INSERT OR REPLACE INTO crsql_master VALUES ('crsqlite_version', ?)")?;
        stmt.bind_int(1, consts::CRSQLITE_VERSION)?;
        stmt.step()?;
    }

    Ok(ResultCode::OK)
}

/// **Init Residue Heal** predicate (docs/CRSQLITE_INIT_RACE_FIX.md). True when
/// the file is a Half-Initialized Database that holds NO cr-sqlite state:
///   1. `crsql_master` exists and has zero rows (not merely no version row:
///      `config.*` rows mean the file was opened successfully once);
///   2. `crsql_tracked_peers` and `crsql_db_versions`, if they exist, are
///      empty;
///   3. no other table, index, trigger or view whose name contains `crsql`
///      exists, other than those three tables and their `sqlite_autoindex_*`
///      indexes. The exemption is for objects of type `table` with those
///      names only: a trigger named `crsql_master` still blocks the heal.
/// Rule 3 matches with `instr(lower(name), 'crsql')`, NOT `LIKE`: LIKE's case
/// sensitivity follows `PRAGMA case_sensitive_like`, which a host may set
/// before a `load_extension`. Keep it name-based and broad: every cr-sqlite
/// version that ever finished init left a site-id table (under one of its
/// names), and every CRR leaves `__crsql_*` objects, so a file this admits has
/// nothing to lose. Application tables are allowed (scope ruled broad).
/// Called only when `crsql_master` exists without a `crsqlite_version` row,
/// and only inside init's transaction: under `BEGIN IMMEDIATE` (the write
/// lock held), or -- for `load_extension` inside a host transaction -- under
/// `SAVEPOINT crsql_init`, which holds no write lock. That is safe: a
/// competing writer makes the heal's first write fail, and the savepoint
/// rolls back cleanly.
fn is_init_residue(db: *mut sqlite3) -> Result<bool, ResultCode> {
    let stmt = db.prepare_v2("SELECT count(*) FROM crsql_master")?;
    if stmt.step()? != ResultCode::ROW || stmt.column_int64(0) != 0 {
        return Ok(false);
    }
    for tbl in ["crsql_tracked_peers", "crsql_db_versions"] {
        if has_table(db, tbl)? {
            let stmt = db.prepare_v2(&format!("SELECT count(*) FROM \"{}\"", tbl))?;
            if stmt.step()? != ResultCode::ROW || stmt.column_int64(0) != 0 {
                return Ok(false);
            }
        }
    }
    let stmt = db.prepare_v2(
        "SELECT count(*) FROM sqlite_master
         WHERE type IN ('table', 'index', 'trigger', 'view')
           AND instr(lower(name), 'crsql') > 0
           AND NOT (type = 'table'
                    AND name IN ('crsql_master', 'crsql_tracked_peers', 'crsql_db_versions'))
           AND NOT (type = 'index'
                    AND tbl_name IN ('crsql_master', 'crsql_tracked_peers', 'crsql_db_versions')
                    AND substr(name, 1, 17) = 'sqlite_autoindex_')",
    )?;
    if stmt.step()? != ResultCode::ROW {
        return Err(ResultCode::ERROR);
    }
    Ok(stmt.column_int64(0) == 0)
}

/// **Init Completeness Check** (read-only). True when today's init sequence
/// would write nothing: the four bookkeeping tables exist, `crsql_master`
/// holds `crsqlite_version` >= `CRSQLITE_VERSION`, `crsql_site_id` holds an
/// ordinal-0 row, and the three site-id triggers exist. Any step result other
/// than ROW/DONE is an error that fails the open -- never "complete" and never
/// "incomplete". Each statement is finalized before this returns, so it never
/// leaves a SHARED lock behind for the slow path to wait on.
///
/// Too strict, and every healthy open takes the write lock; too loose, and a
/// file that needs a write skips it. Pinned by `init_race_reopen_writes_nothing`
/// and `init_race_error_paths_free_once` (an initialized file must not reach
/// `BEGIN IMMEDIATE`).
pub fn is_init_complete(db: *mut sqlite3) -> Result<bool, ResultCode> {
    let catalog_ok = {
        let stmt = db.prepare_v2(
            "SELECT
               (SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN
                  ('crsql_tracked_peers', 'crsql_db_versions', 'crsql_master', 'crsql_site_id')) = 4
               AND (SELECT count(*) FROM sqlite_master WHERE type = 'trigger'
                  AND tbl_name = 'crsql_site_id' AND name IN
                  ('crsql_site_id_insert_trig', 'crsql_site_id_update_trig', 'crsql_site_id_delete_trig')) = 3",
        )?;
        if stmt.step()? != ResultCode::ROW {
            return Err(ResultCode::ERROR);
        }
        stmt.column_int(0) == 1
    };
    if !catalog_ok {
        return Ok(false);
    }
    let stmt = db.prepare_v2(
        "SELECT
           coalesce((SELECT CAST(value AS INTEGER) >= ?1 FROM crsql_master
                     WHERE key = 'crsqlite_version'), 0)
           AND EXISTS (SELECT 1 FROM crsql_site_id WHERE ordinal = 0)",
    )?;
    stmt.bind_int(1, consts::CRSQLITE_VERSION)?;
    if stmt.step()? != ResultCode::ROW {
        return Err(ResultCode::ERROR);
    }
    Ok(stmt.column_int(0) == 1)
}

/// The transaction of the **Atomic Init Transaction**: a read transaction
/// (`BEGIN DEFERRED`), a write transaction (`BEGIN IMMEDIATE`), or -- when the
/// host is already inside a transaction (only under `load_extension`) -- a
/// `SAVEPOINT crsql_init`. Dropping it without `commit()` rolls back, so every
/// early return of init rolls back automatically; the savepoint form rolls
/// back only to its own savepoint, never the host's transaction.
pub struct InitTxn {
    db: *mut sqlite3,
    savepoint: bool,
    open: bool,
}

impl InitTxn {
    pub fn begin_read(db: *mut sqlite3) -> Result<InitTxn, ResultCode> {
        db.exec_safe("BEGIN DEFERRED")?;
        Ok(InitTxn {
            db,
            savepoint: false,
            open: true,
        })
    }

    /// Takes RESERVED before reading, so the slow path's reads and writes see
    /// one state. A deferred transaction upgrading from SHARED can be refused
    /// with SQLITE_BUSY without the busy handler ever being invoked; this one
    /// waits under the **Init Busy Timeout**.
    pub fn begin_immediate(db: *mut sqlite3) -> Result<InitTxn, ResultCode> {
        db.exec_safe("BEGIN IMMEDIATE")?;
        Ok(InitTxn {
            db,
            savepoint: false,
            open: true,
        })
    }

    pub fn savepoint(db: *mut sqlite3) -> Result<InitTxn, ResultCode> {
        db.exec_safe("SAVEPOINT crsql_init")?;
        Ok(InitTxn {
            db,
            savepoint: true,
            open: true,
        })
    }

    /// COMMIT (or RELEASE). On failure -- e.g. SQLITE_BUSY once the busy
    /// budget is spent -- the guard is dropped still open and rolls back.
    pub fn commit(mut self) -> Result<(), ResultCode> {
        let sql = if self.savepoint {
            "RELEASE crsql_init"
        } else {
            "COMMIT"
        };
        self.db.exec_safe(sql)?;
        self.open = false;
        Ok(())
    }
}

impl Drop for InitTxn {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        let sql = if self.savepoint {
            "ROLLBACK TO crsql_init; RELEASE crsql_init"
        } else {
            "ROLLBACK"
        };
        // SQLite may already have rolled back (for example after an I/O
        // error); then this fails harmlessly.
        let _ = self.db.exec_safe(sql);
    }
}

/**
 * The clock table holds the versions for each column of a given row.
 *
 * These version are set to the dbversion at the time of the write to the
 * column.
 *
 * The dbversion is updated on transaction commit.
 * This allows us to find all columns written in the same transaction
 * albeit with caveats.
 *
 * The caveats being that two partiall overlapping transactions will
 * clobber the full transaction picture given we only keep latest
 * state and not a full causal history.
 *
 * @param tableInfo
 */
pub fn create_clock_table(
    db: *mut sqlite3,
    table_info: &TableInfo,
    _err: *mut *mut c_char,
) -> Result<ResultCode, ResultCode> {
    let pk_list = crate::util::as_identifier_list(&table_info.pks, None)?;
    let table_name = &table_info.tbl_name;

    db.exec_safe(&format!(
        "CREATE TABLE IF NOT EXISTS \"{table_name}__crsql_clock\" (
      key INTEGER NOT NULL,
      col_name TEXT NOT NULL,
      col_version INTEGER NOT NULL,
      db_version INTEGER NOT NULL,
      site_id INTEGER NOT NULL DEFAULT 0,
      seq INTEGER NOT NULL,
      ts TEXT NOT NULL DEFAULT '0',

      PRIMARY KEY (key, col_name)
    ) WITHOUT ROWID, STRICT",
        table_name = crate::util::escape_ident(table_name),
    ))?;

    db.exec_safe(
        &format!(
        "CREATE INDEX IF NOT EXISTS \"{table_name}__crsql_clock_dbv_idx\" ON \"{table_name}__crsql_clock\" (\"site_id\", \"db_version\")",
        table_name = crate::util::escape_ident(table_name),
        ))?;
    db.exec_safe(
        &format!(
        "CREATE TABLE IF NOT EXISTS \"{table_name}__crsql_pks\" (__crsql_key INTEGER PRIMARY KEY, {pk_list})",
        table_name = table_name,
        pk_list = pk_list,
        )
    )?;
    db.exec_safe(
        &format!(
        "CREATE UNIQUE INDEX IF NOT EXISTS \"{table_name}__crsql_pks_pks\" ON \"{table_name}__crsql_pks\" ({pk_list})",
        table_name = table_name,
        pk_list = pk_list
        )
    )
}

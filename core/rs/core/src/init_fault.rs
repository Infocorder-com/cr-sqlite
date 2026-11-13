//! Fault injection points for extension init (**Atomic Init Transaction**,
//! docs/CRSQLITE_INIT_RACE_FIX.md, "Fault injection").
//!
//! Each `InitStep` names one fallible point of `sqlite3_crsqlite_init`, in the
//! order init runs. The values are stable: they are named points, not
//! positions, so inserting a step later must append a new value rather than
//! renumber. The tests loop over `1..=InitStep::COUNT`, and assert `COUNT`, so
//! adding a step without a value fails a test.
//!
//! Outside the `test` feature `fail_at` is a constant `false`, so release
//! builds compile every injection point away.

#[repr(i32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InitStep {
    /// The early registrations (`crsql_set_debug` ... `clset`), as one point.
    EarlyRegistrations = 1,
    /// `crsql_internal_sync_bit` registration (destructor `sqlite3_free`).
    SyncBitRegistration = 2,
    /// The fast-path read transaction (`BEGIN DEFERRED` and the check).
    ReadTxn = 3,
    /// The slow path's `BEGIN IMMEDIATE`.
    BeginImmediate = 4,
    /// `crsql_init_peer_tracking_table`.
    PeerTrackingTable = 5,
    /// `crsql_init_db_versions_table`.
    DbVersionsTable = 6,
    /// `crsql_create_schema_table_if_not_exists`.
    SchemaTable = 7,
    /// The `crsqlite_version` write in `maybe_update_db_inner`.
    VersionWrite = 8,
    /// `crsql_newExtData` (injected in C, after allocation).
    NewExtData = 9,
    /// `crsql_update_site_id` registration (no destructor).
    UpdateSiteIdRegistration = 10,
    /// `crsql_init_site_id`.
    InitSiteId = 11,
    /// `crsql_initSiteIdExt` (the call runs; its result is replaced).
    InitSiteIdExt = 12,
    /// `create_site_id_triggers`.
    SiteIdTriggers = 13,
    /// The `COMMIT` that ends init's transaction.
    Commit = 14,
    /// The post-commit registrations without a destructor, as one point.
    PostCommitRegistrations = 15,
    /// The `crsql_changes` module (in `sqlite3_crsqlite_init`, C).
    ChangesModule = 16,
    /// The ownership handoff: `crsql_db_version` with
    /// `x_free_connection_ext_data`.
    OwnershipHandoff = 17,
}

impl InitStep {
    pub const COUNT: i32 = 17;
}

/// True when the test hook asks init to fail at `step`.
#[inline(always)]
pub fn fail_at(step: InitStep) -> bool {
    #[cfg(feature = "test")]
    {
        crate::test_exports::init_fail_step() == step as i32
    }
    #[cfg(not(feature = "test"))]
    {
        let _ = step;
        false
    }
}

/// `nArg` for a registration at `step`: SQLite rejects `-2` with
/// `SQLITE_MISUSE` through its real failure path, which runs the destructor
/// because the registration took no reference -- exactly what a real failure
/// does. Skipping the call instead would fake a leak or hide a double free.
#[inline(always)]
pub fn n_arg(step: InitStep, n_arg: i32) -> i32 {
    if fail_at(step) {
        -2
    } else {
        n_arg
    }
}

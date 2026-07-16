// The sha of the commit that this version of crsqlite was built from.
pub const SHA: &'static str = core::env!("CRSQLITE_COMMIT_SHA");

// A build-provenance marker for the Infocorder fork, exposed as `crsql_build_id()`.
// Its mere presence proves the loaded NIF is this fork's build: upstream cr-sqlite
// has no `crsql_build_id()` function, so the host can tell the statically-linked
// fork build (which carries the E1 file-descriptor-release-on-close fix) apart from
// an upstream fallback that answers `crsql_db_version()` / `crsql_sha()` identically.
// The `e1-fd-close` tag identifies the feature set; the sha identifies the commit.
// See docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md.
pub const BUILD_ID: &'static str =
    concat!("e1-fd-close ", core::env!("CRSQLITE_COMMIT_SHA"));

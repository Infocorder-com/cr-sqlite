// The sha of the commit that this version of crsqlite was built from.
pub const SHA: &'static str = core::env!("CRSQLITE_COMMIT_SHA");

// A build-provenance marker for the Infocorder fork, exposed as `crsql_build_id()`.
// Its mere presence proves the loaded NIF is this fork's build: upstream cr-sqlite
// has no `crsql_build_id()` function, so the host can tell the statically-linked
// fork build (which carries the E1 file-descriptor-release-on-close fix) apart from
// an upstream fallback that answers `crsql_db_version()` / `crsql_sha()` identically.
// The tag identifies the feature set; the sha identifies the commit.
//
// The tag is a `+`-joined, cumulative capability list, so BUILD_ID always stays two
// space-separated fields (tag, sha) and a host gates on its own token with a substring
// match -- adding a capability never breaks an existing gate.
//   e1-fd-close      file descriptors released on connection close
//                    (docs/OPTION_B_CRSQLITE_CLOSE_FD_FIX.md)
//   e2-tbl-pushdown  `crsql_changes WHERE "table" = ?` is pushed into the clock union,
//                    bounding a table-scoped read by that table's own clock
//                    (docs/CRSQL_CHANGES_TABLE_FILTER_PUSHDOWN.md, Release A)
pub const BUILD_ID: &'static str = concat!(
    "e1-fd-close+e2-tbl-pushdown ",
    core::env!("CRSQLITE_COMMIT_SHA")
);

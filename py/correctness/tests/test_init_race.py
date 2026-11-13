# init_race tests that need `load_extension`, which the C test binary omits
# (docs/CRSQLITE_INIT_RACE_FIX.md, "Deterministic tests").
#
# A failed `load_extension` makes SQLite unload the library at once, so any
# test that fails a load on purpose first loads the extension on a KEEPER
# connection: dlopen reference-counts the library, so it stays mapped until
# the keeper closes, which happens only after the failing connection closed.
# test_init_race_failed_load_close is the exception: it exercises the
# **Failed Load Unmapped Callback** fix itself, with no keeper, in a
# subprocess so a crash cannot take pytest down.
import pathlib
import shutil
import sqlite3
import subprocess
import sys

import pytest

from crsql_correctness import connect, close

EXTENSION = str(pathlib.Path("../../core/dist/crsqlite").resolve())
PRIOR_V0_12 = pathlib.Path("./prior-dbs/v0.12.0.prior-db")

# Same DDL as core/rs/core/src/bootstrap.rs: a Half-Initialized Database.
RESIDUE_DDL = """
CREATE TABLE IF NOT EXISTS crsql_tracked_peers ("site_id" BLOB NOT NULL, "version" INTEGER NOT NULL, "seq" INTEGER DEFAULT 0, "tag" INTEGER, "event" INTEGER, PRIMARY KEY ("site_id", "tag", "event")) STRICT;
CREATE TABLE IF NOT EXISTS crsql_db_versions ("site_id" BLOB NOT NULL PRIMARY KEY, "db_version" INTEGER NOT NULL) STRICT;
CREATE TABLE IF NOT EXISTS "crsql_master" ("key" TEXT PRIMARY KEY, "value" ANY);
"""


@pytest.fixture
def keeper():
    k = connect(":memory:")
    yield k
    close(k)


def test_init_race_host_txn_preserved(tmp_path, keeper):
    path = tmp_path / "v012.db"
    shutil.copyfile(PRIOR_V0_12, path)
    c = sqlite3.connect(path, isolation_level=None)
    try:
        c.execute("CREATE TABLE host_t (a)")
        c.execute("BEGIN")
        c.execute("INSERT INTO host_t VALUES (1)")
        c.enable_load_extension(True)
        with pytest.raises(sqlite3.OperationalError, match="version 0 is not supported"):
            c.load_extension(EXTENSION)
        # A failing init must roll back to its own savepoint, never the host's
        # transaction (**Init Host Rollback**).
        assert c.in_transaction
        assert c.execute("SELECT count(*) FROM host_t").fetchone()[0] == 1
        c.execute("COMMIT")
    finally:
        # Close before the keeper releases the library.
        c.close()


def test_init_race_heal_case_sensitive_like(tmp_path, keeper):
    path = tmp_path / "residue.db"
    p = sqlite3.connect(path)
    p.executescript(RESIDUE_DDL + "CREATE TABLE Foo__CRSQL_Clock (a);")
    p.close()
    c = sqlite3.connect(path)
    try:
        c.execute("PRAGMA case_sensitive_like = ON")
        c.enable_load_extension(True)
        # A cr-sqlite object named in mixed case must block the heal even when
        # the host made LIKE case-sensitive (rule 3 matches with instr/lower).
        with pytest.raises(sqlite3.OperationalError, match="version 0 is not supported"):
            c.load_extension(EXTENSION)
    finally:
        # Close before the keeper releases the library.
        c.close()


FAILED_LOAD_CLOSE = """
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.enable_load_extension(True)
try:
    c.load_extension(sys.argv[2])
    sys.exit(2)  # the load was expected to fail
except sqlite3.OperationalError:
    pass
c.close()
sys.exit(0)
"""


def test_init_race_failed_load_close(tmp_path):
    path = tmp_path / "v012.db"
    shutil.copyfile(PRIOR_V0_12, path)
    # No keeper: closing must not call into the unloaded library.
    r = subprocess.run(
        [sys.executable, "-I", "-c", FAILED_LOAD_CLOSE, str(path), EXTENSION],
        timeout=60,
    )
    assert r.returncode == 0, f"subprocess exited {r.returncode}"


# What a load_extension host sees when init runs out of lock budget. SQLite's
# sqlite3_load_extension always returns SQLITE_ERROR for a failed entry point
# (whatever code the entry point returned) and prefixes the entry point's
# message, so the message is the host's only signal; it must name the lock.
LOCKED_LOAD_MSG = "error during initialization: cr-sqlite init: database is locked"


@pytest.mark.parametrize("host_txn", [False, True])
def test_init_race_busy_load_message(tmp_path, keeper, host_txn):
    path = tmp_path / "fresh.db"
    holder = sqlite3.connect(path, isolation_level=None)
    holder.execute("BEGIN IMMEDIATE")
    # timeout=0: no host busy handler, so the lock fails the load at once.
    c = sqlite3.connect(path, isolation_level=None, timeout=0)
    try:
        if host_txn:
            c.execute("BEGIN")
            c.execute("SELECT count(*) FROM sqlite_master").fetchall()
        c.enable_load_extension(True)
        with pytest.raises(sqlite3.OperationalError) as e:
            c.load_extension(EXTENSION)
        assert str(e.value) == LOCKED_LOAD_MSG
        # A failed init leaves the host's transaction alone.
        assert c.in_transaction == host_txn
    finally:
        # Close before the keeper releases the library.
        c.close()
        holder.execute("ROLLBACK")
        holder.close()

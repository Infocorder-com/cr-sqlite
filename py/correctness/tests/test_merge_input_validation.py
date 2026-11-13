# Validation of a peer's change before it is merged
# (docs/CRSQLITE_INIT_RACE_FIX.md, "Merge Input Validation").
#
# Every row inserted into crsql_changes comes from another node, so each of
# these is something a buggy or hostile peer can send:
#
# - Packed PK Arity Check: a packed primary key with fewer or more columns
#   than the table's primary key. Fewer was accepted: the value landed in a
#   primary-key column, the pks table got a NULL key, and crsql_changes then
#   emitted that NULL key to every peer.
# - Merge Counter Bound: a col_version, db_version, causal length or seq that
#   is negative or above 2^53. col_version = i64::MAX was accepted, and the
#   next local update of that cell failed, because a local write stores
#   col_version + 1.
# - Error Message NUL Escape: an error message that quotes a peer's string
#   containing a NUL byte. Building the C string failed and the merge
#   reported SQLITE_NOMEM instead of SQLITE_ERROR.
import sqlite3

import pytest

from crsql_correctness import connect, close

SITE = b"\x01" * 16
I64_MAX = (1 << 63) - 1
I64_MIN = -(1 << 63)
BOUND = 1 << 53
TABLES = ("t", "u")


def make_db():
    c = connect(":memory:")
    c.execute("CREATE TABLE t (id PRIMARY KEY NOT NULL, v)")
    c.execute("SELECT crsql_as_crr('t')")
    c.execute("CREATE TABLE u (a NOT NULL, b NOT NULL, v, PRIMARY KEY (a, b))")
    c.execute("SELECT crsql_as_crr('u')")
    c.execute("INSERT INTO t VALUES (1, 'one')")
    c.execute("INSERT INTO u VALUES (1, 2, 'one-two')")
    c.commit()
    return c


def snapshot(c):
    out = {}
    for t in TABLES:
        for suffix in ("", "__crsql_pks", "__crsql_clock"):
            name = t + suffix
            out[name] = sorted(
                c.execute("SELECT * FROM \"" + name + "\"").fetchall(),
                key=repr)
    out["changes"] = sorted(
        c.execute("SELECT * FROM crsql_changes").fetchall(), key=repr)
    out["db_versions"] = sorted(
        c.execute("SELECT * FROM crsql_db_versions").fetchall(), key=repr)
    return out


def merge(c, tbl, pk, cid="v", val="X", col_version=1, db_version=1,
          site=SITE, cl=1, seq=0, ts="0"):
    c.execute(
        "INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (tbl, pk, cid, val, col_version, db_version, site, cl, seq, ts))


def pack(*vals):
    if not vals:
        return b"\x00"
    c = connect(":memory:")
    try:
        return c.execute(
            "SELECT crsql_pack_columns(" + ", ".join("?" * len(vals)) + ")",
            vals).fetchone()[0]
    finally:
        close(c)


# --- Packed PK Arity Check -------------------------------------------------

# (table, packed pk) where the arity does not match the table's primary key.
WRONG_ARITY = [
    pytest.param("t", (), id="t-zero"),
    pytest.param("t", (7, 8), id="t-more"),
    pytest.param("u", (), id="u-zero"),
    pytest.param("u", (7,), id="u-fewer"),
    pytest.param("u", (7, 8, 9), id="u-more"),
]

# (cid, cl): a column write, an insert sentinel (row creation), a delete
# (the -1 sentinel with an even causal length) and a resurrect (odd cl > 1).
KINDS = [
    pytest.param("v", 1, id="column"),
    pytest.param("-1", 1, id="sentinel"),
    pytest.param("-1", 2, id="delete"),
    pytest.param("v", 3, id="resurrect"),
]


@pytest.mark.parametrize("cid,cl", KINDS)
@pytest.mark.parametrize("tbl,pk_vals", WRONG_ARITY)
def test_wrong_pk_arity_is_rejected(tbl, pk_vals, cid, cl):
    c = make_db()
    try:
        before = snapshot(c)
        with pytest.raises(sqlite3.OperationalError) as e:
            merge(c, tbl, pack(*pk_vals), cid=cid, cl=cl)
        assert "primary key" in str(e.value), str(e.value)
        assert e.value.sqlite_errorcode == sqlite3.SQLITE_ERROR
        c.commit()
        # Nothing was written, and crsql_changes emits no NULL key.
        assert snapshot(c) == before
        for (pk,) in c.execute("SELECT pk FROM crsql_changes"):
            vals = c.execute(
                "SELECT cell FROM crsql_unpack_columns(?)", (pk,)).fetchall()
            assert all(v is not None for (v,) in vals), vals
    finally:
        close(c)


def test_wrong_pk_arity_matches_existing_row_key_too():
    # A 1-column key whose value matches the first column of u's existing
    # row (1, 2) must not reach that row.
    c = make_db()
    try:
        before = snapshot(c)
        with pytest.raises(sqlite3.OperationalError):
            merge(c, "u", pack(1), cl=2, cid="-1")
        assert snapshot(c) == before
        assert c.execute("SELECT * FROM u").fetchall() == [(1, 2, "one-two")]
    finally:
        close(c)


def sync(src, dst):
    changes = src.execute("SELECT * FROM crsql_changes").fetchall()
    for ch in changes:
        dst.execute(
            "INSERT INTO crsql_changes VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            ch)
    dst.commit()


def test_valid_merges_still_work():
    a = make_db()
    b = connect(":memory:")
    try:
        b.execute("CREATE TABLE t (id PRIMARY KEY NOT NULL, v)")
        b.execute("SELECT crsql_as_crr('t')")
        b.execute(
            "CREATE TABLE u (a NOT NULL, b NOT NULL, v, PRIMARY KEY (a, b))")
        b.execute("SELECT crsql_as_crr('u')")
        b.commit()

        a.execute("INSERT INTO t VALUES (2, 'two'), (3, 'three')")
        a.execute("INSERT INTO u VALUES (3, 4, 'three-four')")
        a.execute("UPDATE t SET v = 'ONE' WHERE id = 1")
        a.commit()
        sync(a, b)
        for q in ("SELECT * FROM t ORDER BY id", "SELECT * FROM u ORDER BY a"):
            assert b.execute(q).fetchall() == a.execute(q).fetchall()

        # Deletes, on a 1- and a 2-column key.
        a.execute("DELETE FROM t WHERE id = 2")
        a.execute("DELETE FROM u WHERE a = 1 AND b = 2")
        a.commit()
        sync(a, b)
        assert b.execute("SELECT id FROM t ORDER BY id").fetchall() == [
            (1,), (3,)]
        assert b.execute("SELECT a, b FROM u").fetchall() == [(3, 4)]

        # Resurrect after the delete.
        a.execute("INSERT INTO t VALUES (2, 'again')")
        a.execute("INSERT INTO u VALUES (1, 2, 'back')")
        a.commit()
        sync(a, b)
        for q in ("SELECT * FROM t ORDER BY id", "SELECT * FROM u ORDER BY a"):
            assert b.execute(q).fetchall() == a.execute(q).fetchall()
    finally:
        close(a)
        close(b)


# --- Merge Counter Bound ---------------------------------------------------

COUNTERS = ["col_version", "db_version", "cl", "seq"]
OUT_OF_RANGE = [-1, BOUND + 1, I64_MAX, I64_MIN]
IN_RANGE = [0, 1, BOUND]


@pytest.mark.parametrize("value", OUT_OF_RANGE)
@pytest.mark.parametrize("counter", COUNTERS)
def test_out_of_range_counter_is_rejected(counter, value):
    c = make_db()
    try:
        before = snapshot(c)
        with pytest.raises(sqlite3.OperationalError) as e:
            merge(c, "t", pack(5), **{counter: value})
        assert "out of range" in str(e.value), str(e.value)
        assert counter in str(e.value), str(e.value)
        assert e.value.sqlite_errorcode == sqlite3.SQLITE_ERROR
        c.commit()
        assert snapshot(c) == before
    finally:
        close(c)


@pytest.mark.parametrize("value", IN_RANGE)
@pytest.mark.parametrize("counter", COUNTERS)
def test_in_range_counter_merges(counter, value):
    c = make_db()
    try:
        merge(c, "t", pack(5), **{counter: value})
        c.commit()
        # A local write after it works.
        c.execute("INSERT INTO t VALUES (6, 'local')")
        c.execute("UPDATE t SET v = 'local' WHERE id = 1")
        c.commit()
    finally:
        close(c)


def test_col_version_at_bound_then_local_updates():
    # The reviewer's scenario (dbvmax.py), at the largest accepted value.
    c = make_db()
    try:
        merge(c, "t", pack(5), col_version=BOUND, db_version=5)
        c.commit()
        assert c.execute("SELECT v FROM t WHERE id = 5").fetchone() == ("X",)
        c.execute("UPDATE t SET v = 'local' WHERE id = 5")
        c.execute("UPDATE t SET v = 'local2' WHERE id = 5")
        c.commit()
        assert c.execute(
            "SELECT val, col_version FROM crsql_changes WHERE pk = ?",
            (pack(5),)).fetchall() == [("local2", BOUND + 2)]
    finally:
        close(c)


def test_dbvmax_scenario_rejected_and_local_writes_keep_working():
    # dbvmax.py: a merge of col_version = i64::MAX used to be accepted, and
    # every later local update of that cell failed with
    # "unable to reset cached trigger stmt, code CONSTRAINT".
    c = make_db()
    try:
        for dbv, colv in ((I64_MAX, 1), (5, I64_MAX)):
            with pytest.raises(sqlite3.OperationalError):
                merge(c, "t", pack(dbv), col_version=colv, db_version=dbv)
        c.commit()
        c.execute("INSERT INTO t VALUES (5, 'n')")
        c.execute("UPDATE t SET v = 'local2' WHERE id = 5")
        c.execute("UPDATE t SET v = 'local3' WHERE id = 1")
        c.commit()
        assert c.execute("SELECT v FROM t WHERE id = 5").fetchone() == (
            "local2",)
    finally:
        close(c)


# --- Error Message NUL Escape ----------------------------------------------

@pytest.mark.parametrize("tbl", ["t\x00", "\x00", "nope\x00x"])
def test_nul_in_table_name_is_sqlite_error_not_nomem(tbl):
    c = make_db()
    try:
        before = snapshot(c)
        # NOMEM surfaces in Python as MemoryError, not sqlite3.Error.
        with pytest.raises(sqlite3.OperationalError) as e:
            merge(c, tbl, pack(5))
        assert e.value.sqlite_errorcode == sqlite3.SQLITE_ERROR
        assert "could not find the schema information" in str(e.value)
        # The NUL is shown escaped, so the name is still recognisable.
        assert "\\0" in str(e.value), str(e.value)
        c.commit()
        assert snapshot(c) == before
    finally:
        close(c)

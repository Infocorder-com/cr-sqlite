# Packed-column length boundaries (docs/CRSQLITE_INIT_RACE_FIX.md,
# "Packed Blob Length Sign-Extension").
#
# A packed primary key stores a blob's or a text's length in the fewest
# big-endian bytes that hold it. bytes 1.11 made `Buf::get_int` sign-extend,
# so a blob length whose top encoded byte has bit 7 set (128-255,
# 32768-65535, 2^23 to 2^24-1) decoded as negative and every read of
# crsql_changes, crsql_unpack_columns and every merge of such a row failed.
# An empty blob made a debug build panic (a shift by 64).
#
# These tests round-trip blob, text and integer primary keys at every length
# boundary through pack, unpack, crsql_changes and a merge into a second
# database, and check that a malformed package is an error, never a crash.
import subprocess
import sys

import pytest

from crsql_correctness import connect, close
import crsql_correctness

LENGTHS = [0, 1, 127, 128, 255, 256, 32767, 32768, 65535, 65536,
           1 << 23, (1 << 24) - 1]

INTS = [0, 1, -1, 127, 128, 255, 256, 32767, 32768, 65535, 65536,
        (1 << 23) - 1, 1 << 23, (1 << 24) - 1, 1 << 24, (1 << 31) - 1,
        1 << 31, (1 << 32) - 1, 1 << 32, (1 << 63) - 1, -(1 << 63),
        -128, -129, -32768, -32769]


def blob_key(n):
    # Not all one byte value, so a truncated or shifted decode cannot pass.
    return bytes((i * 7 + 3) % 256 for i in range(n))


def text_key(n):
    return "".join(chr(ord("a") + (i % 26)) for i in range(n))


KEYS = ([pytest.param(blob_key(n), id=f"blob-{n}") for n in LENGTHS] +
        [pytest.param(text_key(n), id=f"text-{n}") for n in LENGTHS] +
        [pytest.param(i, id=f"int-{i}") for i in INTS])


def make_db():
    c = connect(":memory:")
    c.execute("CREATE TABLE t (id PRIMARY KEY NOT NULL, v)")
    c.execute("SELECT crsql_as_crr('t')")
    c.commit()
    return c


@pytest.mark.parametrize("key", KEYS)
def test_pack_unpack_round_trip(key):
    c = connect(":memory:")
    try:
        cells = c.execute(
            "SELECT cell FROM crsql_unpack_columns(crsql_pack_columns(?))",
            (key,)).fetchall()
        assert len(cells) == 1
        assert type(cells[0][0]) is type(key)
        assert cells[0][0] == key
    finally:
        close(c)


@pytest.mark.parametrize("key", KEYS)
def test_changes_read_and_merge(key):
    a = make_db()
    b = make_db()
    try:
        a.execute("INSERT INTO t VALUES (?, 'x')", (key,))
        a.commit()
        changes = a.execute("SELECT * FROM crsql_changes").fetchall()
        assert len(changes) == 1
        b.executemany(
            "INSERT INTO crsql_changes VALUES (" +
            ", ".join("?" * len(changes[0])) + ")",
            changes)
        b.commit()
        rows = b.execute("SELECT id, v FROM t").fetchall()
        assert len(rows) == 1
        assert type(rows[0][0]) is type(key)
        assert rows[0][0] == key
        assert rows[0][1] == "x"
        # The pk the receiver records packs to the same bytes.
        assert (b.execute("SELECT pk FROM crsql_changes").fetchall() ==
                [(changes[0][1],)])
    finally:
        close(a)
        close(b)


# Malformed packages. Run in a child process: on a broken build each one
# aborted the whole process (a panic across the FFI boundary), which would
# take the test run down with it.
MALFORMED = [
    pytest.param("x''", id="empty"),
    # Column count 1, type blob, a length of 9 bytes: more than a u64 holds.
    pytest.param("x'014C' || zeroblob(16)", id="blob-len-9-bytes"),
    pytest.param("x'0149' || zeroblob(16)", id="int-9-bytes"),
    pytest.param("x'014B' || zeroblob(16)", id="text-len-9-bytes"),
    pytest.param("x'01FC' || zeroblob(64)", id="blob-len-31-bytes"),
    # A declared length longer than the package.
    pytest.param("x'010C05' || x'0102'", id="blob-short"),
]

CHILD = r"""
import sqlite3, sys
c = sqlite3.connect(":memory:")
c.enable_load_extension(True)
c.load_extension(sys.argv[1])
c.execute("CREATE TABLE t (id PRIMARY KEY NOT NULL, v)")
c.execute("SELECT crsql_as_crr('t')")
for sql in ("SELECT count(*) FROM crsql_unpack_columns(" + sys.argv[2] + ")",
            "INSERT INTO crsql_changes VALUES ('t', " + sys.argv[2] +
            ", 'v', 1, 1, 1, randomblob(16), 1, 0, '0')"):
    try:
        c.execute(sql).fetchall()
        print("ACCEPTED", sql)
    except sqlite3.Error as e:
        print("ERROR", type(e).__name__, e)
"""


@pytest.mark.parametrize("package", MALFORMED)
def test_malformed_package_is_an_error_not_a_crash(package):
    r = subprocess.run(
        [sys.executable, "-I", "-c", CHILD, crsql_correctness.extension,
         package],
        capture_output=True, text=True, timeout=60)
    assert r.returncode == 0, (r.returncode, r.stdout, r.stderr)
    lines = r.stdout.splitlines()
    assert len(lines) == 2, r.stdout
    assert all(line.startswith("ERROR") for line in lines), r.stdout

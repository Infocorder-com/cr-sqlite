extern crate alloc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use crsql_bundle::test_exports::pack_columns::unpack_columns;
use crsql_bundle::test_exports::pack_columns::ColumnValue;
use sqlite::{Connection, ResultCode};
use sqlite_nostd as sqlite;

// The rust test is mainly to check with valgrind and ensure we're correctly
// freeing data as we do some passing of destructors from rust to SQLite.
// Complete property based tests for encode & decode exist in python.
fn test_pack_columns() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY, x, y)")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?, ?)")?;
    let blob: [u8; 3] = [1, 2, 3];

    insert_stmt.bind_int(1, 12)?;
    insert_stmt.bind_text(2, "str", sqlite::Destructor::STATIC)?;
    insert_stmt.bind_blob(3, &blob, sqlite::Destructor::STATIC)?;
    insert_stmt.step()?;

    let select_stmt = db
        .db
        .prepare_v2("SELECT quote(crsql_pack_columns(id, x, y)) FROM foo")?;
    select_stmt.step()?;
    let result = select_stmt.column_text(0)?;
    assert!(result == "X'03090C0B037374720C03010203'");
    // 03 09 0C 0B 03 73 74 72 0C 03 01 02 03
    // cols: 03
    // type & intlen: 09 -> 0b00001001 -> 01 type & 01 intlen
    // value: 0C -> 12
    // type & intlen: 0B -> 0b00001011 -> 03 type & 01 intlen
    // 03 -> len
    // 73 74 72 -> str
    // type & intlen: 0C ->  0b00001100 -> 04 type & 01 intlen
    // len: 03
    // bytes: 01 02 3
    // voila, done in 13 bytes! < 18 byte string < 26 byte binary w/o varints

    // Before variable length encoding:
    // 03 01 00 00 00 00 00 00 00 0C 03 00 00 00 03 73 74 72 04 00 00 00 03 01 02 03
    // cols:03
    // type: 01 (integer)
    // value: 00 00 00 00 00 00 00 0C (12) TODO: encode as variable length integers to save space?
    // type: 03 (text)
    // len: 00 00 00 03 (3)
    // byes: 73 (s) 74 (t) 72 (r)
    // type: 04 (blob)
    // len: 00 00 00 03 (3)
    // bytes: 01 02 03
    // vs string:
    // 12|'str'|x'010203'
    // ^ 18 bytes via string
    // vs
    // 26 bytes via binary
    // 13 bytes are wasted due to not using variable length encoding for integers
    // So.. do variable length ints?

    let select_stmt = db
        .db
        .prepare_v2("SELECT crsql_pack_columns(id, x, y) FROM foo")?;
    select_stmt.step()?;
    let result = select_stmt.column_blob(0)?;
    assert!(result.len() == 13);
    let unpacked = unpack_columns(result)?;
    assert!(unpacked.len() == 3);

    if let ColumnValue::Integer(i) = unpacked[0] {
        assert!(i == 12);
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Text(s) = &unpacked[1] {
        assert!(s == "str")
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Blob(b) = &unpacked[2] {
        assert!(b[..] == blob);
    } else {
        assert!("unexpected type" == "");
    }

    db.db.exec_safe("DELETE FROM foo")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?, ?)")?;

    insert_stmt.bind_int(1, 0)?;
    insert_stmt.bind_int(2, 10000000)?;
    insert_stmt.bind_int(3, -2500000)?;
    insert_stmt.step()?;

    let select_stmt = db
        .db
        .prepare_v2("SELECT crsql_pack_columns(id, x, y) FROM foo")?;
    select_stmt.step()?;
    let result = select_stmt.column_blob(0)?;
    let unpacked = unpack_columns(result)?;
    assert!(unpacked.len() == 3);

    if let ColumnValue::Integer(i) = unpacked[0] {
        assert!(i == 0);
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Integer(i) = unpacked[1] {
        assert!(i == 10000000)
    } else {
        assert!("unexpected type" == "");
    }
    if let ColumnValue::Integer(i) = unpacked[2] {
        assert!(i == -2500000);
    } else {
        assert!("unexpected type" == "");
    }

    Ok(())
}

fn test_unpack_columns() -> Result<(), ResultCode> {
    let db = crate::opendb().unwrap();
    db.db.exec_safe("CREATE TABLE foo (id PRIMARY KEY, x, y)")?;
    let insert_stmt = db.db.prepare_v2("INSERT INTO foo VALUES (?, ?, ?)")?;
    let blob: [u8; 3] = [1, 2, 3];

    insert_stmt.bind_int(1, 12)?;
    insert_stmt.bind_text(2, "str", sqlite::Destructor::STATIC)?;
    insert_stmt.bind_blob(3, &blob, sqlite::Destructor::STATIC)?;
    insert_stmt.step()?;

    let select_stmt = db
        .db
        .prepare_v2("SELECT cell FROM crsql_unpack_columns WHERE package = (SELECT crsql_pack_columns(id, x, y) FROM foo)")?;
    select_stmt.step()?;
    assert!(select_stmt.column_int(0) == 12);
    select_stmt.step()?;
    assert!(select_stmt.column_text(0)? == "str");
    select_stmt.step()?;
    assert!(select_stmt.column_blob(0)? == blob);

    db.db.exec_safe("CREATE TABLE bar (id PRIMARY KEY)")?;
    let int_col: [i64; 7] = [
        1,
        -1,
        i64::MAX,
        i64::MIN,
        i8::MAX as i64,
        i16::MIN as i64,
        10156800_i64,
    ];

    for i in int_col {
        let insert_stmt = db.db.prepare_v2("INSERT INTO bar VALUES (?)")?;
        insert_stmt.bind_int64(1, i)?;
        insert_stmt.step()?;

        let select_stmt = db
            .db
            .prepare_v2("SELECT crsql_pack_columns(id) FROM bar where id = ?")?;
        select_stmt.bind_int64(1, i)?;
        select_stmt.step()?;
        let result = select_stmt.column_blob(0)?;
        let unpacked = unpack_columns(result)?;
        assert!(unpacked.len() == 1);
        if let ColumnValue::Integer(got) = unpacked[0] {
            assert!(got == i, "int {} unpacked as {}", i, got);
        } else {
            assert!("unexpected type" == "");
        }
    }

    db.db.exec_safe("DELETE FROM bar")?;
    let text_col: [&str; 4] = ["a", ",", "-abcdefghijklmnopqrstuvwxyz1234567890?!", ""];

    for txt in text_col {
        let insert_stmt = db.db.prepare_v2("INSERT INTO bar VALUES (?)")?;
        insert_stmt.bind_text(1, txt, sqlite::Destructor::STATIC)?;
        insert_stmt.step()?;

        let select_stmt = db
            .db
            .prepare_v2("SELECT crsql_pack_columns(id) FROM bar where id = ?")?;
        select_stmt.bind_text(1, txt, sqlite::Destructor::STATIC)?;
        select_stmt.step()?;
        let result = select_stmt.column_blob(0)?;
        let unpacked = unpack_columns(result)?;
        assert!(unpacked.len() == 1);
        libc_print::std_name::println!("unpacked: {:?}", txt);
        if let ColumnValue::Text(i) = &unpacked[0] {
            assert!(i == txt);
        } else {
            assert!("unexpected type" == "");
        }
    }

    Ok(())
}

/// Blob and text lengths at every boundary of the length encoding: the length
/// is stored in the fewest big-endian bytes that hold it, so 128-255,
/// 32768-65535 and 2^23..2^24-1 have bit 7 of the top encoded byte set. bytes
/// 1.11 made `get_int` sign-extend, which decoded those blob lengths as
/// negative (**Packed Blob Length Sign-Extension**, docs/CRSQLITE_INIT_RACE_FIX.md);
/// a zero length made `get_int(0)` shift by 64 and panic in a debug build.
const BOUNDARY_LENGTHS: [usize; 12] = [
    0,
    1,
    127,
    128,
    255,
    256,
    32767,
    32768,
    65535,
    65536,
    1 << 23,
    (1 << 24) - 1,
];

fn pattern(n: usize) -> Vec<u8> {
    // Not one repeated byte, so a shifted or truncated decode cannot pass.
    (0..n).map(|i| ((i * 7 + 3) % 256) as u8).collect()
}

fn text_pattern(n: usize) -> String {
    (0..n).map(|i| (b'a' + (i % 26) as u8) as char).collect()
}

/// Packs one value with `crsql_pack_columns`, unpacks it with `unpack_columns`
/// and with the `crsql_unpack_columns` table, and returns the unpacked value
/// plus the table's (typeof, length) of the cell.
fn round_trip(
    db: &crate::CRConnection,
    bind: &dyn Fn(&sqlite::ManagedStmt) -> Result<ResultCode, ResultCode>,
) -> Result<(ColumnValue, String, i64), ResultCode> {
    let stmt = db.db.prepare_v2("SELECT crsql_pack_columns(?)")?;
    bind(&stmt)?;
    stmt.step()?;
    let mut unpacked = unpack_columns(stmt.column_blob(0)?)?;
    assert!(unpacked.len() == 1, "{} columns unpacked", unpacked.len());

    let stmt = db.db.prepare_v2(
        "SELECT typeof(cell), length(CAST(cell AS BLOB)) FROM crsql_unpack_columns(crsql_pack_columns(?))",
    )?;
    bind(&stmt)?;
    assert!(
        stmt.step()? == ResultCode::ROW,
        "crsql_unpack_columns: no row"
    );
    let typ = stmt.column_text(0)?.to_string();
    let len = stmt.column_int64(1);
    assert!(
        stmt.step()? == ResultCode::DONE,
        "crsql_unpack_columns: >1 row"
    );
    Ok((unpacked.remove(0), typ, len))
}

fn test_boundary_lengths() -> Result<(), ResultCode> {
    let db = crate::opendb()?;
    for n in BOUNDARY_LENGTHS {
        let blob = pattern(n);
        let (v, typ, len) =
            round_trip(&db, &|s| s.bind_blob(1, &blob, sqlite::Destructor::STATIC))?;
        match v {
            ColumnValue::Blob(b) => assert!(b == blob, "blob of {} bytes did not round-trip", n),
            _ => panic!("blob of {} bytes unpacked as another type", n),
        }
        assert!(
            typ == "blob" && len == n as i64,
            "vtab blob {}: {} {}",
            n,
            typ,
            len
        );

        let text = text_pattern(n);
        let (v, typ, len) =
            round_trip(&db, &|s| s.bind_text(1, &text, sqlite::Destructor::STATIC))?;
        match v {
            ColumnValue::Text(t) => assert!(t == text, "text of {} bytes did not round-trip", n),
            _ => panic!("text of {} bytes unpacked as another type", n),
        }
        assert!(
            typ == "text" && len == n as i64,
            "vtab text {}: {} {}",
            n,
            typ,
            len
        );
    }

    // Integer VALUES are signed: each width's boundaries, both signs.
    let ints: [i64; 22] = [
        0,
        1,
        -1,
        127,
        128,
        255,
        256,
        32767,
        32768,
        65535,
        65536,
        (1 << 23) - 1,
        1 << 23,
        (1 << 24) - 1,
        1 << 24,
        i32::MAX as i64,
        1 << 31,
        u32::MAX as i64,
        i64::MAX,
        i64::MIN,
        -129,
        -32769,
    ];
    for i in ints {
        let (v, typ, _) = round_trip(&db, &|s| s.bind_int64(1, i))?;
        match v {
            ColumnValue::Integer(got) => assert!(got == i, "int {} unpacked as {}", i, got),
            _ => panic!("int {} unpacked as another type", i),
        }
        assert!(typ == "integer", "vtab int {}: {}", i, typ);
    }
    Ok(())
}

/// A malformed package is an error, never a panic: a panic aborts the host
/// process, and a package arrives from a peer in every `crsql_changes` merge.
fn test_malformed_packages() -> Result<(), ResultCode> {
    let mut nine = vec![0x01u8, 0x4C];
    nine.extend_from_slice(&[0u8; 16]);
    let mut int_nine = vec![0x01u8, 0x49];
    int_nine.extend_from_slice(&[0u8; 16]);
    let mut text_nine = vec![0x01u8, 0x4B];
    text_nine.extend_from_slice(&[0u8; 16]);
    let mut blob_31 = vec![0x01u8, 0xFC];
    blob_31.extend_from_slice(&[0u8; 64]);
    let cases: [(&str, &[u8]); 7] = [
        ("empty", &[]),
        ("blob length in 9 bytes", &nine),
        ("integer in 9 bytes", &int_nine),
        ("text length in 9 bytes", &text_nine),
        ("blob length in 31 bytes", &blob_31),
        (
            "blob longer than the package",
            &[0x01, 0x0C, 0x05, 0x01, 0x02],
        ),
        ("two columns, one present", &[0x02, 0x05]),
    ];
    for (name, bytes) in cases {
        assert!(
            unpack_columns(bytes).is_err(),
            "malformed package accepted: {}",
            name
        );
    }
    Ok(())
}

pub fn run_suite() -> Result<(), ResultCode> {
    test_pack_columns()?;
    test_unpack_columns()?;
    test_boundary_lengths()?;
    test_malformed_packages()
}

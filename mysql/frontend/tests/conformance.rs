//! Embedded API behavior the `.sqltest` specs in
//! `mysql/conformance/my-sqltests` cannot express: result metadata,
//! parameter binding, persistence across reopen, and connection recovery.

use std::sync::Arc;
use turso_core::{MemoryIO, Value, IO};
use turso_mysql::{open_memory, open_with_io};

#[test]
fn persisted_create_table_reloads_through_mysql_dialect() -> turso_core::Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-create-table-persistence.db";

    {
        let connection = open_with_io(io.clone(), path)?;
        connection.execute(
            "CREATE TABLE `select` (`id` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
        )?;
        connection.close()?;
    }

    {
        let connection = open_with_io(io, path)?;
        let duplicate = connection
            .execute(
                "CREATE TABLE `select` (`id` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
            )
            .expect_err("reopened schema must contain the persisted table");
        assert!(duplicate.to_string().contains("already exists"));
        connection.execute(
            "CREATE TABLE IF NOT EXISTS `select` (`id` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
        )?;
        connection.close()?;
    }
    Ok(())
}

#[test]
fn persisted_mysql_ddl_keeps_column_rules_after_reopen() -> turso_core::Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-ddl-persistence.db";

    {
        let connection = open_with_io(io.clone(), path)?;
        connection.execute(
            "CREATE TABLE items (
                id INT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT 'key',
                code VARCHAR(3) NOT NULL,
                size ENUM('small', 'large') NOT NULL,
                added DATE DEFAULT '2020-1-2',
                PRIMARY KEY (id),
                UNIQUE KEY uq_code (code),
                KEY idx_size (size)
            ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
        )?;
        connection.execute("CREATE INDEX idx_added USING BTREE ON items (added)")?;
        connection.execute("ALTER TABLE items ADD COLUMN qty TINYINT UNSIGNED DEFAULT 1")?;
        connection.execute("INSERT INTO items (code) VALUES ('a'), ('b'), ('c')")?;
        connection.execute("DELETE FROM items WHERE id = 3")?;
        connection.close()?;
    }

    {
        let connection = open_with_io(io, path)?;
        connection.execute("INSERT INTO items (code, size) VALUES ('d', 'large')")?;
        let mut statement =
            connection.prepare("SELECT id, code, size, added, qty FROM items ORDER BY id")?;
        assert_eq!(
            statement.run_collect_rows()?,
            vec![
                item_row(1, "a", "small"),
                item_row(2, "b", "small"),
                vec![
                    Value::from_i64(4),
                    Value::build_text("d"),
                    Value::build_text("large"),
                    Value::build_text("2020-01-02"),
                    Value::from_i64(1),
                ],
            ]
        );
        for rejected in [
            "INSERT INTO items (code) VALUES ('long')",
            "INSERT INTO items (code) VALUES ('a')",
            "INSERT INTO items (code, size) VALUES ('e', 'huge')",
            "INSERT INTO items (code, qty) VALUES ('f', 256)",
            "INSERT INTO items (id, code) VALUES (-1, 'g')",
        ] {
            connection
                .execute(rejected)
                .expect_err("reopened table must keep its MySQL column rules");
        }
        connection.execute("DROP INDEX idx_added ON items")?;
        connection.execute("DROP TABLE items")?;
        connection.close()?;
    }
    Ok(())
}

fn item_row(id: i64, code: &'static str, size: &'static str) -> Vec<Value> {
    vec![
        Value::from_i64(id),
        Value::build_text(code),
        Value::build_text(size),
        Value::build_text("2020-01-02"),
        Value::from_i64(1),
    ]
}

#[test]
fn typed_tables_keep_their_marker_column_types_and_integer_keys_after_reopen(
) -> turso_core::Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-typed-table-persistence.db";

    {
        let connection = open_with_io(io.clone(), path)?;
        connection.execute(
            "CREATE TABLE t (
                id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
                n TINYINT,
                c CHAR(3),
                b BINARY(2)
            )",
        )?;
        connection.execute("INSERT INTO t (n, c, b) VALUES ('5', 'ab  ', 'x'), (-1.5, 'c', '')")?;
        connection.execute("ALTER TABLE t ADD COLUMN v VARCHAR(2)")?;
        connection.close()?;
    }

    {
        let connection = open_with_io(io, path)?;
        let stored = query(
            &connection,
            "SELECT sql FROM sqlite_schema WHERE name = 't'",
        )?;
        let [stored] = stored.as_slice() else {
            panic!("one schema row for t, got {stored:?}");
        };
        assert_eq!(
            stored,
            &vec![Value::build_text(
                "/* turso_dialect:mysql rowid_alias */ CREATE TABLE t (id BIGINT UNSIGNED NOT NULL \
                 PRIMARY KEY AUTOINCREMENT, n TINYINT, c CHAR(3), b BINARY(2), v VARCHAR(2)) STRICT"
            )]
        );
        connection.execute("INSERT INTO t (n, c, b, v) VALUES (7, 'd', 'yz', 'ok')")?;
        connection.execute("INSERT INTO t VALUES (0, 8, 'e', NULL, NULL)")?;
        assert_eq!(
            query(&connection, "SELECT rowid, id, n, c, hex(b), v FROM t")?,
            vec![
                typed_row(1, 5, "ab", "7800", None),
                typed_row(2, -2, "c", "0000", None),
                typed_row(3, 7, "d", "797A", Some("ok")),
                vec![
                    Value::from_i64(4),
                    Value::from_i64(4),
                    Value::from_i64(8),
                    Value::build_text("e"),
                    Value::Null,
                    Value::Null,
                ],
            ]
        );
        for rejected in [
            "INSERT INTO t (n) VALUES (128)",
            "INSERT INTO t (n) VALUES ('1x')",
            "INSERT INTO t (c) VALUES ('abcd')",
            "INSERT INTO t (b) VALUES ('xyz')",
            "INSERT INTO t (v) VALUES ('abc')",
            "INSERT INTO t (id) VALUES (-1)",
        ] {
            connection
                .execute(rejected)
                .expect_err("the reopened table must keep its MySQL column types");
        }
        connection.close()?;
    }
    Ok(())
}

fn query(
    connection: &Arc<turso_core::Connection>,
    sql: &str,
) -> turso_core::Result<Vec<Vec<Value>>> {
    connection.prepare(sql)?.run_collect_rows()
}

fn typed_row(
    key: i64,
    n: i64,
    c: &'static str,
    b: &'static str,
    v: Option<&'static str>,
) -> Vec<Value> {
    vec![
        Value::from_i64(key),
        Value::from_i64(key),
        Value::from_i64(n),
        Value::build_text(c),
        Value::build_text(b),
        v.map_or(Value::Null, Value::build_text),
    ]
}

#[test]
fn storing_errors_map_to_mysql_error_codes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (i INT, d DOUBLE, v VARCHAR(2), p POINT)")?;
    for (sql, code, message) in [
        (
            "INSERT INTO t (i) VALUES (1), (2147483648)",
            1264,
            "Out of range value for column 'i' at row 2",
        ),
        (
            "INSERT INTO t (i) VALUES ('abc')",
            1366,
            "Incorrect integer value: 'abc' for column 'i' at row 1",
        ),
        (
            "INSERT INTO t (i) VALUES ('12abc')",
            1265,
            "Data truncated for column 'i' at row 1",
        ),
        (
            "INSERT INTO t (d) VALUES ('abc')",
            1265,
            "Data truncated for column 'd' at row 1",
        ),
        (
            "INSERT INTO t (v) VALUES ('abc')",
            1406,
            "Data too long for column 'v' at row 1",
        ),
        (
            "INSERT INTO t (p) VALUES ('abc')",
            1416,
            "Cannot get geometry object from data you send to the GEOMETRY field",
        ),
    ] {
        let error = connection.execute(sql).expect_err(sql);
        let mysql = turso_mysql::mysql_error(&error).unwrap_or_else(|| panic!("{sql}: {error}"));
        assert_eq!(
            (mysql.code, mysql.message.as_str()),
            (code, message),
            "{sql}"
        );
    }
    connection.execute("CREATE TABLE k (id TINYINT AUTO_INCREMENT PRIMARY KEY)")?;
    connection.execute("INSERT INTO k VALUES (127)")?;
    let error = connection
        .execute("INSERT INTO k VALUES (NULL)")
        .expect_err("no key is left after 127");
    assert_eq!(
        turso_mysql::mysql_error(&error).map(|mysql| mysql.code),
        Some(1062)
    );
    Ok(())
}

#[test]
fn binary_strings_cannot_be_stored_in_number_columns_yet() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(
        "CREATE TABLE t (i INT, u BIGINT UNSIGNED, d DOUBLE, e DECIMAL(10, 2), b BIT(16))",
    )?;
    for column in ["i", "u", "d", "e", "b"] {
        let sql = format!("INSERT INTO t ({column}) VALUES (X'3132')");
        let error = connection.execute(&sql).expect_err(&sql);
        assert_eq!(
            turso_mysql::mysql_error(&error).map(|mysql| mysql.code),
            Some(1366),
            "{sql}"
        );
    }
    assert_eq!(
        query(&connection, "SELECT count(*) FROM t")?,
        vec![vec![Value::from_i64(0)]]
    );
    Ok(())
}

#[test]
fn bigint_unsigned_refuses_doubles_it_cannot_store_exactly() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (n INT, u BIGINT UNSIGNED)")?;
    for sql in [
        "INSERT INTO t VALUES (1, 12345678901234567891)",
        "INSERT INTO t VALUES (2, 9223372036854775808)",
        "INSERT INTO t VALUES (3, 1e19)",
    ] {
        let error = connection.execute(sql).expect_err(sql);
        assert_eq!(
            turso_mysql::mysql_error(&error).map(|mysql| mysql.code),
            Some(1366),
            "{sql}"
        );
    }
    connection.execute("INSERT INTO t VALUES (4, '12345678901234567891'), (5, 1e18)")?;
    assert_eq!(
        query(&connection, "SELECT n, CAST(u AS CHAR) FROM t ORDER BY n")?,
        vec![
            vec![
                Value::from_i64(4),
                Value::build_text("12345678901234567891")
            ],
            vec![Value::from_i64(5), Value::build_text("1000000000000000000")],
        ]
    );
    Ok(())
}

#[test]
fn create_table_the_mysql_lowering_refuses_reports_why_and_creates_nothing(
) -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, reason) in [
        (
            "CREATE TABLE t (a SERIAL)",
            "column type `SERIAL()` is not available",
        ),
        (
            "CREATE TABLE t (a INT) STRICT",
            "CREATE TABLE contains a clause that is not available",
        ),
        (
            "CREATE TABLE t (b BINARY(4611686018427387904)) STRICT",
            "CREATE TABLE contains a clause that is not available",
        ),
        (
            "CREATE TABLE t (d DOUBLE(2, 5)) STRICT",
            "CREATE TABLE contains a clause that is not available",
        ),
    ] {
        let error = connection.execute(sql).expect_err(sql);
        assert!(error.to_string().contains(reason), "{sql}: {error}");
    }
    assert_eq!(
        query(&connection, "SELECT name FROM sqlite_schema")?,
        Vec::<Vec<Value>>::new()
    );
    Ok(())
}

#[test]
fn store_functions_refuse_type_parameters_mysql_does_not_allow() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for sql in [
        "SELECT __mysql_assign_binary('a', 4611686018427387904)",
        "SELECT __mysql_assign_binary('a', 256)",
        "SELECT __mysql_assign_double(1, 2, 5, 0)",
        "SELECT __mysql_assign_double(1, 256, 2, 0)",
        "SELECT __mysql_assign_double(1, 60, 31, 0)",
    ] {
        query(&connection, sql).expect_err(sql);
    }
    assert_eq!(
        query(&connection, "SELECT __mysql_assign_binary('a', 2)")?,
        vec![vec![
            Value::from_slice(b"a\0").expect("two bytes fit in memory")
        ]]
    );
    Ok(())
}

#[test]
fn reports_projection_metadata() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let statement =
        connection.prepare("SELECT 1 AS answer, 'Ada' AS `display name`, NULL AS missing")?;
    assert_eq!(statement.num_columns(), 3);
    assert_eq!(statement.get_column_name(0), "answer");
    assert_eq!(statement.get_column_name(1), "display name");
    assert_eq!(statement.get_column_name(2), "missing");
    connection.close()?;
    Ok(())
}

#[test]
fn executes_bound_anonymous_projection_parameters() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let mut statement = connection.prepare("SELECT 1, ?, 'Ada', ? AS named, ?")?;
    assert_eq!(statement.parameters_count(), 3);
    assert_eq!(statement.num_columns(), 5);
    assert_eq!(statement.get_column_name(1), "?");
    assert_eq!(statement.get_column_name(3), "named");
    assert_eq!(statement.get_column_name(4), "?");

    statement.bind_at(1.try_into().unwrap(), Value::from_i64(42))?;
    statement.bind_at(2.try_into().unwrap(), Value::build_text("Grüße 世界 🦀"))?;
    statement.bind_at(3.try_into().unwrap(), Value::Null)?;
    let rows = statement.run_collect_rows()?;
    assert_eq!(
        rows,
        vec![vec![
            Value::from_i64(1),
            Value::from_i64(42),
            Value::build_text("Ada"),
            Value::build_text("Grüße 世界 🦀"),
            Value::Null,
        ]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn resets_and_rebinds_anonymous_projection_parameters() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let mut statement = connection.prepare("SELECT ?, ?, ?")?;

    statement.bind_at(1.try_into().unwrap(), Value::build_text("stale text"))?;
    statement.bind_at(2.try_into().unwrap(), Value::from_i64(7))?;
    statement.bind_at(3.try_into().unwrap(), Value::Null)?;
    assert_eq!(
        statement.run_collect_rows()?,
        vec![vec![
            Value::build_text("stale text"),
            Value::from_i64(7),
            Value::Null,
        ]]
    );

    statement.reset()?;
    statement.bind_at(1.try_into().unwrap(), Value::build_text("fresh"))?;
    statement.bind_at(2.try_into().unwrap(), Value::Null)?;
    statement.bind_at(3.try_into().unwrap(), Value::from_i64(9))?;
    assert_eq!(
        statement.run_collect_rows()?,
        vec![vec![
            Value::build_text("fresh"),
            Value::Null,
            Value::from_i64(9),
        ]]
    );
    connection.close()?;
    Ok(())
}

#[test]
fn connection_remains_usable_after_a_prepare_error() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection
        .prepare("SELECT 1 FROM unsupported_table")
        .expect_err("unsupported statement must fail");
    let mut statement = connection.prepare("SELECT 'still usable'")?;
    let rows = statement.run_collect_rows()?;
    assert_eq!(rows, vec![vec![Value::build_text("still usable")]]);
    connection.close()?;
    Ok(())
}

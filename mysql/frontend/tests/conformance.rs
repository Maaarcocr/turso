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
                 PRIMARY KEY AUTOINCREMENT, n TINYINT, c CHAR(3) COLLATE \"und-u-ks-level1\", b BINARY(2), v VARCHAR(2) COLLATE \"und-u-ks-level1\") STRICT"
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

#[test]
fn tables_made_by_create_table_as_select_keep_their_column_types_after_reopen(
) -> turso_core::Result<()> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let path = "mysql-create-table-as-select-persistence.db";

    {
        let connection = open_with_io(io.clone(), path)?;
        connection.execute("CREATE TABLE s (k VARCHAR(3), n TINYINT, l BIT(3))")?;
        connection.execute("INSERT INTO s VALUES ('a', 1, 5)")?;
        connection.execute("CREATE TABLE t AS SELECT k, n, l, CONCAT(k, 'x') AS c FROM s")?;
        connection.close()?;
    }

    {
        let connection = open_with_io(io, path)?;
        assert_eq!(
            query(
                &connection,
                "SELECT sql FROM sqlite_schema WHERE name = 't'"
            )?,
            vec![vec![Value::build_text(
                "/* turso_dialect:mysql */ CREATE TABLE t (k varchar (3), n tinyint, l bit (3), \
                 c longtext) STRICT"
            )]]
        );
        connection.execute("INSERT INTO t VALUES ('b', '2', 7, 12)")?;
        assert_eq!(
            query(&connection, "SELECT k, n, l, c FROM t ORDER BY k")?,
            vec![
                vec![
                    Value::build_text("a"),
                    Value::from_i64(1),
                    Value::from_i64(5),
                    Value::build_text("ax"),
                ],
                vec![
                    Value::build_text("b"),
                    Value::from_i64(2),
                    Value::from_i64(7),
                    Value::build_text("12"),
                ],
            ]
        );
        for rejected in [
            "INSERT INTO t (k) VALUES ('abcd')",
            "INSERT INTO t (n) VALUES (128)",
            "INSERT INTO t (l) VALUES (8)",
        ] {
            connection
                .execute(rejected)
                .expect_err("the reopened table must keep the copied column types");
        }
        connection.close()?;
    }
    Ok(())
}

#[test]
fn transactions_keep_mysql_semantics() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (id INT PRIMARY KEY, d DATE)")?;
    for statement in ["COMMIT", "ROLLBACK", "SAVEPOINT a"] {
        connection.execute(statement)?;
    }
    for statement in ["RELEASE SAVEPOINT a", "ROLLBACK TO SAVEPOINT a"] {
        let error = connection.execute(statement).expect_err(statement);
        assert!(error.to_string().contains("does not exist"), "{error}");
    }
    let commit_prepared_outside = connection.prepare("COMMIT")?;
    connection.execute("BEGIN")?;
    connection.execute("INSERT INTO t VALUES (1, '2024-01-01')")?;
    connection
        .execute("INSERT INTO t VALUES (2, '2024-01-02'), (1, '2024-01-03')")
        .expect_err("a duplicate key fails the statement");
    connection
        .execute("INSERT INTO t VALUES (3, '2024-01-03'), (4, '2024-02-30')")
        .expect_err("an invalid date fails the statement");
    connection.execute("SAVEPOINT s")?;
    connection.execute("INSERT INTO t VALUES (5, NULL)")?;
    connection.execute("ROLLBACK TO SAVEPOINT s")?;
    for statement in [
        "CREATE TABLE u (i INT)",
        "DROP TABLE t",
        "START TRANSACTION",
        "SET autocommit = 1",
    ] {
        let error = connection.execute(statement).expect_err(statement);
        assert!(
            error.to_string().contains("inside a transaction"),
            "{statement}: {error}"
        );
    }
    let mut commit = commit_prepared_outside;
    commit.run_collect_rows()?;
    assert!(
        connection.get_auto_commit(),
        "COMMIT must end the transaction"
    );
    connection.execute("ROLLBACK")?;
    assert_eq!(
        query(&connection, "SELECT id FROM t")?,
        vec![vec![Value::from_i64(1)]]
    );
    connection.execute("START TRANSACTION")?;
    connection.execute("INSERT INTO t VALUES (6, NULL)")?;
    connection.execute("ROLLBACK")?;
    assert_eq!(
        query(&connection, "SELECT count(*) FROM t")?,
        vec![vec![Value::from_i64(1)]]
    );
    let mut isolation = connection.prepare("SELECT @@transaction_isolation")?;
    assert_eq!(
        isolation.run_collect_rows()?,
        vec![vec![Value::build_text("REPEATABLE-READ")]]
    );
    connection.execute("SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED")?;
    isolation.reset()?;
    assert_eq!(
        isolation.run_collect_rows()?,
        vec![vec![Value::build_text("READ-COMMITTED")]]
    );
    let other = open_memory()?;
    assert_eq!(
        query(&other, "SELECT @@transaction_isolation")?,
        vec![vec![Value::build_text("REPEATABLE-READ")]]
    );
    Ok(())
}

#[test]
fn user_level_locks_belong_to_a_session_until_it_closes() -> turso_core::Result<()> {
    let first = open_memory()?;
    let second = open_memory()?;
    let one = |connection: &Arc<turso_core::Connection>, sql: &str| {
        query(connection, sql).map(|rows| rows[0].clone())
    };
    assert_eq!(
        one(
            &first,
            "SELECT GET_LOCK('conformance_lock', 0), GET_LOCK('CONFORMANCE_LOCK', 5)"
        )?,
        vec![Value::from_i64(1), Value::from_i64(1)]
    );
    assert_eq!(
        one(
            &second,
            "SELECT GET_LOCK('conformance_lock', 0), RELEASE_LOCK('conformance_lock'),              IS_FREE_LOCK('conformance_lock'), IS_USED_LOCK('conformance_lock') <> CONNECTION_ID()"
        )?,
        vec![Value::from_i64(0), Value::from_i64(0), Value::from_i64(0), Value::from_i64(1)]
    );
    let error = second
        .execute("SELECT GET_LOCK('conformance_lock', 1)")
        .expect_err("waiting for a lock is refused");
    assert!(error.to_string().contains("not supported yet"), "{error}");
    first.close()?;
    drop(first);
    assert_eq!(
        one(
            &second,
            "SELECT GET_LOCK('conformance_lock', 0), RELEASE_ALL_LOCKS(), RELEASE_LOCK('conformance_lock')"
        )?,
        vec![Value::from_i64(1), Value::from_i64(1), Value::Null]
    );
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
fn enum_and_set_columns_store_the_members_a_value_names_or_numbers() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (id INT, e ENUM('b', 'a', 'c,d'), s SET('x', 'y', 'z'))")?;
    connection.execute(
        "INSERT INTO t VALUES (1, 'A ', 'z,X'), (2, 3, 6), (3, '2', '5'), (4, NULL, ''), (5, 'c,d', 'x,x')",
    )?;
    let text = |value: Option<&'static str>| value.map_or(Value::Null, Value::build_text);
    assert_eq!(
        query(&connection, "SELECT e, s FROM t ORDER BY id")?,
        [
            (Some("a"), Some("x,z")),
            (Some("c,d"), Some("y,z")),
            (Some("a"), Some("x,z")),
            (None, Some("")),
            (Some("c,d"), Some("x")),
        ]
        .into_iter()
        .map(|(e, s)| vec![text(e), text(s)])
        .collect::<Vec<_>>()
    );
    for rejected in [
        "INSERT INTO t (e) VALUES ('q')",
        "INSERT INTO t (e) VALUES (0)",
        "INSERT INTO t (e) VALUES (4)",
        "INSERT INTO t (s) VALUES ('x,q')",
        "INSERT INTO t (s) VALUES (8)",
        "UPDATE t SET e = 'zz'",
    ] {
        let error = connection.execute(rejected).expect_err(rejected);
        assert_eq!(
            turso_mysql::mysql_error(&error).map(|mysql| mysql.code),
            Some(1265),
            "{rejected}"
        );
    }
    connection.execute("CREATE TABLE b (s SET('x', 'y') COLLATE utf8mb4_0900_bin)")?;
    connection.execute("INSERT INTO b VALUES ('y,x')")?;
    connection
        .execute("INSERT INTO b VALUES ('X')")
        .expect_err("a case-sensitive collation matches names with their case");
    assert_eq!(
        query(
            &connection,
            "SELECT id FROM t WHERE e = 'a' OR e < 'b' ORDER BY id"
        )?,
        vec![vec![Value::from_i64(1)], vec![Value::from_i64(3)]]
    );
    for refused in [
        "SELECT e + 0 FROM t",
        "SELECT id FROM t WHERE e = 2",
        "SELECT -s FROM t",
    ] {
        let error = query(&connection, refused).expect_err(refused);
        assert!(
            error.to_string().contains("not supported yet"),
            "{refused}: {error}"
        );
    }
    Ok(())
}

#[test]
fn user_variables_keep_their_values_for_the_session() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let mut read = connection.prepare("SELECT @A")?;
    assert_eq!(read.run_collect_rows()?, vec![vec![Value::Null]]);
    connection.execute("SET @a = 41, @s = 'abc', @h = X'41'")?;
    read.reset()?;
    assert_eq!(read.run_collect_rows()?, vec![vec![Value::from_i64(41)]]);
    assert_eq!(
        query(
            &connection,
            "SELECT @a + 1, @s = 'ABC', @h = 'a', @nope IS NULL"
        )?,
        vec![vec![
            Value::from_i64(42),
            Value::from_i64(1),
            Value::from_i64(0),
            Value::from_i64(1)
        ]]
    );
    connection.execute("SET @saved = @@character_set_client")?;
    connection.execute("SET character_set_client = @saved")?;
    for refused in [
        "SET @d = 1.5",
        "SET @c = CONCAT('a', 'b'); SELECT @c = 'AB'",
        "SET @x = 'latin1'; SET character_set_client = @x",
    ] {
        let error = connection.execute(refused).expect_err(refused);
        assert!(
            error.to_string().contains("not supported yet"),
            "{refused}: {error}"
        );
    }
    let other = open_memory()?;
    assert_eq!(query(&other, "SELECT @a")?, vec![vec![Value::Null]]);
    Ok(())
}

#[test]
fn json_columns_store_mysql_documents() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (id INT PRIMARY KEY, j JSON, s TEXT)")?;
    connection.execute(
        r#"INSERT INTO t VALUES (1, '{"b":2,  "a":[1,2], "aa": true, "b": 3}', '{"a": 1}')"#,
    )?;
    let texts = |sql: &str| -> turso_core::Result<Vec<Vec<String>>> {
        Ok(query(&connection, sql)?
            .into_iter()
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect())
    };
    assert_eq!(
        texts("SELECT j, JSON_ARRAY(j), JSON_ARRAY(s), JSON_TYPE(j) FROM t")?,
        vec![vec![
            r#"{"a": [1, 2], "b": 3, "aa": true}"#,
            r#"[{"a": [1, 2], "b": 3, "aa": true}]"#,
            r#"["{\"a\": 1}"]"#,
            "OBJECT",
        ]]
    );
    for (value, error) in [
        ("'{bad'", "ERROR 3140"),
        ("1", "ERROR 3140"),
        ("X'7B7D'", "ERROR 3144"),
    ] {
        let sql = format!("UPDATE t SET j = {value}");
        let failure = connection.execute(&sql).expect_err(&sql);
        assert!(failure.to_string().contains(error), "{sql}: {failure}");
    }
    Ok(())
}

#[test]
fn temporal_columns_store_the_text_mysql_stores() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, a DATETIME(6), b DATETIME, c DATE, d TIME(2), e TIMESTAMP)",
    )?;
    connection.execute(
        "INSERT INTO t VALUES (1, '2024-3-5 9:4:5.25', '2024-03-05T10:00:00', '2024-3-5 00:00:00', '1:2:3.4', '1970-01-01 00:00:01')",
    )?;
    connection.execute("UPDATE t SET b = '2024-02-29 23:59:59.5'")?;
    assert_eq!(
        query(&connection, "SELECT a, b, c, d, e FROM t")?,
        vec![vec![
            Value::build_text("2024-03-05 09:04:05.250000"),
            Value::build_text("2024-03-01 00:00:00"),
            Value::build_text("2024-03-05"),
            Value::build_text("01:02:03.40"),
            Value::build_text("1970-01-01 00:00:01"),
        ]]
    );
    for (rejected, column, value) in [
        ("Incorrect datetime value", "b", "'2024-02-30 00:00:00'"),
        ("Incorrect datetime value", "e", "'1970-01-01 00:00:00'"),
        ("Incorrect date value", "c", "'2024-00-05'"),
        ("Incorrect time value", "d", "'839:00:00'"),
        ("not supported yet", "b", "'9999-12-31 23:59:59.5'"),
        ("not supported yet", "d", "'-00:00:00.001'"),
        ("not supported yet", "c", "'2024-02-01 10:00:00'"),
        ("not supported yet", "b", "20240201"),
    ] {
        let sql = format!("UPDATE t SET {column} = {value}");
        let error = connection.execute(&sql).expect_err(&sql);
        assert!(error.to_string().contains(rejected), "{sql}: {error}");
    }
    Ok(())
}

#[test]
fn multi_table_updates_change_each_row_once() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE a (id INT PRIMARY KEY, v INT)")?;
    connection.execute("CREATE TABLE b (id INT, w INT)")?;
    connection.execute("INSERT INTO a VALUES (1, 0), (2, 0), (3, 0)")?;
    connection.execute("INSERT INTO b VALUES (1, 10), (2, 20), (2, 30), (3, 40)")?;
    let rows = |connection: &Arc<turso_core::Connection>| {
        query(connection, "SELECT id, v FROM a ORDER BY id")
    };
    connection.execute("UPDATE a JOIN b ON b.id = a.id SET a.v = a.v + 1")?;
    let once = vec![
        vec![Value::from_i64(1), Value::from_i64(1)],
        vec![Value::from_i64(2), Value::from_i64(1)],
        vec![Value::from_i64(3), Value::from_i64(1)],
    ];
    assert_eq!(rows(&connection)?, once);
    let error = connection
        .execute("UPDATE a JOIN b ON b.id = a.id SET a.v = b.w")
        .expect_err("row 2 takes the values of one of two joined rows");
    assert!(error.to_string().contains("not supported yet"), "{error}");
    assert_eq!(rows(&connection)?, once);
    connection.execute("UPDATE a JOIN b ON b.id = a.id SET a.v = b.w WHERE b.w <> 30")?;
    assert_eq!(
        rows(&connection)?,
        vec![
            vec![Value::from_i64(1), Value::from_i64(10)],
            vec![Value::from_i64(2), Value::from_i64(20)],
            vec![Value::from_i64(3), Value::from_i64(40)],
        ]
    );
    Ok(())
}

#[test]
fn json_aggregates_collect_the_rows_of_each_group() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE c (id BIGINT PRIMARY KEY, p INT, lbl VARCHAR(5))")?;
    connection
        .execute("INSERT INTO c VALUES (10, 1, 'x'), (11, 1, 'y'), (12, 2, 'z'), (13, 2, 'x')")?;
    let texts = |sql: &str| -> turso_core::Result<Vec<Vec<String>>> {
        Ok(query(&connection, sql)?
            .into_iter()
            .map(|row| row.iter().map(ToString::to_string).collect())
            .collect())
    };
    assert_eq!(
        texts("SELECT p, JSON_ARRAYAGG(id) FROM c GROUP BY p ORDER BY p")?,
        vec![vec!["1", "[10, 11]"], vec!["2", "[12, 13]"]]
    );
    assert_eq!(
        texts("SELECT JSON_OBJECTAGG(lbl, id), JSON_ARRAYAGG(lbl = 'x') FROM c")?,
        vec![vec![
            r#"{"x": 13, "y": 11, "z": 12}"#,
            "[true, false, false, true]"
        ]]
    );
    assert_eq!(
        texts("SELECT JSON_ARRAYAGG(id), JSON_OBJECTAGG(lbl, id) FROM c WHERE id > 99")?,
        vec![vec!["", ""]]
    );
    assert_eq!(
        texts("SELECT JSON_ARRAY(JSON_ARRAYAGG(JSON_OBJECT('i', id))) FROM c WHERE p = 1")?,
        vec![vec![r#"[[{"i": 10}, {"i": 11}]]"#]]
    );
    let error = query(&connection, "SELECT JSON_OBJECTAGG(NULL, id) FROM c").expect_err("NULL key");
    assert!(error.to_string().contains("ERROR 3158"), "{error}");
    Ok(())
}

#[test]
fn dates_compare_with_strings_and_other_dates_as_datetimes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (id INT, d DATE, dt DATETIME(3), tm TIME)")?;
    connection.execute(
        "INSERT INTO t VALUES (1, '2020-01-02', '2020-01-02 00:00:00', '10:00:00'),          (2, '2020-01-03', '2020-01-02 10:00:00.5', NULL)",
    )?;
    let ids = |sql: &str| -> turso_core::Result<Vec<i64>> {
        Ok(query(&connection, sql)?
            .into_iter()
            .map(|row| match row[0] {
                Value::Numeric(turso_core::Numeric::Integer(id)) => id,
                ref other => panic!("{sql}: {other}"),
            })
            .collect())
    };
    assert_eq!(
        ids("SELECT id FROM t WHERE d = '2020-1-2' ORDER BY id")?,
        vec![1]
    );
    assert_eq!(ids("SELECT id FROM t WHERE d = dt ORDER BY id")?, vec![1]);
    assert_eq!(
        ids("SELECT id FROM t WHERE '20200102095959' < dt ORDER BY id")?,
        vec![2]
    );
    assert_eq!(
        ids("SELECT id FROM t WHERE d >= '2020-01-02 00:00:01' ORDER BY id")?,
        vec![2]
    );
    for failing in [
        "SELECT id FROM t WHERE d >= 'not a valid date'",
        "SELECT id FROM t WHERE dt < '12_4'",
    ] {
        query(&connection, failing).expect_err(failing);
    }
    for refused in [
        "SELECT id FROM t WHERE d = 20200102",
        "SELECT id FROM t WHERE d = '0000-00-00'",
        "SELECT id FROM t WHERE d < tm",
    ] {
        let error = query(&connection, refused).expect_err(refused);
        assert!(
            error.to_string().contains("not supported yet"),
            "{refused}: {error}"
        );
    }
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
fn integer_casts_round_decimals_and_doubles_and_truncate_strings() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (d DECIMAL(5, 2), f DOUBLE, s VARCHAR(20), day DATE)")?;
    connection.execute("INSERT INTO t VALUES (-2.50, 2.5, ' -7.9xyz', '2020-01-01')")?;
    assert_eq!(
        query(
            &connection,
            "SELECT CAST(d AS SIGNED), CAST(f AS SIGNED), CAST(s AS SIGNED), \
             CAST(1.5 AS UNSIGNED), CAST('18446744073709551615' AS SIGNED), \
             CAST(1e30 AS SIGNED) FROM t"
        )?,
        vec![vec![
            Value::from_i64(-3),
            Value::from_i64(2),
            Value::from_i64(-7),
            Value::from_i64(2),
            Value::from_i64(-1),
            Value::from_i64(i64::MAX),
        ]]
    );
    for sql in [
        "SELECT CAST(d AS UNSIGNED) FROM t",
        "SELECT CAST(day AS SIGNED) FROM t",
    ] {
        connection.prepare(sql)?.run_collect_rows().expect_err(sql);
    }
    Ok(())
}

#[test]
fn bigint_unsigned_stores_integer_literals_exactly_and_refuses_doubles() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (n INT, u BIGINT UNSIGNED)")?;
    let sql = "INSERT INTO t VALUES (3, 1e19)";
    let error = connection.execute(sql).expect_err(sql);
    assert_eq!(
        turso_mysql::mysql_error(&error).map(|mysql| mysql.code),
        Some(1366),
        "{sql}"
    );
    connection.execute(
        "INSERT INTO t VALUES (1, 12345678901234567891), (2, 9223372036854775808), (4, '12345678901234567891'), (5, 1e18)",
    )?;
    assert_eq!(
        query(&connection, "SELECT n, CAST(u AS CHAR) FROM t ORDER BY n")?,
        vec![
            vec![
                Value::from_i64(1),
                Value::build_text("12345678901234567891")
            ],
            vec![Value::from_i64(2), Value::build_text("9223372036854775808")],
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
            "STRICT and WITHOUT ROWID are not MySQL table options",
        ),
        (
            "CREATE TABLE t (b BINARY(4611686018427387904)) STRICT",
            "STRICT and WITHOUT ROWID are not MySQL table options",
        ),
        (
            "CREATE TABLE t (d DOUBLE(2, 5)) STRICT",
            "STRICT and WITHOUT ROWID are not MySQL table options",
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

#[test]
fn schema_names_with_backslashes_are_found_after_create() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(r"CREATE TABLE `a\b` (`x\y` INT)")?;
    connection.execute(r"CREATE INDEX `i\n` ON `a\b` (`x\y`)")?;
    connection.execute(r"INSERT INTO `a\b` VALUES (7)")?;
    let mut statement = connection.prepare(r"SELECT `x\y` FROM `a\b` WHERE `x\y` = 7")?;
    assert_eq!(
        statement.run_collect_rows()?,
        vec![vec![Value::from_i64(7)]]
    );
    connection.execute(r"DROP INDEX `i\n` ON `a\b`")?;
    connection.execute(r"DROP TABLE `a\b`")?;
    Ok(())
}

#[test]
fn connections_check_foreign_keys_from_the_start() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE parent (id INT PRIMARY KEY)")?;
    connection.execute(
        "CREATE TABLE child (id INT PRIMARY KEY, parent_id INT, FOREIGN KEY (parent_id) REFERENCES parent (id))",
    )?;
    connection.execute("INSERT INTO parent VALUES (1)")?;
    connection.execute("INSERT INTO child VALUES (1, 1)")?;
    connection
        .execute("INSERT INTO child VALUES (2, 9)")
        .expect_err("a child row without a parent must be rejected");
    connection
        .execute("DELETE FROM parent WHERE id = 1")
        .expect_err("a referenced parent row must not be deleted");
    connection.execute("SET FOREIGN_KEY_CHECKS = 0")?;
    connection.execute("INSERT INTO child VALUES (2, 9)")?;
    Ok(())
}

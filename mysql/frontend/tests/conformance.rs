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
fn drop_index_names_the_table_of_the_index() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t1 (a INT)")?;
    connection.execute("CREATE TABLE t2 (a INT)")?;
    connection.execute("CREATE INDEX idx ON t1 (a)")?;
    let mut drop_from_t2 = connection.prepare("DROP INDEX idx ON t2");
    let error = drop_from_t2
        .as_mut()
        .map(|_| ())
        .expect_err("idx is not an index of t2");
    assert!(
        error.to_string().contains("check that column/key exists"),
        "{error}"
    );
    let index_count = "SELECT count(*) FROM sqlite_schema WHERE type = 'index'";
    assert_eq!(
        query(&connection, index_count)?,
        vec![vec![Value::from_i64(1)]]
    );
    let mut drop_from_t1 = connection.prepare("ALTER TABLE t1 DROP INDEX IDX")?;
    drop_from_t1.run_collect_rows()?;
    assert_eq!(
        query(&connection, index_count)?,
        vec![vec![Value::from_i64(0)]]
    );
    connection
        .execute("DROP INDEX idx ON t1")
        .expect_err("idx was dropped");
    Ok(())
}

#[test]
fn foreign_keys_are_checked_when_created_as_mysql_checks_them() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for statement in [
        "CREATE TABLE p (id INT(11) UNSIGNED PRIMARY KEY, x INT, s VARCHAR(10) COLLATE utf8mb4_bin, t TEXT, UNIQUE KEY (x))",
        "CREATE TABLE c (pid INT UNSIGNED, px INT, CONSTRAINT f FOREIGN KEY (pid) REFERENCES p (id) ON DELETE SET NULL, FOREIGN KEY (px) REFERENCES p (x))",
        "CREATE TABLE tree (id INT PRIMARY KEY, parent INT, FOREIGN KEY (parent) REFERENCES tree (id))",
    ] {
        connection.execute(statement)?;
    }
    let rejected = [
        "CREATE TABLE d (a INT, FOREIGN KEY (a) REFERENCES nope (id))",
        "CREATE TABLE d (a INT UNSIGNED, FOREIGN KEY (a) REFERENCES p (nope))",
        "CREATE TABLE d (a VARCHAR(10), FOREIGN KEY (a) REFERENCES p (s))",
        "CREATE TABLE d (a TEXT, FOREIGN KEY (a) REFERENCES p (t))",
        "CREATE TABLE d (a BIGINT, FOREIGN KEY (a) REFERENCES p (x))",
        "CREATE TABLE d (a INT, FOREIGN KEY (a) REFERENCES p (id))",
        "CREATE TABLE d (a INT UNSIGNED NOT NULL, FOREIGN KEY (a) REFERENCES p (id) ON UPDATE SET NULL)",
        "CREATE TABLE d (a INT UNSIGNED, CONSTRAINT F FOREIGN KEY (a) REFERENCES p (id))",
    ];
    for statement in rejected {
        let error = connection.execute(statement).expect_err(statement);
        assert!(
            error.to_string().contains("MySQL rejects"),
            "{statement}: {error}"
        );
    }
    let refused = connection
        .execute("CREATE TABLE d (a DECIMAL(4,1), FOREIGN KEY (a) REFERENCES p (x))")
        .expect_err("MySQL compares keys of different types as stored");
    assert!(refused.to_string().contains("unsupported"), "{refused}");
    connection.execute("CREATE TABLE dp (d DECIMAL(4,2) PRIMARY KEY, t DATETIME UNIQUE)")?;
    connection.execute("INSERT INTO dp VALUES (1.23, '2001-02-03 12:34:56')")?;
    connection.execute("CREATE TABLE same (d DECIMAL(3,2), FOREIGN KEY (d) REFERENCES dp (d))")?;
    connection.execute("INSERT INTO same VALUES (1.23)")?;
    for statement in [
        "CREATE TABLE scale (d DECIMAL(4,1), FOREIGN KEY (d) REFERENCES dp (d))",
        "CREATE TABLE stamp (t TIMESTAMP, FOREIGN KEY (t) REFERENCES dp (t))",
    ] {
        connection.execute(statement)?;
    }
    connection.execute("INSERT INTO scale VALUES (NULL)")?;
    connection.execute("CREATE TABLE cp (c CHAR(3) PRIMARY KEY)")?;
    connection.execute("INSERT INTO cp VALUES ('abc')")?;
    connection.execute("CREATE TABLE vc (v VARCHAR(10), FOREIGN KEY (v) REFERENCES cp (c))")?;
    connection.execute("INSERT INTO vc VALUES ('abc')")?;
    for (statement, expected) in [
        ("INSERT INTO scale VALUES (1.2)", "MySQL rejects"),
        (
            "INSERT INTO stamp VALUES ('2001-02-03 12:34:56')",
            "MySQL rejects",
        ),
        ("UPDATE scale SET d = 1.2", "unsupported"),
        ("INSERT INTO scale SELECT 1.2", "unsupported"),
        ("INSERT INTO vc VALUES ('abc ')", "unsupported"),
    ] {
        let error = connection
            .execute(statement)
            .expect_err("MySQL matches these keys as stored");
        assert!(error.to_string().contains(expected), "{statement}: {error}");
    }
    connection.execute("SET FOREIGN_KEY_CHECKS = 0")?;
    connection.execute("CREATE TABLE d (a INT, FOREIGN KEY (a) REFERENCES later (id))")?;
    Ok(())
}

#[test]
fn column_expressions_call_mysql_s_functions() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(
        "CREATE TABLE t (pk INT PRIMARY KEY, a VARCHAR(5), c VARCHAR(10) AS (CONCAT(a, 'x')), w VARCHAR(10) DEFAULT (CONCAT('a', 'b')))",
    )?;
    connection.execute("INSERT INTO t (pk) VALUES (1)")?;
    assert_eq!(
        query(&connection, "SELECT c, w FROM t")?,
        vec![vec![Value::Null, Value::from_text("ab")]]
    );
    let error = connection
        .execute("CREATE TABLE u (pk INT PRIMARY KEY, v INT DEFAULT (CUSTOMFUNC(1)))")
        .expect_err("Turso has no CUSTOMFUNC");
    assert!(error.to_string().contains("unsupported"), "{error}");
    Ok(())
}

#[test]
fn default_writes_the_default_of_the_column() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(
        "CREATE TABLE t (pk INT PRIMARY KEY AUTO_INCREMENT, i INT DEFAULT 7, \
         s VARCHAR(5) DEFAULT 'x', n INT, g INT AS (i + 1))",
    )?;
    connection.execute("INSERT INTO t (pk) VALUES (DEFAULT), (10), (DEFAULT)")?;
    connection.execute("INSERT INTO t VALUES (DEFAULT, 1, DEFAULT, DEFAULT, DEFAULT)")?;
    connection.execute("INSERT INTO t SET i = DEFAULT, n = 5")?;
    connection.execute("UPDATE t SET i = 2 WHERE pk IN (1, 10)")?;
    connection.execute("UPDATE t SET s = DEFAULT, n = DEFAULT WHERE pk = 12")?;
    connection
        .execute("INSERT INTO t (pk, i) VALUES (1, 0) ON DUPLICATE KEY UPDATE i = DEFAULT")?;
    let row = |pk: i64, i: i64, n: Option<i64>| {
        vec![
            Value::from_i64(pk),
            Value::from_i64(i),
            Value::build_text("x"),
            n.map_or(Value::Null, Value::from_i64),
            Value::from_i64(i + 1),
        ]
    };
    assert_eq!(
        query(&connection, "SELECT pk, i, s, n, g FROM t ORDER BY pk")?,
        vec![
            row(1, 7, None),
            row(10, 2, None),
            row(11, 7, None),
            row(12, 1, None),
            row(13, 7, Some(5)),
        ]
    );
    connection.execute("CREATE TABLE r (a INT NOT NULL, b INT)")?;
    for (statement, expected) in [
        ("INSERT INTO r VALUES (DEFAULT, 1)", "MySQL rejects"),
        (
            "INSERT INTO t (i, g) VALUES (1, 2), (2, DEFAULT)",
            "MySQL rejects",
        ),
        ("INSERT IGNORE INTO r VALUES (DEFAULT, 1)", "unsupported"),
        ("UPDATE r SET a = DEFAULT", "unsupported"),
    ] {
        let error = connection.execute(statement).expect_err(statement);
        assert!(error.to_string().contains(expected), "{statement}: {error}");
    }
    Ok(())
}

#[test]
fn geometries_are_stored_read_and_written_as_in_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE g (i INT PRIMARY KEY, g GEOMETRY NOT NULL)")?;
    connection.execute(
        "INSERT INTO g VALUES (1, ST_GeomFromText('Point(1 2)')), (2, ST_SRID(ST_GeomFromText('Linestring(1 2,3 4)'), 4326)), (3, ST_GeomFromText('MultiPoint(1 2,3 4)')), (4, ST_GeomFromText('GeometryCollection(GeometryCollection())'))",
    )?;
    let text = |row: &[&str]| {
        row.iter()
            .map(|text| Value::build_text(text.to_string()))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        query(
            &connection,
            "SELECT ST_AsWKT(g), HEX(ST_AsWKB(g)), ST_GeometryType(g) FROM g ORDER BY i",
        )?,
        vec![
            text(&[
                "POINT(1 2)",
                "0101000000000000000000F03F0000000000000040",
                "POINT"
            ]),
            text(&[
                "LINESTRING(2 1,4 3)",
                "0102000000020000000000000000000040000000000000F03F00000000000010400000000000000840",
                "LINESTRING"
            ]),
            text(&[
                "MULTIPOINT((1 2),(3 4))",
                "0104000000020000000101000000000000000000F03F0000000000000040010100000000000000000008400000000000001040",
                "MULTIPOINT"
            ]),
            text(&[
                "GEOMETRYCOLLECTION(GEOMETRYCOLLECTION EMPTY)",
                "010700000001000000010700000000000000",
                "GEOMCOLLECTION"
            ]),
        ]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT ST_SRID(g), ST_Dimension(g), ST_IsEmpty(g) FROM g WHERE i < 4 ORDER BY i"
        )?,
        [(0, 0), (4326, 1), (0, 0)].map(|(srid, dimension)| vec![
            Value::from_i64(srid),
            Value::from_i64(dimension),
            Value::from_i64(0),
        ])
    );
    assert_eq!(
        query(
            &connection,
            "SELECT ST_Dimension(g), ST_IsEmpty(GEOMETRYCOLLECTION()) FROM g WHERE i = 4"
        )?,
        vec![vec![Value::Null, Value::from_i64(1)]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT ST_AsWKT(ST_PointN(LINESTRING(POINT(0,0),POINT(1,1)), 2)), ST_NumPoints(POINT(0,0)), ST_AsWKT(ST_Envelope(LINESTRING(POINT(0,0),POINT(3,4)))), ST_AsWKT(ST_GeomFromText('POINT(1 2)', 4326)), ST_X(POINT(1.5, 2))",
        )?,
        vec![vec![
            Value::build_text("POINT(1 1)"),
            Value::Null,
            Value::build_text("POLYGON((0 0,3 0,3 4,0 4,0 0))"),
            Value::build_text("POINT(1 2)"),
            Value::from_f64(1.5),
        ]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT ST_AsGeoJSON(LINESTRING(POINT(1, 2), POINT(3.5, 4))), ST_GeoHash(-87.65, 41.85, 10), ST_LatFromGeoHash('s0000'), ST_AsWKT(ST_PointFromGeoHash('dp3wjztvtc', 0))",
        )?,
        vec![vec![
            Value::build_text(r#"{"type": "LineString", "coordinates": [[1.0, 2.0], [3.5, 4.0]]}"#),
            Value::build_text("dp3wj6x1yv"),
            Value::from_f64(0.0),
            Value::build_text("POINT(-87.62979 41.87809)"),
        ]]
    );
    connection.execute("CREATE TABLE p (p POINT SRID 4326)")?;
    connection.execute("INSERT INTO p VALUES (ST_SRID(POINT(1, 2), 4326))")?;
    for rejected in [
        "INSERT INTO p VALUES (POINT(1, 2))",
        "INSERT INTO p VALUES (ST_SRID(LINESTRING(POINT(0, 0), POINT(1, 1)), 4326))",
        "SELECT ST_GeomFromText('POINT(1)')",
        "SELECT ST_GeomFromText('POINT(100 1)', 4326)",
        "SELECT ST_AsWKT('not a geometry')",
    ] {
        query(&connection, rejected).expect_err(rejected);
    }
    for refused in [
        "SELECT ST_GeomFromText('POINT(1 2)', 3857)",
        "SELECT LINESTRING(POINT(0, 0))",
        "CREATE TABLE q (p POINT SRID 3857)",
    ] {
        let error = query(&connection, refused).expect_err(refused);
        assert!(error.to_string().contains("not "), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn random_bytes_reads_its_length_as_mysql_does() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT LENGTH(RANDOM_BYTES(1.5)), LENGTH(RANDOM_BYTES('1.5')), LENGTH(RANDOM_BYTES(X'40')), RANDOM_BYTES(NULL)",
        )?,
        vec![vec![
            Value::from_i64(2),
            Value::from_i64(1),
            Value::from_i64(64),
            Value::Null,
        ]]
    );
    connection.execute("CREATE TABLE t (i INT)")?;
    connection.execute("INSERT INTO t VALUES (1), (2), (3)")?;
    assert_eq!(
        query(
            &connection,
            "SELECT COUNT(DISTINCT RANDOM_BYTES(16)) FROM t"
        )?,
        vec![vec![Value::from_i64(3)]]
    );
    for out_of_range in ["SELECT RANDOM_BYTES(0)", "SELECT RANDOM_BYTES(1025)"] {
        let error = query(&connection, out_of_range).expect_err(out_of_range);
        assert!(
            error.to_string().contains("ERROR 1690"),
            "{out_of_range}: {error}"
        );
    }
    Ok(())
}

#[test]
fn variances_follow_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (i INT, j DECIMAL(5, 2))")?;
    assert_eq!(
        query(
            &connection,
            "SELECT VARIANCE(i), STD(i), VAR_SAMP(i) FROM t"
        )?,
        vec![vec![Value::Null, Value::Null, Value::Null]]
    );
    connection.execute("INSERT INTO t VALUES (1, 1.5)")?;
    assert_eq!(
        query(&connection, "SELECT VAR_POP(i), STDDEV_SAMP(i) FROM t")?,
        vec![vec![Value::from_f64(0.0), Value::Null]]
    );
    connection.execute("INSERT INTO t VALUES (2, 2.25), (2, NULL), (3, NULL)")?;
    assert_eq!(
        query(
            &connection,
            "SELECT STD(i), STDDEV_POP(i), STDDEV_SAMP(i), VARIANCE(i), VAR_SAMP(i), VARIANCE(j) FROM t",
        )?,
        vec![vec![
            Value::from_f64(std::f64::consts::FRAC_1_SQRT_2),
            Value::from_f64(std::f64::consts::FRAC_1_SQRT_2),
            Value::from_f64(0.816496580927726),
            Value::from_f64(0.5),
            Value::from_f64(0.6666666666666666),
            Value::from_f64(0.140625),
        ]]
    );
    for refused in [
        "SELECT STD(i) OVER () FROM t",
        "SELECT VARIANCE(DISTINCT i) FROM t",
    ] {
        query(&connection, refused).expect_err(refused);
    }
    Ok(())
}

#[test]
fn having_resolves_names_as_mysql_does() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (a INT, b INT)")?;
    connection.execute("INSERT INTO t VALUES (1, 2), (2, 3)")?;
    for (sql, rows) in [
        (
            "SELECT -b AS a FROM t GROUP BY a, b HAVING b > -a ORDER BY 1",
            vec![-3, -2],
        ),
        ("SELECT a AS b FROM t GROUP BY a HAVING AVG(b) > 2", vec![2]),
        ("SELECT a AS b FROM t GROUP BY a HAVING b > 1", vec![2]),
        ("SELECT 1 AS a GROUP BY a HAVING a = 1", vec![1]),
    ] {
        assert_eq!(
            query(&connection, sql)?,
            rows.into_iter()
                .map(|row| vec![Value::from_i64(row)])
                .collect::<Vec<_>>(),
            "{sql}"
        );
    }
    connection.execute("CREATE VIEW v AS SELECT 1 AS k")?;
    let refused = "SELECT 5 AS k FROM v GROUP BY k HAVING k = 1";
    let error = query(&connection, refused).expect_err(refused);
    assert!(
        error.to_string().contains("not lowered"),
        "{refused}: {error}"
    );
    Ok(())
}

#[test]
fn format_rounds_and_groups_digits_as_mysql_does() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT FORMAT(1234567.891, 2), FORMAT(1, 3), FORMAT('12345.5', 0), FORMAT(12.5, 0), FORMAT(12.5e0, 0), FORMAT(-1234.5, 1, 'de_DE'), FORMAT(NULL, 2)",
        )?,
        vec![vec![
            Value::build_text("1,234,567.89"),
            Value::build_text("1.000"),
            Value::build_text("12,346"),
            Value::build_text("13"),
            Value::build_text("12"),
            Value::build_text("-1.234,5"),
            Value::Null,
        ]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT FORMAT(1.25, 1.5), FORMAT(1.25, 2.5), FORMAT(1.25, '1.5'), FORMAT(1, X'02')",
        )?,
        vec![vec![
            Value::build_text("1.25"),
            Value::build_text("1.250"),
            Value::build_text("1.3"),
            Value::build_text("1.00"),
        ]]
    );
    for refused in ["SELECT FORMAT(-0.001, 2)", "SELECT FORMAT(1, 2, 'fr_FR')"] {
        let error = query(&connection, refused).expect_err(refused);
        assert!(
            error.to_string().contains("not supported"),
            "{refused}: {error}"
        );
    }
    Ok(())
}

#[test]
fn grouped_selects_use_columns_as_only_full_group_by_allows() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE p (id INT PRIMARY KEY, nm VARCHAR(10), r INT)")?;
    connection.execute("CREATE TABLE c (id INT PRIMARY KEY, p_id INT)")?;
    connection.execute("INSERT INTO p VALUES (1, 'a', 5), (2, 'b', 6)")?;
    connection.execute("INSERT INTO c VALUES (1, 1), (2, 1), (3, 2)")?;
    assert_eq!(
        query(
            &connection,
            "SELECT p.nm, COUNT(c.id) FROM p JOIN c ON c.p_id = p.id GROUP BY p.id ORDER BY p.id",
        )?,
        vec![
            vec![Value::build_text("a"), Value::from_i64(2)],
            vec![Value::build_text("b"), Value::from_i64(1)],
        ]
    );
    assert_eq!(
        query(&connection, "SELECT nm, COUNT(*) FROM p WHERE id = 2")?,
        vec![vec![Value::build_text("b"), Value::from_i64(1)]]
    );
    for statement in [
        "SELECT nm, COUNT(*) FROM p GROUP BY r",
        "SELECT * FROM c GROUP BY p_id",
        "SELECT nm, SUM(r) FROM p",
    ] {
        let error = query(&connection, statement).expect_err(statement);
        assert!(
            error.to_string().contains("MySQL rejects"),
            "{statement}: {error}"
        );
    }
    let error = query(
        &connection,
        "SELECT nm FROM p, (SELECT 1 AS x) AS d GROUP BY x",
    )
    .expect_err("a derived table's columns are not resolved");
    assert!(error.to_string().contains("unsupported"), "{error}");
    Ok(())
}

#[test]
fn create_index_checks_the_types_of_the_columns() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (i INT, s TEXT, j JSON, v VARCHAR(10))")?;
    for statement in ["CREATE INDEX x ON t (j)", "CREATE INDEX x ON t (i, s)"] {
        let error = connection.execute(statement).expect_err(statement);
        assert!(
            error.to_string().contains("MySQL rejects"),
            "{statement}: {error}"
        );
    }
    connection.execute("CREATE INDEX x ON t (i, v)")?;
    Ok(())
}

#[test]
fn triggers_fire_as_mysql_s_or_are_refused() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for statement in [
        "CREATE TABLE a (id INT AUTO_INCREMENT PRIMARY KEY, x INT)",
        "CREATE TABLE b (y INT PRIMARY KEY)",
        "CREATE TABLE c (pid INT, FOREIGN KEY (pid) REFERENCES b (y) ON DELETE CASCADE)",
        "CREATE TABLE log (z INT)",
        "CREATE TRIGGER a1 AFTER INSERT ON a FOR EACH ROW INSERT INTO b VALUES (NEW.x * 7)",
        "CREATE TRIGGER a2 BEFORE INSERT ON a FOR EACH ROW INSERT INTO log VALUES (NEW.x)",
        "INSERT INTO a (x) VALUES (1), (2)",
    ] {
        connection.execute(statement)?;
    }
    assert_eq!(
        query(&connection, "SELECT y FROM b ORDER BY y")?,
        vec![vec![Value::from_i64(7)], vec![Value::from_i64(14)]]
    );
    connection
        .execute("INSERT INTO a (x) VALUES (3), (1)")
        .expect_err("the trigger's duplicate key fails the statement");
    assert_eq!(
        query(&connection, "SELECT count(*) FROM a")?,
        vec![vec![Value::from_i64(2)]]
    );
    for refused in [
        "CREATE TRIGGER a3 AFTER INSERT ON a FOR EACH ROW INSERT INTO log VALUES (1)",
        "CREATE TRIGGER a3 BEFORE INSERT ON a FOR EACH ROW FOLLOWS a2 INSERT INTO log VALUES (1)",
        "INSERT INTO a (x) SELECT y FROM b",
        "ALTER TABLE log ADD COLUMN w INT",
    ] {
        connection.execute(refused).expect_err(refused);
    }
    connection.execute(
        "CREATE TRIGGER c1 AFTER DELETE ON c FOR EACH ROW INSERT INTO log VALUES (OLD.pid)",
    )?;
    connection.execute("INSERT INTO c VALUES (7)")?;
    let error = connection
        .execute("DELETE FROM b WHERE y = 7")
        .expect_err("MySQL does not fire c1 for the cascade");
    assert!(error.to_string().contains("foreign key action"), "{error}");
    connection.execute("DROP TRIGGER c1")?;
    connection.execute("DELETE FROM b WHERE y = 7")?;
    assert_eq!(
        query(&connection, "SELECT count(*) FROM c")?,
        vec![vec![Value::from_i64(0)]]
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
    assert_eq!(
        query(&connection, "SELECT id FROM t WHERE id < 4 ORDER BY e, id")?,
        vec![
            vec![Value::from_i64(1)],
            vec![Value::from_i64(3)],
            vec![Value::from_i64(2)],
        ]
    );
    assert_eq!(
        query(&connection, "SELECT id FROM t ORDER BY s DESC, id")?,
        [2, 1, 3, 5, 4].map(|id| vec![Value::from_i64(id)])
    );
    for refused in [
        "SELECT CAST(e AS DECIMAL) FROM t",
        "SELECT CAST(s AS DOUBLE) FROM t",
    ] {
        let error = query(&connection, refused).expect_err(refused);
        assert!(
            error.to_string().contains("not supported"),
            "{refused}: {error}"
        );
    }
    connection.execute("CREATE TABLE animals (e ENUM('rat', 'ox', 'dog') PRIMARY KEY)")?;
    connection.execute(
        "CREATE TABLE pets (e ENUM('cat', 'dog', 'rat'), FOREIGN KEY (e) REFERENCES animals (e))",
    )?;
    connection.execute("INSERT INTO animals VALUES ('rat')")?;
    connection.execute("INSERT INTO pets VALUES ('cat')")?;
    connection
        .execute("INSERT INTO pets VALUES ('rat')")
        .expect_err("MySQL matches ENUM keys by member position");
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
fn prepared_statements_run_with_user_variables() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (i INT, j VARCHAR(10))")?;
    connection.execute("SET @a = 1, @b = 'x', @n = 1, @q = 'SELECT COUNT(*) FROM t'")?;
    connection.execute("PREPARE ins FROM 'INSERT INTO t VALUES (?, ?)'")?;
    connection.execute("EXECUTE ins USING @a, @b")?;
    connection.execute("EXECUTE INS USING @n, @nope")?;
    assert_eq!(
        query(&connection, "SELECT i, j FROM t ORDER BY j")?,
        vec![
            vec![Value::from_i64(1), Value::Null],
            vec![Value::from_i64(1), Value::build_text("x")],
        ]
    );
    connection.execute("PREPARE s FROM @q")?;
    let mut execute = connection.prepare("EXECUTE s")?;
    assert_eq!(execute.run_collect_rows()?, vec![vec![Value::from_i64(2)]]);
    connection.execute("PREPARE s FROM 'SELECT j FROM t ORDER BY j LIMIT ?'")?;
    connection.execute("PREPARE s FROM 'SELECT j FROM t ORDER BY j DESC LIMIT 1'")?;
    execute.reset()?;
    assert_eq!(
        execute.run_collect_rows()?,
        vec![vec![Value::build_text("x")]]
    );
    connection.execute("DEALLOCATE PREPARE s")?;
    for (failing, error) in [
        ("EXECUTE s", "ERROR 1243"),
        ("DROP PREPARE s", "ERROR 1243"),
        ("EXECUTE ins USING @a", "ERROR 1210"),
        ("PREPARE s FROM 'SELECT k FROM t'", "no such column"),
        ("PREPARE s FROM 'SELECT * FROM nope'", "no such table"),
        (
            "PREPARE s FROM 'SELECT 1; SELECT 2'",
            "expected one MySQL statement",
        ),
        ("PREPARE s FROM 'EXECUTE ins'", "ERROR 1295"),
        ("PREPARE s FROM @nope", "ERROR 1064"),
    ] {
        let message = connection.execute(failing).expect_err(failing).to_string();
        assert!(message.contains(error), "{failing}: {message}");
    }
    connection.execute("SET @n = '1'")?;
    connection.execute("PREPARE s FROM 'SELECT j FROM t LIMIT ?'")?;
    let message = connection
        .execute("EXECUTE s USING @n")
        .expect_err("a string LIMIT")
        .to_string();
    assert!(message.contains("not supported yet"), "{message}");
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
fn recursive_common_table_expressions_stop_before_mysql_limit() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "WITH RECURSIVE c (n, p) AS (SELECT 1, 1 UNION ALL SELECT n + 1, p * 10 FROM c WHERE n < 3) \
             SELECT * FROM c"
        )?,
        vec![
            vec![Value::from_i64(1), Value::from_i64(1)],
            vec![Value::from_i64(2), Value::from_i64(10)],
            vec![Value::from_i64(3), Value::from_i64(100)],
        ]
    );
    connection.execute("CREATE TABLE e (id INT PRIMARY KEY, boss INT)")?;
    connection.execute("INSERT INTO e VALUES (1, NULL), (2, 1), (3, 2)")?;
    assert_eq!(
        query(
            &connection,
            "WITH RECURSIVE chain AS (SELECT 1 AS id, 0 AS lvl UNION ALL \
             SELECT e.id, chain.lvl + 1 FROM e JOIN chain ON e.boss = chain.id) \
             SELECT id, lvl FROM chain ORDER BY id"
        )?,
        vec![
            vec![Value::from_i64(1), Value::from_i64(0)],
            vec![Value::from_i64(2), Value::from_i64(1)],
            vec![Value::from_i64(3), Value::from_i64(2)],
        ]
    );
    connection.execute("CREATE TABLE named (id INT PRIMARY KEY, boss INT, name VARCHAR(10))")?;
    connection.execute("INSERT INTO named VALUES (1, NULL, 'a'), (2, 1, 'b'), (3, 2, 'c')")?;
    assert_eq!(
        query(
            &connection,
            "WITH RECURSIVE chain AS (SELECT id, name FROM named WHERE boss IS NULL UNION ALL              SELECT n.id + 0, n.name FROM named n JOIN chain ON n.boss = chain.id)              SELECT * FROM chain ORDER BY id"
        )?,
        vec![
            vec![Value::from_i64(1), Value::build_text("a")],
            vec![Value::from_i64(2), Value::build_text("b")],
            vec![Value::from_i64(3), Value::build_text("c")],
        ]
    );
    for refused in [
        "WITH RECURSIVE c (s) AS (SELECT name FROM named WHERE id = 1 UNION ALL          SELECT CONCAT(s, 'a') FROM c WHERE LENGTH(s) < 3) SELECT * FROM c",
        "WITH RECURSIVE c (s) AS (SELECT name FROM named WHERE id = 1 UNION ALL          SELECT e.boss FROM c JOIN e ON e.id = 2 WHERE s = 'a') SELECT * FROM c",
        "WITH RECURSIVE c (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 2000) SELECT COUNT(*) FROM c",
        "WITH RECURSIVE c (s) AS (SELECT 'a' UNION ALL SELECT CONCAT(s, 'a') FROM c WHERE LENGTH(s) < 3) SELECT * FROM c",
        "WITH RECURSIVE c (n) AS (SELECT 1 UNION ALL SELECT n * 100000 FROM c WHERE n < 1000000000) SELECT * FROM c",
        "SELECT (WITH RECURSIVE c (n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < e.id) SELECT MAX(n) FROM c) FROM e",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(error.contains("not"), "{refused}: {error}");
        assert!(!error.contains("ERROR "), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn having_without_grouping_filters_rows_by_alias() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE uv (u INT, v INT)")?;
    connection.execute("INSERT INTO uv VALUES (1, 2), (0, 3), (5, 1)")?;
    assert_eq!(
        query(
            &connection,
            "SELECT u + 1 AS w FROM uv WHERE v > 1 HAVING w > 1 ORDER BY w"
        )?,
        vec![vec![Value::from_i64(2)]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT u, (SELECT v HAVING u > 3) FROM uv ORDER BY u"
        )?,
        vec![
            vec![Value::from_i64(0), Value::Null],
            vec![Value::from_i64(1), Value::Null],
            vec![Value::from_i64(5), Value::from_i64(1)],
        ]
    );
    assert!(query(&connection, "SELECT 1 HAVING FALSE")?.is_empty());
    for refused in [
        "SELECT u FROM uv HAVING v = 1",
        "SELECT u, ROW_NUMBER() OVER () AS r FROM uv HAVING r = 1",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(error.contains("not lowered yet"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn empty_regular_expressions_fail_when_evaluated() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE e (s TEXT)")?;
    assert!(query(&connection, "SELECT s REGEXP '' FROM e")?.is_empty());
    for sql in ["SELECT 'foo' REGEXP ''", "SELECT 1 NOT REGEXP ''"] {
        let error = query(&connection, sql).expect_err(sql).to_string();
        assert!(error.contains("ERROR 3685 "), "{sql}: {error}");
    }
    assert_eq!(
        query(&connection, "SELECT 0 AND ('a' REGEXP '')")?,
        vec![vec![Value::from_i64(0)]]
    );
    let error = query(&connection, "SELECT 1 + ('a' REGEXP '')")
        .expect_err("REGEXP '' in arithmetic")
        .to_string();
    assert!(error.contains("ERROR 3685 "), "{error}");
    for refused in ["SELECT NULL REGEXP ''", "SELECT 0xFF REGEXP ''"] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(error.contains("not"), "{refused}: {error}");
        assert!(!error.contains("ERROR "), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn group_by_a_name_of_a_column_and_an_alias_groups_by_the_column() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE g (a INT, b INT)")?;
    connection.execute("INSERT INTO g VALUES (1, 300), (2, 50)")?;
    let sql = "SELECT b - 100 AS a FROM g GROUP BY a";
    let error = query(&connection, sql).expect_err(sql).to_string();
    assert!(error.contains("nonaggregated column"), "{sql}: {error}");
    assert_eq!(
        query(
            &connection,
            "SELECT b - 100 AS x FROM g GROUP BY x ORDER BY x"
        )?,
        vec![vec![Value::from_i64(-50)], vec![Value::from_i64(200)]]
    );
    Ok(())
}

#[test]
fn derived_tables_reject_a_repeated_column_name() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE m (i INT, s TEXT)")?;
    for sql in [
        "SELECT i FROM (SELECT * FROM m a JOIN m b ON a.i = b.i) dt",
        "SELECT * FROM (SELECT 1 AS a, 2 AS A) t",
        "SELECT * FROM (SELECT 1, 2) t (x, x)",
        "WITH c AS (SELECT a.i, b.i FROM m a JOIN m b) SELECT * FROM c",
        "CREATE VIEW v AS SELECT * FROM m a JOIN m b",
    ] {
        let error = query(&connection, sql).expect_err(sql).to_string();
        assert!(error.contains("ERROR 1060 "), "{sql}: {error}");
    }
    assert!(query(
        &connection,
        "SELECT * FROM (SELECT a.i, b.s FROM m a JOIN m b) t"
    )?
    .is_empty());
    Ok(())
}

#[test]
fn weight_strings_of_literals_are_their_sort_bytes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, expected) in [
        (
            "SELECT HEX(WEIGHT_STRING(1))",
            Value::build_text("8000000000000001"),
        ),
        (
            "SELECT HEX(WEIGHT_STRING(-1))",
            Value::build_text("7FFFFFFFFFFFFFFF"),
        ),
        (
            "SELECT HEX(WEIGHT_STRING(18446744073709551615 AS CHAR(3)))",
            Value::build_text("FFFFFFFFFFFFFFFF"),
        ),
        ("SELECT WEIGHT_STRING(1.5)", Value::Null),
        (
            "SELECT HEX(WEIGHT_STRING(_latin1 'ab1'))",
            Value::build_text("414231"),
        ),
        (
            "SELECT HEX(WEIGHT_STRING(-12 AS BINARY(2)))",
            Value::build_text("2D31"),
        ),
        (
            "SELECT HEX(WEIGHT_STRING(DATE '2000-01-01' AS BINARY(4)))",
            Value::build_text("32303030"),
        ),
    ] {
        assert_eq!(query(&connection, sql)?, vec![vec![expected]], "{sql}");
    }
    for refused in [
        "SELECT WEIGHT_STRING('a')",
        "SELECT WEIGHT_STRING('ab' AS BINARY(4))",
        "SELECT WEIGHT_STRING(DATE '2000-1-1' AS BINARY(2))",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(error.contains("not lowered yet"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn counts_compare_with_strings_as_numbers() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE c (s TEXT)")?;
    connection.execute("INSERT INTO c VALUES ('1'), ('22')")?;
    assert_eq!(
        query(&connection, "SELECT COUNT(*) AS a FROM c HAVING a = '2'")?,
        vec![vec![Value::from_i64(2)]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT COUNT(*) < '10abc', LENGTH(MAX(s)) = '2' FROM c"
        )?,
        vec![vec![Value::from_i64(1), Value::from_i64(1)]]
    );
    Ok(())
}

#[test]
fn time_literals_compare_with_binary_strings_as_bytes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT TIME '-31:34:58' > X'00', TIME '-31:34:58' > 0xFF, X'FF' <= TIME '10:04:58',              TIME '32:34:58.5' <= _binary 'foobar', _utf8 'foobar' > TIME '-10:04:58',              TIME '-101:34:58' <= b'1001', TIME '10:04:58' = 0x31303A30343A3538"
        )?,
        vec![[1, 0, 0, 1, 1, 0, 1].map(Value::from_i64).to_vec()]
    );
    Ok(())
}

#[test]
fn negated_doubles_below_bigint_fail_as_bit_operands() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for failing in [
        "SELECT -1.7 ^ -18446744073709540000e0",
        "SELECT ~ -18446744073709540000e0",
        "SELECT -18446744073709540000e0 | NULL",
        "SELECT CHAR(1, 7e0, -18446744073709540000e0)",
    ] {
        let error = query(&connection, failing).expect_err(failing).to_string();
        assert!(
            error.contains(
                "ERROR 1690 (22003): BIGINT value is out of range in '-(18446744073709540000e0)'"
            ),
            "{failing}: {error}"
        );
    }
    for skipped in [
        "SELECT NULL | -18446744073709540000e0",
        "SELECT NULL = (1 | -18446744073709540000e0)",
        "SELECT NULL = ('a' REGEXP '')",
    ] {
        assert_eq!(
            query(&connection, skipped)?,
            vec![vec![Value::Null]],
            "{skipped}"
        );
    }
    let failing = "SELECT NULL + 1 - CAST('invalid' AS JSON)";
    let error = query(&connection, failing).expect_err(failing).to_string();
    assert!(error.contains("ERROR 3141"), "{failing}: {error}");
    assert_eq!(
        query(
            &connection,
            "SELECT 0 AND CAST('x' AS JSON), '2000-01-01 x' OR CAST('x' AS JSON), \
             TIME '00:00:00' AND CAST('x' AS JSON), NULL OR 1 OR CAST('x' AS JSON), \
             0x01 || CAST('x' AS JSON), 0 AND (1 | -18446744073709540000e0)"
        )?,
        vec![[0, 1, 0, 1, 1, 0].map(Value::from_i64).to_vec()]
    );
    for failing in [
        "SELECT 'foo' OR CAST('x' AS JSON)",
        "SELECT TIME '130:34:58.6' AND CAST('x' AS JSON)",
        "SELECT X'00' || CAST('x' AS JSON)",
    ] {
        let error = query(&connection, failing).expect_err(failing).to_string();
        assert!(error.contains("ERROR 3141"), "{failing}: {error}");
    }
    Ok(())
}

#[test]
fn temporal_results_are_numbers_in_arithmetic() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE tt (d DATE, dt DATETIME)")?;
    connection.execute("INSERT INTO tt VALUES ('2000-01-02', '2000-01-02 03:04:05')")?;
    assert_eq!(
        query(
            &connection,
            "SELECT SEC_TO_TIME(2378) + 0, MAKETIME(1, 2, 3) + 0, SEC_TO_TIME(-3600) * 1, \
             DATE(dt) + 0, TIME(dt) + 0, FROM_UNIXTIME(0) + 0 > 19700000000000, \
             SEC_TO_TIME(1) < 2, DATE(dt) < 20000103 FROM tt"
        )?,
        vec![vec![
            Value::from_i64(3938),
            Value::from_i64(10203),
            Value::from_i64(-10000),
            Value::from_i64(20000102),
            Value::from_i64(30405),
            Value::from_i64(1),
            Value::from_i64(1),
            Value::from_i64(1),
        ]]
    );
    for refused in [
        "SELECT d + 0 FROM tt",
        "SELECT -dt FROM tt",
        "SELECT SEC_TO_TIME(1.5) + 0",
        "SELECT DATE_ADD(SEC_TO_TIME(1), INTERVAL 1 SECOND)",
    ] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(error.contains("not"), "{refused}: {error}");
        assert!(!error.contains("ERROR "), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn greatest_and_least_of_temporal_literals_compare_as_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT GREATEST(DATE '2024-02-18', DATE '2100-02-01', \
             TIMESTAMP '2025-01-01 00:00:00.123456'), \
             LEAST(TIME '23:59:59.5432', TIME '120:59:59'), \
             GREATEST(TIME '23:59:59.5432', TIME '120:59:59'), \
             LEAST(TIME '-1:00:00', TIME '00:00:01.5'), \
             GREATEST(DATE '2000-01-01', DATE '1999-12-31'), GREATEST(TIME '1:00:00', NULL)"
        )?,
        vec![vec![
            Value::build_text("2100-02-01 00:00:00.000000"),
            Value::build_text("23:59:59.5432"),
            Value::build_text("120:59:59.0000"),
            Value::build_text("-01:00:00.0"),
            Value::build_text("2000-01-01"),
            Value::Null,
        ]]
    );
    let refused = "SELECT LEAST(TIME '23:59:59', TIMESTAMP '2020-12-31 23:59:59')";
    let error = query(&connection, refused).expect_err(refused).to_string();
    assert!(error.contains("not lowered yet"), "{error}");
    Ok(())
}

#[test]
fn text_functions_read_unsigned_bigint_literals_as_digits() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT HEX(FROM_BASE64(18446744073709551615)), TO_BASE64(10000000000000000000), \
             HEX(18446744073709551615), INET_NTOA(0x0A000509), INET_NTOA(b'1010'), \
             HEX(CHAR(-1, '64', CAST('18446744073709551615' AS JSON), CAST(-1.1 AS JSON)))"
        )?,
        vec![vec![
            Value::build_text("D7CE38EBBE38D3BDFBD3DE79D7AD79"),
            Value::build_text("MTAwMDAwMDAwMDAwMDAwMDAwMDA="),
            Value::build_text("FFFFFFFFFFFFFFFF"),
            Value::build_text("10.0.5.9"),
            Value::build_text("0.0.0.10"),
            Value::build_text("FFFFFFFF40FFFFFFFFFFFFFFFF"),
        ]]
    );
    Ok(())
}

#[test]
fn bit_operations_read_binary_strings_as_numbers_next_to_numbers() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT _binary 'foobar' ^ 1.5, '1.5' >> _binary 'foobar', _binary '12' & 7, \
             -1.5 | _binary 'foobar1234'"
        )?,
        vec![vec![
            Value::from_i64(2),
            Value::from_i64(1),
            Value::from_i64(4),
            Value::build_text("18446744073709551614"),
        ]]
    );
    for refused in [
        "SELECT x'09' ^ _binary 'foobar'",
        "SELECT ~_binary 'ab'",
        "SELECT _binary 'a' << 1",
    ] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(error.contains("not lowered yet"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn alter_table_changes_columns() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE ac (a DOUBLE, b VARCHAR(10), k INT PRIMARY KEY)")?;
    connection.execute("INSERT INTO ac VALUES (1.5, 'abc', 1), (2.5, NULL, 2)")?;
    connection.execute("ALTER TABLE ac CHANGE a n INT DEFAULT 5")?;
    connection.execute("INSERT INTO ac (b, k) VALUES ('x', 3)")?;
    assert_eq!(
        query(&connection, "SELECT n FROM ac ORDER BY k")?,
        vec![
            vec![Value::from_i64(2)],
            vec![Value::from_i64(3)],
            vec![Value::from_i64(5)],
        ]
    );
    for failing in [
        "ALTER TABLE ac CHANGE b c INT",
        "ALTER TABLE ac CHANGE b c VARCHAR(10) NOT NULL",
        "ALTER TABLE ac CHANGE b c VARCHAR(2)",
    ] {
        assert!(connection.execute(failing).is_err(), "{failing}");
    }
    for refused in [
        "ALTER TABLE ac MODIFY b VARCHAR(20)",
        "ALTER TABLE ac CHANGE b b VARCHAR(20)",
        "ALTER TABLE ac CHANGE k j BIGINT",
        "ALTER TABLE ac CHANGE b c VARCHAR(20) UNIQUE",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(error.contains("not available"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn alter_table_adds_indexes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE ai (a INT, b INT, t TEXT)")?;
    connection.execute("CREATE TABLE aj (a INT)")?;
    connection.execute("ALTER TABLE ai ADD INDEX ia (a)")?;
    connection.execute("ALTER TABLE ai ADD UNIQUE KEY ub (b)")?;
    connection.execute("ALTER TABLE ai ADD INDEX (a, b)")?;
    connection.execute("INSERT INTO ai VALUES (1, 1, 'x')")?;
    assert!(connection
        .execute("INSERT INTO ai VALUES (2, 1, 'y')")
        .is_err());
    connection.execute("ALTER TABLE ai DROP INDEX ub")?;
    connection.execute("INSERT INTO ai VALUES (2, 1, 'y')")?;
    for refused in [
        "ALTER TABLE ai ADD INDEX it (t)",
        "ALTER TABLE ai ADD UNIQUE (a)",
        "ALTER TABLE ai ADD INDEX (a)",
        "ALTER TABLE aj ADD INDEX ia (a)",
    ] {
        assert!(connection.execute(refused).is_err(), "{refused}");
    }
    connection.execute("CREATE TABLE ak (c INT, d INT UNIQUE)")?;
    connection.execute("ALTER TABLE ak ADD UNIQUE (c)")?;
    assert!(connection
        .execute("INSERT INTO ak VALUES (1, 1), (1, 2)")
        .is_err());
    for refused in [
        "ALTER TABLE ak ADD INDEX (d)",
        "ALTER TABLE ak ADD UNIQUE (c)",
        "CREATE INDEX d ON ak (c)",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(
            error.contains("unsupported MySQL syntax"),
            "{refused}: {error}"
        );
    }
    Ok(())
}

#[test]
fn strcmp_with_a_number_compares_bytes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE sn (i INT, f DOUBLE, s VARCHAR(10))")?;
    connection.execute("INSERT INTO sn VALUES (103, 103.5, '10:')")?;
    assert_eq!(
        query(
            &connection,
            "SELECT STRCMP(TIME '10:04:58', 103458), STRCMP(103458, TIME '10:04:58'), \
             STRCMP('10:04:58', 103458), STRCMP(103458.123456, TIME '10:04:58.1'), \
             STRCMP('10:' COLLATE utf8mb4_0900_ai_ci, 103.5), STRCMP(s, i), STRCMP(s, f), \
             STRCMP(s, '103'), STRCMP(1e0, '1') FROM sn"
        )?,
        vec![[1, -1, 1, -1, 1, 1, 1, -1, 0].map(Value::from_i64).to_vec()]
    );
    Ok(())
}

#[test]
fn modulus_and_integer_division_read_binary_strings_as_numbers() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT _binary '7.5' DIV 2, _binary '64' DIV _binary 'foobar', _binary X'40' MOD 13e0, \
             -1 MOD _binary '64', _binary '7.5' MOD 2"
        )?,
        vec![vec![
            Value::from_i64(3),
            Value::Null,
            Value::from_f64(0.0),
            Value::from_i64(-1),
            Value::from_f64(1.5),
        ]]
    );
    for refused in ["SELECT _binary ' 7' DIV 2", "SELECT _binary '\t7' MOD 2"] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(
            error.contains("binary string as an operand"),
            "{refused}: {error}"
        );
    }
    Ok(())
}

#[test]
fn integer_division_of_hex_literals_folds_exact_decimals() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT 0xFF666F6F626172FF DIV TIMESTAMP '2024-12-30 10:34:58', \
             0xFF666F6F626172FF DIV '20000101103458.123456', X'ff' DIV TIME '00:00:59.2132234', \
             TIME '-5 10:34:58' DIV 0b1001, -1.1e0 DIV 0xFF666F6F626172, 0x10 DIV 0.1e0, \
             TIME '10:04:58.1' DIV X'00'"
        )?,
        vec![vec![
            Value::from_i64(909209),
            Value::from_i64(920171),
            Value::from_i64(4),
            Value::from_i64(-144828),
            Value::from_i64(0),
            Value::from_i64(160),
            Value::Null,
        ]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT 0xff DIV CAST(true AS JSON), X'01' DIV CAST('1.1' AS JSON), \
             CAST(-20 AS JSON) DIV 0b11, CAST(-2 AS JSON) DIV 0x3, x'09' DIV CAST('{}' AS JSON)"
        )?,
        vec![vec![
            Value::from_i64(255),
            Value::from_i64(0),
            Value::from_i64(-6),
            Value::from_i64(0),
            Value::Null,
        ]]
    );
    let failing = "SELECT CAST(-20 AS JSON) DIV 0x3";
    let error = query(&connection, failing).expect_err(failing).to_string();
    assert!(error.contains("ERROR 1690"), "{failing}: {error}");
    for refused in ["SELECT -1.5e0 DIV 0x1", "SELECT TIME '-5 10:34:58' DIV 0x1"] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(error.contains("BIGINT UNSIGNED"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn integer_arithmetic_of_literals_outside_its_range_fails_as_in_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, kind) in [
        ("SELECT 0x0 - 0x1", "BIGINT UNSIGNED"),
        ("SELECT 0xffffffffffffffff + 1", "BIGINT UNSIGNED"),
        ("SELECT TIME '-101:34:58' * X'01'", "BIGINT UNSIGNED"),
        ("SELECT 9223372036854775807 + 1", "BIGINT value"),
        ("SELECT CONCAT(-9223372036854775808 - 1)", "BIGINT value"),
    ] {
        let error = query(&connection, sql).expect_err(sql).to_string();
        assert!(
            error.contains("ERROR 1690") && error.contains(kind),
            "{sql}: {error}"
        );
    }
    assert_eq!(
        query(
            &connection,
            "SELECT TIME '-101:34:58' - b'1001', 0b1001 * -3, 0 AND (0x0 - 0x1), 0x10 - 0x1"
        )?,
        vec![[-1013467, -27, 0, 15].map(Value::from_i64).to_vec()]
    );
    Ok(())
}

#[test]
fn negated_fractional_temporal_literals_show_mysql_doubles() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, shown) in [
        ("SELECT -TIME '10:04:58.1'", "-100458.1"),
        ("SELECT -TIME '5 10:34:58.9'", "-1303458.9"),
        (
            "SELECT -TIMESTAMP '2000-01-01 10:34:58.123456'",
            "-20000101103458.125000",
        ),
        (
            "SELECT -TIMESTAMP '2000-01-01 10:34:58.978654'",
            "-20000101103458.980000",
        ),
        ("SELECT -TIME '00:00:01.0000'", "-1.0000"),
        ("SELECT -TIME '-10:00:00.5'", "100000.5"),
    ] {
        assert_eq!(
            query(&connection, sql)?,
            vec![vec![Value::build_text(shown)]],
            "{sql}"
        );
    }
    Ok(())
}

#[test]
fn selected_unsigned_casts_show_values_above_bigint() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT CAST(-3 AS UNSIGNED), CONVERT(TIME '-31:34:58', UNSIGNED),              CAST(-1.5e0 AS UNSIGNED INTEGER), CONVERT(-1.7, UNSIGNED),              CAST(CAST(18446744073709551615 AS JSON) AS UNSIGNED), CAST(5 AS UNSIGNED)"
        )?,
        vec![vec![
            Value::build_text("18446744073709551613"),
            Value::build_text("18446744073709238158"),
            Value::build_text("18446744073709551614"),
            Value::build_text("18446744073709551614"),
            Value::build_text("18446744073709551615"),
            Value::from_i64(5),
        ]]
    );
    let failing = "SELECT CAST(-3 AS UNSIGNED) + 1";
    let error = query(&connection, failing).expect_err(failing).to_string();
    assert!(error.contains("not supported yet"), "{error}");
    Ok(())
}

#[test]
fn integer_division_of_literals_beyond_bigint_folds_exactly() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT 18446744073709551615 DIV '103458.123456', \
             18446744073709540000e0 DIV -18446744073709540000e0, \
             -9223372036854775808 DIV CAST(2 AS JSON), \
             CAST('1' AS JSON) DIV 18446744073709551615, \
             18446744073709551615 DIV 0.0"
        )?,
        vec![vec![
            Value::from_i64(178301552913385),
            Value::from_i64(-1),
            Value::from_i64(-4611686018427387904),
            Value::from_i64(0),
            Value::Null,
        ]]
    );
    let failing = "SELECT 18446744073709551615 DIV CAST(-1 AS JSON)";
    let error = query(&connection, failing).expect_err(failing).to_string();
    assert!(
        error.contains("ERROR 1690 (22003): BIGINT UNSIGNED value is out of range"),
        "{error}"
    );
    Ok(())
}

#[test]
fn show_variables_shows_one_known_system_variable() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(&connection, "SHOW SESSION VARIABLES LIKE 'SQL_MODE'")?,
        vec![vec![
            Value::build_text("sql_mode"),
            Value::build_text(
                "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,\
                 ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION"
            ),
        ]]
    );
    assert_eq!(
        query(
            &connection,
            "SHOW VARIABLES WHERE Variable_name = 'character_set_connection'"
        )?,
        vec![vec![
            Value::build_text("character_set_connection"),
            Value::build_text("utf8mb4"),
        ]]
    );
    assert_eq!(
        query(&connection, "SHOW VARIABLES LIKE 'autocommit'")?,
        vec![vec![
            Value::build_text("autocommit"),
            Value::build_text("ON")
        ]]
    );
    for refused in [
        "SHOW VARIABLES LIKE 'char%'",
        "SHOW GLOBAL VARIABLES LIKE 'sql_mode'",
        "SHOW VARIABLES LIKE 'innodb_page_size'",
        "SHOW VARIABLES",
    ] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(
            error.contains("unsupported MySQL syntax"),
            "{refused}: {error}"
        );
    }
    Ok(())
}

#[test]
fn uuids_are_new_version_1_uuids_also_as_defaults() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT IS_UUID(UUID()), LENGTH(UUID()), SUBSTRING(UUID(), 15, 1), UUID() = UUID()"
        )?,
        vec![vec![
            Value::from_i64(1),
            Value::from_i64(36),
            Value::build_text("1"),
            Value::from_i64(0),
        ]]
    );
    connection.execute("CREATE TABLE t (pk VARCHAR(36) DEFAULT (UUID()) PRIMARY KEY, v INT)")?;
    connection.execute("INSERT INTO t (v) VALUES (1), (2), (3)")?;
    assert_eq!(
        query(
            &connection,
            "SELECT COUNT(DISTINCT pk), MIN(IS_UUID(pk)) FROM t"
        )?,
        vec![vec![Value::from_i64(3), Value::from_i64(1)]]
    );
    Ok(())
}

#[test]
fn calls_fail_as_calls_of_missing_procedures() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let error = query(&connection, "CALL p(1, 2)")
        .expect_err("CALL")
        .to_string();
    assert!(error.contains("PROCEDURE p does not exist"), "{error}");
    let error = query(&connection, "CALL sys.ps_setup_show_enabled()")
        .expect_err("CALL")
        .to_string();
    assert!(error.contains("unsupported MySQL syntax"), "{error}");
    Ok(())
}

#[test]
fn decimal_literal_keys_of_json_objects_are_their_text() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT JSON_OBJECT(10.1, 10), JSON_OBJECT(1.0, JSON_OBJECT()), \
             JSON_OBJECT(01.50, 1, .5, 2, 3., 3)"
        )?,
        vec![vec![
            Value::build_text("{\"10.1\": 10}"),
            Value::build_text("{\"1.0\": {}}"),
            Value::build_text("{\"3\": 3, \"0.5\": 2, \"1.50\": 1}"),
        ]]
    );
    Ok(())
}

#[test]
fn shown_conversions_to_ascii_replace_other_characters() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, nm VARCHAR(20), b VARBINARY(5))")?;
    connection
        .execute("INSERT INTO t VALUES (1, 'h\u{e9}llo', _binary X'C3A941'), (2, NULL, 'Ab')")?;
    assert_eq!(
        query(
            &connection,
            "SELECT CONVERT(nm USING ascii), CAST(HEX(nm) AS CHAR CHARACTER SET ascii), \
             CAST(nm AS CHAR(2) CHARSET ascii), CAST(nm AS CHAR CHARACTER SET utf8mb4) \
             FROM t WHERE id = 1"
        )?,
        vec![vec![
            Value::build_text("h?llo"),
            Value::build_text("68C3A96C6C6F"),
            Value::build_text("h?"),
            Value::build_text("h\u{e9}llo"),
        ]]
    );
    assert_eq!(
        query(
            &connection,
            "SELECT CONVERT(nm USING ascii), CONVERT(b USING ascii) FROM t WHERE id = 2"
        )?,
        vec![vec![Value::Null, Value::build_text("Ab")]]
    );
    for refused in [
        "SELECT CONVERT(nm USING ascii) FROM t ORDER BY 1",
        "SELECT CONVERT(nm USING ascii) = 'h?llo' FROM t",
        "SELECT DISTINCT CAST(nm AS CHAR CHARACTER SET ascii) FROM t",
        "SELECT CAST(nm AS CHAR CHARACTER SET latin1) FROM t",
    ] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(error.contains("is not lowered yet"), "{refused}: {error}");
    }
    let failing = "SELECT CONVERT(b USING ascii) FROM t WHERE id = 1";
    let error = query(&connection, failing).expect_err(failing).to_string();
    assert!(error.contains("bytes outside ASCII"), "{error}");
    Ok(())
}

#[test]
fn columns_compare_with_json_values_as_json() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(
        "CREATE TABLE t (id BIGINT PRIMARY KEY, j JSON, s TEXT, i INT, f DOUBLE, v VARCHAR(5))",
    )?;
    connection.execute(
        "INSERT INTO t VALUES (1, '{\"b\":2,  \"a\":[1,2]}', 'x', 3, 2.5, '3'), \
         (2, NULL, NULL, NULL, NULL, NULL)",
    )?;
    assert_eq!(
        query(
            &connection,
            "SELECT j = CAST('{\"a\": [1, 2], \"b\": 2}' AS JSON), \
             s = CAST('\"x\"' AS JSON), i < CAST(3.5 AS JSON), f = CAST(2.5 AS JSON), \
             v = CAST(3 AS JSON), v = CAST('\"3\"' AS JSON), id = CAST('1' AS JSON) \
             FROM t ORDER BY id"
        )?,
        vec![
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(0),
                Value::from_i64(1),
                Value::from_i64(1),
            ],
            vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::from_i64(0),
            ],
        ]
    );
    Ok(())
}

#[test]
fn json_values_compare_exactly_with_integers_beyond_bigint() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT CAST(18446744073709551614 AS JSON) < 18446744073709551615, \
             18446744073709551615 > CAST('18446744073709551615' AS JSON), \
             -9223372036854775808 <= CAST(9223372036854775807 AS JSON), \
             -9223372036854775809 < CAST(1 AS JSON), \
             CAST('[\"a\"]' AS JSON) < 18446744073709551615"
        )?,
        vec![vec![
            Value::from_i64(1),
            Value::from_i64(0),
            Value::from_i64(1),
            Value::from_i64(1),
            Value::from_i64(0),
        ]]
    );
    Ok(())
}

#[test]
fn number_literals_beyond_bigint_compare_exactly() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT 18446744073709551615 = 18446744073709551614, \
             18446744073709551615 > 18446744073709551614.5, \
             -9223372036854775809 < -9223372036854775808, \
             18446744073709551615 <=> 18446744073709551615.000, \
             18446744073709551614 BETWEEN 18446744073709551615 AND 18446744073709551615, \
             18446744073709551614 NOT BETWEEN 1 AND 18446744073709551615,              0xFF666F6F626172FF <= 9223372036854775807,              20000101103458.123456 < 0xFF666F6F626172FF,              0xFF666F6F626172FF > -1.1e0"
        )?,
        vec![vec![
            Value::from_i64(0),
            Value::from_i64(1),
            Value::from_i64(1),
            Value::from_i64(1),
            Value::from_i64(0),
            Value::from_i64(0),
            Value::from_i64(0),
            Value::from_i64(1),
            Value::from_i64(1),
        ]]
    );
    Ok(())
}

#[test]
fn strings_compare_with_dates_and_timestamps_as_datetimes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, expected) in [
        ("SELECT '0000-01-03' != TIMESTAMP '2024-12-30 10:34:58'", 1),
        ("SELECT '3001-02-19 00:00:00' <= DATE '2000-01-01'", 0),
        ("SELECT DATE '2000-01-01' < '2000-01-01 00:00:01'", 1),
        (
            "SELECT '20000101103458' = TIMESTAMP '2024-12-30 10:34:58'",
            0,
        ),
        (
            "SELECT TIMESTAMP '2000-01-01 10:34:58.978654' > '20000101103458.123456'",
            1,
        ),
        (
            "SELECT TIMESTAMP '2020-01-02 03:04:05.5' = '2020-01-02 03:04:05.50'",
            1,
        ),
        ("SELECT 0 AND DATE '2000-01-01' = 'foo'", 0),
    ] {
        assert_eq!(
            query(&connection, sql)?,
            vec![vec![Value::from_i64(expected)]],
            "{sql}"
        );
    }
    for (sql, message) in [
        (
            "SELECT DATE '2000-01-01' <=> _utf8 'foobar'",
            "ERROR 1525 (HY000): Incorrect DATE value: 'foobar'",
        ),
        (
            "SELECT '20000101foo' <=> DATE '2000-01-01'",
            "ERROR 1525 (HY000): Incorrect DATE value: '20000101foo'",
        ),
        (
            "SELECT '-1.5' <= TIMESTAMP '2000-01-01 10:34:58.123456'",
            "ERROR 1525 (HY000): Incorrect DATETIME value: '-1.5'",
        ),
        (
            "SELECT '0000-02-29' >= TIMESTAMP '2024-12-30 10:34:58'",
            "ERROR 1525 (HY000): Incorrect DATETIME value: '0000-02-29'",
        ),
    ] {
        let error = query(&connection, sql).expect_err(sql).to_string();
        assert!(error.contains(message), "{sql}: {error}");
    }
    for refused in [
        "SELECT ' 0 ' = TIMESTAMP '2000-01-01 10:34:58'",
        "SELECT '2000-00-01' = DATE '2000-01-01'",
    ] {
        let error = query(&connection, refused).expect_err(refused).to_string();
        assert!(error.contains("not lowered yet"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn temporal_literals_of_different_types_compare_as_datetimes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, expected) in [
        (
            "SELECT TIME '31:34:58' <= TIMESTAMP '2000-01-01 10:34:58.978654'",
            0,
        ),
        (
            "SELECT TIME '-5 10:34:58' > TIMESTAMP '2000-01-01 10:34:58.123456'",
            1,
        ),
        (
            "SELECT TIMESTAMP '2000-01-01 10:34:58' = DATE '2000-01-01'",
            0,
        ),
        (
            "SELECT DATE '2000-01-01' <=> TIMESTAMP '2000-01-01 00:00:00.000'",
            1,
        ),
        (
            "SELECT TIMESTAMP '2000-01-01 10:34:58.5' < TIMESTAMP '2000-01-01 10:34:58.50001'",
            1,
        ),
        ("SELECT TIME '-10:04:58' < DATE '2000-01-01'", 0),
    ] {
        assert_eq!(
            query(&connection, sql)?,
            vec![vec![Value::from_i64(expected)]],
            "{sql}"
        );
    }
    let today = match query(&connection, "SELECT CURRENT_DATE")?.as_slice() {
        [row] => row[0].to_string(),
        rows => panic!("CURRENT_DATE gave {rows:?}"),
    };
    let sql = format!(
        "SELECT TIME '00:00:00' = TIMESTAMP '{today} 00:00:00', \
         TIME '24:00:00.5' > TIMESTAMP '{today} 23:59:59.999999'"
    );
    assert_eq!(
        query(&connection, &sql)?,
        vec![vec![Value::from_i64(1), Value::from_i64(1)]]
    );
    Ok(())
}

#[test]
fn null_operands_skip_invalid_json_as_in_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for sql in [
        "SELECT NULL - 1 <= CAST('\"a\": 1' AS JSON)",
        "SELECT NULL = CAST('x' AS JSON)",
        "SELECT NULL NOT LIKE CAST('x' AS JSON)",
        "SELECT CONCAT(NULL * NULL, CAST('x' AS JSON))",
        "SELECT STRCMP(NULL * 1, CAST('x' AS JSON))",
        "SELECT MAKETIME(NULL * NULL, 0, CAST('x' AS JSON))",
        "SELECT MAKEDATE(CAST('x' AS JSON), NULL)",
    ] {
        assert_eq!(query(&connection, sql)?, vec![vec![Value::Null]], "{sql}");
    }
    for sql in [
        "SELECT NULL - NULL <=> CAST('x' AS JSON)",
        "SELECT NULL * 1 || CAST('x' AS JSON)",
        "SELECT JSON_ARRAY() > CAST('x' AS JSON)",
        "SELECT CONCAT(CAST('x' AS JSON), NULL * NULL)",
        "SELECT MAKEDATE(1 + NULL, CAST('x' AS JSON))",
    ] {
        let error = query(&connection, sql).expect_err(sql).to_string();
        assert!(error.contains("3141"), "{sql}: {error}");
    }
    Ok(())
}

#[test]
fn casts_of_temporal_json_values_convert_as_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, shown) in [
        (
            "SELECT CAST(CAST(TIME '-101:34:58' AS JSON) AS TIME(4))",
            "-05:34:58.0000",
        ),
        (
            "SELECT CAST(CAST(TIME '32:34:58.5' AS JSON) AS TIME)",
            "00:34:59",
        ),
        (
            "SELECT CAST(CAST(DATE '2000-01-01' AS JSON) AS DATETIME(4))",
            "2000-01-01 00:00:00.0000",
        ),
        (
            "SELECT CONVERT(CAST(TIME '5 12:34:58' AS JSON), TIME(4))",
            "04:34:58.0000",
        ),
        (
            "SELECT CAST(CAST(CAST(DATE '2000-01-02' AS JSON) AS JSON) AS CHAR(16))",
            "\"2000-01-02\"",
        ),
    ] {
        assert_eq!(
            query(&connection, sql)?,
            vec![vec![Value::build_text(shown)]],
            "{sql}"
        );
    }
    assert_eq!(
        query(
            &connection,
            "SELECT CAST(CAST(TIMESTAMP '2024-12-30 10:34:58' AS JSON) AS TIME), \
             CAST(CAST(CAST(TIME '12:34:58' AS JSON) AS JSON) AS SIGNED)"
        )?,
        vec![vec![Value::Null, Value::from_i64(0)]]
    );
    Ok(())
}

#[test]
fn comparisons_read_nul_binary_literals_as_bytes() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT HEX(LEAST(0x0, 1)), HEX(GREATEST(0x0, 'a', X'FF')), \
             STRCMP(X'00', 'a'), STRCMP(X'0001', X'00')"
        )?,
        vec![vec![
            Value::build_text("00"),
            Value::build_text("FF"),
            Value::from_i64(-1),
            Value::from_i64(1),
        ]]
    );
    Ok(())
}

#[test]
fn like_matches_numbers_as_mysql_text() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE ln (g DOUBLE, x DECIMAL(5, 2))")?;
    connection.execute("INSERT INTO ln VALUES (1, 1.5)")?;
    assert_eq!(
        query(
            &connection,
            "SELECT 0.0 NOT LIKE 0.0e0, 1e0 LIKE '1', 100e0 LIKE '100', 0.0 LIKE 0, \
             1.50 LIKE '1.50', g LIKE '1', x LIKE '1.50' FROM ln"
        )?,
        vec![[1, 1, 1, 0, 1, 1, 1].map(Value::from_i64).to_vec()]
    );
    Ok(())
}

#[test]
fn in_lists_with_null_compare_as_mysql() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT 'FOO' IN ('foo', NULL), _latin1 'FOO' NOT IN (_latin1 'foo', NULL),              0 NOT IN (NULL, 'foo'), 2 NOT IN (NULL, '1')"
        )?,
        vec![vec![
            Value::from_i64(1),
            Value::from_i64(0),
            Value::from_i64(0),
            Value::Null,
        ]]
    );
    Ok(())
}

#[test]
fn literal_calls_take_mysql_values() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            "SELECT MINUTE(DATE '2000-01-01'), TIME_TO_SEC(DATE '2000-01-01'), UNHEX(0.0), \
             HEX(REVERSE(_binary 'ab\u{fc}')), CAST(CAST(18446744073709551615 AS DECIMAL(30, 2)) AS CHAR)"
        )?,
        vec![vec![
            Value::from_i64(0),
            Value::from_i64(0),
            Value::Null,
            Value::build_text("BCC36261"),
            Value::build_text("18446744073709551615.00"),
        ]]
    );
    Ok(())
}

#[test]
fn hex_of_a_conversion_shows_the_bytes_of_the_character_set() -> turso_core::Result<()> {
    let connection = open_memory()?;
    for (sql, expected) in [
        (
            "SELECT HEX(CONVERT('foo' USING utf16))",
            Value::build_text("0066006F006F"),
        ),
        (
            "SELECT HEX(CONVERT(X'4D7953514C' USING ucs2))",
            Value::build_text("004D7953514C"),
        ),
        (
            "SELECT HEX(CONVERT(X'4D7953514C' USING utf32))",
            Value::Null,
        ),
        (
            "SELECT HEX(CONVERT(_latin1 X'80' USING utf32))",
            Value::build_text("000020AC"),
        ),
        (
            "SELECT HEX(CONVERT('\u{e9}' USING latin1))",
            Value::build_text("E9"),
        ),
    ] {
        assert_eq!(query(&connection, sql)?, vec![vec![expected]], "{sql}");
    }
    for refused in [
        "SELECT HEX(CONVERT('\u{1F600}' USING ucs2))",
        "SELECT CONVERT('foo' USING utf16)",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(error.contains("not lowered yet"), "{refused}: {error}");
    }
    Ok(())
}

#[test]
fn table_statements_read_whole_tables() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute("CREATE TABLE t1 (i INT)")?;
    connection.execute("CREATE TABLE t2 (j INT)")?;
    connection.execute("INSERT INTO t1 VALUES (3), (1)")?;
    connection.execute("INSERT INTO t2 VALUES (2), (1)")?;
    assert_eq!(
        query(&connection, "TABLE t1 UNION TABLE t2 ORDER BY i")?,
        vec![
            vec![Value::from_i64(1)],
            vec![Value::from_i64(2)],
            vec![Value::from_i64(3)],
        ]
    );
    assert_eq!(
        query(&connection, "table t1 order by i limit 1")?,
        vec![vec![Value::from_i64(1)]]
    );
    let error = connection
        .execute("TABLE t1 UNION TABLE t2 ORDER BY t1.i")
        .expect_err("a qualified column in a global ORDER BY")
        .to_string();
    assert!(error.contains("ERROR 1250 "), "{error}");
    let error = connection
        .execute("TABLE t1 WHERE i = 1")
        .expect_err("TABLE takes no WHERE")
        .to_string();
    assert!(error.contains("not lowered yet"), "{error}");
    Ok(())
}

#[test]
fn json_table_lists_the_rows_of_a_document() -> turso_core::Result<()> {
    let connection = open_memory()?;
    assert_eq!(
        query(
            &connection,
            r#"SELECT * FROM JSON_TABLE('[{"a":1,"b":[5,6]},{"a":"x","b":[]}]', '$[*]' COLUMNS(
                id FOR ORDINALITY,
                a INT PATH '$.a' DEFAULT '7' ON ERROR,
                NESTED PATH '$.b[*]' COLUMNS (b VARCHAR(4) PATH '$'),
                has_b INT EXISTS PATH '$.b[0]')) AS jt"#
        )?,
        vec![
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::build_text("5"),
                Value::from_i64(1),
            ],
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::build_text("6"),
                Value::from_i64(1),
            ],
            vec![
                Value::from_i64(2),
                Value::from_i64(7),
                Value::Null,
                Value::from_i64(0),
            ],
        ]
    );
    connection.execute("CREATE TABLE t (id INT PRIMARY KEY, j JSON)")?;
    connection.execute(r#"INSERT INTO t VALUES (1, '["p","q"]'), (2, '["r"]')"#)?;
    assert_eq!(
        query(
            &connection,
            "SELECT t.id, jt.v FROM t, JSON_TABLE(t.j, '$[*]' COLUMNS (v CHAR(1) PATH '$')) jt \
             WHERE jt.v <> 'Q' ORDER BY t.id, jt.v"
        )?,
        vec![
            vec![Value::from_i64(1), Value::build_text("p")],
            vec![Value::from_i64(2), Value::build_text("r")],
        ]
    );
    for (sql, code) in [
        (
            "SELECT * FROM JSON_TABLE('{}', '$' COLUMNS (c INT PATH '$.c' ERROR ON EMPTY)) jt",
            3665,
        ),
        (
            r#"SELECT * FROM JSON_TABLE('{"c":"x"}', '$' COLUMNS (c INT PATH '$.c' ERROR ON ERROR)) jt"#,
            3156,
        ),
        (
            "SELECT * FROM JSON_TABLE((SELECT j FROM t LIMIT 1), '$[*]' COLUMNS (v INT PATH '$')) jt",
            1210,
        ),
    ] {
        let error = connection.execute(sql).expect_err(sql).to_string();
        assert!(error.contains(&format!("ERROR {code} ")), "{sql}: {error}");
    }
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
fn binary_literals_that_are_not_text_are_written_to_binary_columns() -> turso_core::Result<()> {
    let connection = open_memory()?;
    connection.execute(
        "CREATE TABLE t (id INT PRIMARY KEY, m MEDIUMBLOB, v VARBINARY(4), b BINARY(3), s TEXT)",
    )?;
    connection.execute("INSERT INTO t VALUES (1, X'00FF8A6A', 0x1F8B, b'11111111', 'a')")?;
    connection.execute("INSERT INTO t (id, v) VALUES (2, X'FE')")?;
    connection
        .execute("INSERT INTO t (id, v) VALUES (2, X'01') ON DUPLICATE KEY UPDATE m = X'80'")?;
    connection.execute("UPDATE t SET b = X'C0' WHERE id = 2")?;
    assert_eq!(
        query(
            &connection,
            "SELECT id, HEX(m), HEX(v), HEX(b), LENGTH(m) FROM t ORDER BY id"
        )?,
        vec![
            vec![
                Value::from_i64(1),
                Value::build_text("00FF8A6A"),
                Value::build_text("1F8B"),
                Value::build_text("FF0000"),
                Value::from_i64(4),
            ],
            vec![
                Value::from_i64(2),
                Value::build_text("80"),
                Value::build_text("FE"),
                Value::build_text("C00000"),
                Value::from_i64(1),
            ],
        ]
    );
    for refused in [
        "INSERT INTO t (id, s) VALUES (3, X'FF')",
        "UPDATE t SET s = 0xFF",
    ] {
        let error = connection.execute(refused).expect_err(refused).to_string();
        assert!(error.contains("not lowered yet"), "{refused}: {error}");
    }
    let error = connection
        .execute("INSERT INTO t (id, v) VALUES (3, X'0102030405')")
        .expect_err("too long");
    assert_eq!(
        turso_mysql::mysql_error(&error).map(|mysql| mysql.code),
        Some(1406)
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

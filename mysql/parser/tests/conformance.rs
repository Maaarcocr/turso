use turso_mysql_parser::{parse, parse_and_translate, parse_one, translate, MysqlParserError};
use turso_parser::ast;

struct SqlCase {
    name: &'static str,
    sql: &'static str,
}

#[test]
fn mysql_parser_accepts_representative_mysql_84_syntax() {
    let cases = [
        SqlCase { name: "backtick identifiers", sql: "SELECT `id` FROM `users`" },
        SqlCase { name: "limit comma", sql: "SELECT id FROM users LIMIT 5, 10" },
        SqlCase { name: "limit offset", sql: "SELECT id FROM users LIMIT 10 OFFSET 5" },
        SqlCase { name: "select modifier", sql: "SELECT SQL_NO_CACHE id FROM users" },
        SqlCase { name: "insert ignore", sql: "INSERT IGNORE INTO users(id) VALUES (1)" },
        SqlCase { name: "on duplicate key", sql: "INSERT INTO users(id, name) VALUES (1, 'Ada') ON DUPLICATE KEY UPDATE name = 'Ada'" },
        SqlCase { name: "replace", sql: "REPLACE INTO users(id, name) VALUES (1, 'Ada')" },
        SqlCase { name: "update limit", sql: "UPDATE users SET name = 'Ada' WHERE id = 1 LIMIT 1" },
        SqlCase { name: "delete limit", sql: "DELETE FROM users WHERE id = 1 LIMIT 1" },
        SqlCase { name: "create table engine", sql: "CREATE TABLE users(id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4" },
        SqlCase { name: "alter table", sql: "ALTER TABLE users ADD COLUMN active BOOLEAN NOT NULL DEFAULT TRUE" },
        SqlCase { name: "truncate table", sql: "TRUNCATE TABLE users" },
        SqlCase { name: "create index", sql: "CREATE INDEX users_name ON users(name)" },
        SqlCase { name: "drop index", sql: "DROP INDEX users_name ON users" },
        SqlCase { name: "show tables", sql: "SHOW TABLES" },
        SqlCase { name: "show create table", sql: "SHOW CREATE TABLE users" },
        SqlCase { name: "describe", sql: "DESCRIBE users" },
        SqlCase { name: "explain", sql: "EXPLAIN SELECT * FROM users" },
        SqlCase { name: "use database", sql: "USE app" },
        SqlCase { name: "set session", sql: "SET SESSION sql_mode = 'STRICT_TRANS_TABLES'" },
        SqlCase { name: "start transaction", sql: "START TRANSACTION" },
        SqlCase { name: "commit", sql: "COMMIT" },
        SqlCase { name: "rollback", sql: "ROLLBACK" },
        SqlCase { name: "union", sql: "SELECT 1 UNION ALL SELECT 2" },
        SqlCase { name: "common table expression", sql: "WITH cte AS (SELECT 1) SELECT * FROM cte" },
        SqlCase { name: "case expression", sql: "SELECT CASE WHEN active THEN 'yes' ELSE 'no' END FROM users" },
        SqlCase { name: "cast signed", sql: "SELECT CAST('42' AS SIGNED)" },
        SqlCase { name: "json extract operator", sql: "SELECT payload->>'$.name' FROM events" },
    ];

    for case in cases {
        parse_one(case.sql).unwrap_or_else(|error| {
            panic!("{} failed to parse: {error}\nSQL: {}", case.name, case.sql)
        });
    }
}

#[test]
fn parser_handles_statement_boundaries_and_comments() {
    let cases = [
        ("", 0),
        ("-- comment only\n", 0),
        ("# mysql comment only\n", 0),
        ("/* block comment */", 0),
        ("SELECT 1;", 1),
        ("/* before */ SELECT 1 /* after */;", 1),
        ("SELECT 1; SELECT 2;", 2),
    ];

    for (sql, expected_count) in cases {
        let statements =
            parse(sql).unwrap_or_else(|error| panic!("boundary case failed: {error}\nSQL: {sql}"));
        assert_eq!(statements.len(), expected_count, "SQL: {sql}");
    }
}

#[test]
fn malformed_sql_returns_parse_errors() {
    let cases = [
        "SELEC 1",
        "SELECT 'unterminated",
        "SELECT (1",
        "INSERT INTO",
        "SELECT TRIM(LEADING 'a')",
    ];

    for sql in cases {
        let error = parse_one(sql).expect_err("malformed SQL must not parse");
        assert!(
            matches!(error, MysqlParserError::Parse(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
}

#[test]
fn lowers_basic_create_table_to_shared_ast() {
    let statement = parse_and_translate(
        "CREATE TABLE users(id INTEGER PRIMARY KEY, name VARCHAR(100), bio VARCHAR(1000));",
    )
    .expect("basic CREATE TABLE must lower");
    assert_eq!(
        statement.to_string(),
        "CREATE TABLE users (\
         id INT PRIMARY KEY NOT NULL, name VARCHAR (100) COLLATE \"und-u-ks-level1\", bio VARCHAR (1000) COLLATE \"und-u-ks-level1\") STRICT"
    );

    let ast::Stmt::CreateTable {
        temporary,
        if_not_exists,
        tbl_name,
        body,
    } = statement
    else {
        panic!("statement must lower to CREATE TABLE");
    };
    assert!(!temporary);
    assert!(!if_not_exists);
    assert_eq!(tbl_name.namespace, None);
    assert_eq!(tbl_name.name.as_str(), "users");
    assert_eq!(tbl_name.alias, None);

    let ast::CreateTableBody::ColumnsAndConstraints {
        columns,
        constraints,
        options,
    } = body
    else {
        panic!("CREATE TABLE must lower to columns and constraints");
    };
    assert_eq!(columns.len(), 3);
    assert!(constraints.is_empty());
    assert_eq!(
        options,
        ast::TableOptions {
            without_rowid_text: None,
            strict_text: Some("STRICT".to_string()),
        }
    );
    assert_eq!(columns[0].col_name.as_str(), "id");
    assert_eq!(columns[0].col_type.as_ref().unwrap().name, "INT");
    assert!(matches!(
        columns[0].constraints.as_slice(),
        [
            ast::NamedColumnConstraint {
                name: None,
                constraint: ast::ColumnConstraint::PrimaryKey {
                    order: None,
                    conflict_clause: None,
                    auto_increment: false,
                },
            },
            ast::NamedColumnConstraint {
                name: None,
                constraint: ast::ColumnConstraint::NotNull {
                    nullable: false,
                    conflict_clause: None,
                },
            },
        ]
    ));
    assert_eq!(columns[1].col_name.as_str(), "name");
    assert_eq!(columns[1].col_type.as_ref().unwrap().name, "VARCHAR");
    assert!(matches!(
        columns[1].col_type.as_ref().unwrap().size,
        Some(ast::TypeSize::MaxSize(_))
    ));
}

#[test]
fn lowers_create_table_if_not_exists_and_preserves_identifiers() {
    let statement = parse_and_translate(
        "CrEaTe TaBlE IF NOT EXISTS `select` (`primary` INTEGER PRIMARY KEY, `display``name` VARCHAR(100))",
    )
    .expect("quoted CREATE TABLE must lower");
    assert_eq!(
        statement.to_string(),
        "CREATE TABLE IF NOT EXISTS `select` (\
         `primary` INT PRIMARY KEY NOT NULL, `display``name` VARCHAR (100) COLLATE \"und-u-ks-level1\") STRICT"
    );

    let ast::Stmt::CreateTable {
        if_not_exists,
        tbl_name,
        body,
        ..
    } = statement
    else {
        panic!("statement must lower to CREATE TABLE");
    };
    assert!(if_not_exists);
    assert_eq!(tbl_name.name.as_str(), "select");
    assert!(tbl_name.name.quoted_with('`'));
    let ast::CreateTableBody::ColumnsAndConstraints { columns, .. } = body else {
        panic!("CREATE TABLE must lower to columns and constraints");
    };
    assert_eq!(columns[0].col_name.as_str(), "primary");
    assert!(columns[0].col_name.quoted_with('`'));
    assert_eq!(columns[1].col_name.as_str(), "display`name");
    assert!(columns[1].col_name.quoted_with('`'));
}

#[test]
fn lowers_mysql_ddl_to_canonical_sqlite_ddl() {
    let cases = [
        (
            "CREATE TABLE t (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT COMMENT 'key', PRIMARY KEY (id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4",
            "CREATE TABLE t (id BIGINT UNSIGNED PRIMARY KEY AUTOINCREMENT NOT NULL) STRICT",
        ),
        (
            "CREATE TABLE t (a TINYINT(1) DEFAULT '1', b DECIMAL(10,2) UNSIGNED, c FLOAT(7,3), d BIT(3))",
            "CREATE TABLE t (a TINYINT (1) DEFAULT 1, b DECIMAL UNSIGNED (10, 2), c FLOAT (7, 3), d BIT (3)) STRICT",
        ),
        (
            "CREATE TABLE t (a CHAR(2), b TEXT COLLATE utf8mb4_bin, c VARCHAR(3) CHARACTER SET utf8mb4) DEFAULT COLLATE utf8mb4_0900_bin",
            "CREATE TABLE t (a CHAR (2) COLLATE BINARY, b TEXT COLLATE RTRIM, c VARCHAR (3) COLLATE \"und-u-ks-level1\") STRICT",
        ),
        (
            "CREATE TABLE t (e ENUM('x','y') NOT NULL, s SET('r','w') DEFAULT 'w,r', j JSON, t TINYTEXT)",
            "CREATE TABLE t (e ENUM NOT NULL DEFAULT 'x' CHECK (e IN ('x', 'y')), s \"SET\" DEFAULT 'r,w' CHECK (s IN ('', 'r', 'w', 'r,w')), j LONGTEXT CHECK (json_valid (j)), t TINYTEXT COLLATE \"und-u-ks-level1\") STRICT",
        ),
        (
            "CREATE TABLE t (d DATE DEFAULT '2020-1-2', ts DATETIME(3) DEFAULT '2020-01-02 03:04:05', c TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
            "CREATE TABLE t (d DATE DEFAULT '2020-01-02', ts DATETIME (3) DEFAULT '2020-01-02 03:04:05.000', c TIMESTAMP DEFAULT CURRENT_TIMESTAMP) STRICT",
        ),
        (
            "CREATE TABLE t (id INT, email VARCHAR(50), KEY k (id), UNIQUE KEY uq (email), CONSTRAINT ck CHECK (id > 0), CONSTRAINT fk FOREIGN KEY (id) REFERENCES p (id) ON DELETE CASCADE)",
            "CREATE TABLE t (id INT, email VARCHAR (50) COLLATE \"und-u-ks-level1\", CONSTRAINT uq UNIQUE (email), CONSTRAINT ck CHECK (id > 0), CONSTRAINT fk FOREIGN KEY (id) REFERENCES p (id) ON DELETE CASCADE) STRICT",
        ),
        (
            "CREATE TABLE t (a INT, b INT AS (a + 1) STORED, c INT GENERATED ALWAYS AS (a * 2))",
            "CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED, c INT GENERATED ALWAYS AS (a * 2) VIRTUAL) STRICT",
        ),
        (
            "CREATE TEMPORARY TABLE t (id INTEGER, code BINARY(4), location POINT)",
            "CREATE TEMP TABLE t (id INT, code BINARY (4), location POINT) STRICT",
        ),
        (
            "CREATE TABLE t AS SELECT 1",
            "CREATE TABLE t AS SELECT 1",
        ),
        (
            "CREATE TABLE t SELECT 1",
            "CREATE TABLE t AS SELECT 1",
        ),
        (
            "CREATE TABLE t (j JSON, b INT AS (j->>'$.b'))",
            "CREATE TABLE t (j LONGTEXT CHECK (json_valid (j)), b INT GENERATED ALWAYS AS (JSON_UNQUOTE (JSON_EXTRACT (j, '$.b'))) VIRTUAL) STRICT",
        ),
        (
            "CREATE UNIQUE INDEX idx USING BTREE ON t (a DESC, b)",
            "CREATE UNIQUE INDEX idx ON t (a DESC, b)",
        ),
        (
            "DROP INDEX idx ON t",
            "DROP INDEX idx",
        ),
        (
            "ALTER TABLE t DROP INDEX idx",
            "DROP INDEX idx",
        ),
        (
            "ALTER TABLE t ADD COLUMN c INT UNSIGNED DEFAULT 0 COMMENT 'x'",
            "ALTER TABLE t ADD COLUMN c INT UNSIGNED DEFAULT 0",
        ),
        (
            "ALTER TABLE t RENAME COLUMN a TO b",
            "ALTER TABLE t RENAME COLUMN a TO b",
        ),
        (
            "ALTER TABLE t DROP COLUMN a",
            "ALTER TABLE t DROP COLUMN a",
        ),
        (
            "ALTER TABLE t RENAME TO u",
            "ALTER TABLE t RENAME TO u",
        ),
        (
            "RENAME TABLE t TO u",
            "ALTER TABLE t RENAME TO u",
        ),
        (
            "DROP TABLE IF EXISTS t",
            "DROP TABLE IF EXISTS t",
        ),
    ];

    for (mysql, sqlite) in cases {
        let statement = parse_and_translate(mysql)
            .unwrap_or_else(|error| panic!("DDL must lower: {error}\nSQL: {mysql}"));
        assert_eq!(statement.to_string(), sqlite, "SQL: {mysql}");
    }
}

#[test]
fn rejects_create_table_shapes_without_a_faithful_lowering() {
    let cases = [
        SqlCase {
            name: "qualified name",
            sql: "CREATE TABLE app.users(id INTEGER)",
        },
        SqlCase {
            name: "like",
            sql: "CREATE TABLE users LIKE source_users",
        },
        SqlCase {
            name: "columns and a select",
            sql: "CREATE TABLE users (id INT) AS SELECT 1",
        },
        SqlCase {
            name: "external",
            sql: "CREATE EXTERNAL TABLE users(id INTEGER)",
        },
        SqlCase {
            name: "transient",
            sql: "CREATE TRANSIENT TABLE users(id INTEGER)",
        },
        SqlCase {
            name: "global",
            sql: "CREATE GLOBAL TEMPORARY TABLE users(id INTEGER)",
        },
        SqlCase {
            name: "empty columns",
            sql: "CREATE TABLE users()",
        },
        SqlCase {
            name: "zerofill",
            sql: "CREATE TABLE users(id INTEGER ZEROFILL)",
        },
        SqlCase {
            name: "auto increment in a composite primary key",
            sql: "CREATE TABLE users(a INT AUTO_INCREMENT, b INT, PRIMARY KEY (a, b))",
        },
        SqlCase {
            name: "auto increment on a decimal",
            sql: "CREATE TABLE users(id DECIMAL(10,0) AUTO_INCREMENT PRIMARY KEY)",
        },
        SqlCase {
            name: "varchar missing length",
            sql: "CREATE TABLE users(name VARCHAR)",
        },
        SqlCase {
            name: "varchar dynamic length",
            sql: "CREATE TABLE users(name VARCHAR(?))",
        },
        SqlCase {
            name: "varchar fractional length",
            sql: "CREATE TABLE users(name VARCHAR(1.5))",
        },
        SqlCase {
            name: "geometry with a reference system",
            sql: "CREATE TABLE users(location POINT SRID 4326)",
        },
        SqlCase {
            name: "set with too many members to enumerate",
            sql: "CREATE TABLE users(flags SET('a','b','c','d','e','f','g'))",
        },
        SqlCase {
            name: "other character set",
            sql: "CREATE TABLE users(name VARCHAR(10) CHARACTER SET latin1)",
        },
        SqlCase {
            name: "other collation",
            sql: "CREATE TABLE users(name VARCHAR(10) COLLATE utf8mb4_general_ci)",
        },
        SqlCase {
            name: "on update current timestamp",
            sql: "CREATE TABLE users(updated TIMESTAMP ON UPDATE CURRENT_TIMESTAMP)",
        },
        SqlCase {
            name: "invisible column",
            sql: "CREATE TABLE users(id INT INVISIBLE)",
        },
        SqlCase {
            name: "current timestamp with fractional seconds",
            sql: "CREATE TABLE users(created DATETIME(6) DEFAULT CURRENT_TIMESTAMP(6))",
        },
        SqlCase {
            name: "datetime default that needs rounding",
            sql: "CREATE TABLE users(seen DATETIME DEFAULT '2020-01-01 00:00:00.5')",
        },
        SqlCase {
            name: "named inline primary key",
            sql: "CREATE TABLE users(id INTEGER CONSTRAINT users_pk PRIMARY KEY)",
        },
        SqlCase {
            name: "unique prefix key",
            sql: "CREATE TABLE users(name VARCHAR(10), UNIQUE KEY name_key (name(3)))",
        },
        SqlCase {
            name: "functional key part",
            sql: "CREATE TABLE users(a INT, KEY k ((a + 1)))",
        },
        SqlCase {
            name: "fulltext key",
            sql: "CREATE TABLE users(bio TEXT, FULLTEXT KEY bio_text (bio))",
        },
        SqlCase {
            name: "foreign key set default",
            sql:
                "CREATE TABLE users(a INT, FOREIGN KEY (a) REFERENCES p (id) ON DELETE SET DEFAULT)",
        },
        SqlCase {
            name: "other storage engine",
            sql: "CREATE TABLE users(id INTEGER) ENGINE=MyISAM",
        },
        SqlCase {
            name: "other table character set",
            sql: "CREATE TABLE users(id INTEGER) DEFAULT CHARSET=latin1",
        },
        SqlCase {
            name: "auto increment start",
            sql: "CREATE TABLE users(id INTEGER) AUTO_INCREMENT=10",
        },
        SqlCase {
            name: "partitioning",
            sql: "CREATE TABLE users(id INTEGER) PARTITION BY HASH(id) PARTITIONS 2",
        },
    ];

    for case in cases {
        let error = match parse_and_translate(case.sql) {
            Ok(_) => panic!("{} unexpectedly lowered\nSQL: {}", case.name, case.sql),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                MysqlParserError::Parse(_) | MysqlParserError::Unsupported(_)
            ),
            "SQL: {}\nerror: {error}",
            case.sql
        );
    }
}

#[test]
fn reports_ddl_that_mysql_rejects() {
    let cases = [
        SqlCase {
            name: "auto increment outside any key",
            sql: "CREATE TABLE users(id INTEGER AUTO_INCREMENT)",
        },
        SqlCase {
            name: "varchar longer than a utf8mb4 row allows",
            sql: "CREATE TABLE users(name VARCHAR(16384))",
        },
        SqlCase {
            name: "float precision beyond double",
            sql: "CREATE TABLE users(ratio FLOAT(54))",
        },
        SqlCase {
            name: "invalid decimal",
            sql: "CREATE TABLE users(price DECIMAL(5,6))",
        },
        SqlCase {
            name: "fractional seconds beyond microseconds",
            sql: "CREATE TABLE users(created DATETIME(7))",
        },
        SqlCase {
            name: "enum with duplicate members",
            sql: "CREATE TABLE users(state ENUM('a', 'A'))",
        },
        SqlCase {
            name: "collation on a binary string",
            sql: "CREATE TABLE users(code VARBINARY(10) COLLATE utf8mb4_0900_bin)",
        },
        SqlCase {
            name: "current timestamp on an integer",
            sql: "CREATE TABLE users(created INT DEFAULT CURRENT_TIMESTAMP)",
        },
        SqlCase {
            name: "non-numeric integer default",
            sql: "CREATE TABLE users(id INT DEFAULT 'abc')",
        },
        SqlCase {
            name: "out of range default",
            sql: "CREATE TABLE users(id TINYINT DEFAULT 300)",
        },
        SqlCase {
            name: "literal text default",
            sql: "CREATE TABLE users(bio TEXT DEFAULT 'none')",
        },
        SqlCase {
            name: "null default on a not null column",
            sql: "CREATE TABLE users(id INT NOT NULL DEFAULT NULL)",
        },
        SqlCase {
            name: "enum default outside the members",
            sql: "CREATE TABLE users(state ENUM('a') DEFAULT 'b')",
        },
        SqlCase {
            name: "invalid date default",
            sql: "CREATE TABLE users(born DATE DEFAULT '2020-02-30')",
        },
        SqlCase {
            name: "two primary keys",
            sql: "CREATE TABLE users(id INTEGER PRIMARY KEY, other INTEGER PRIMARY KEY)",
        },
        SqlCase {
            name: "text primary key without a length",
            sql: "CREATE TABLE users(name TEXT PRIMARY KEY)",
        },
        SqlCase {
            name: "json key",
            sql: "CREATE TABLE users(doc JSON, KEY doc_key (doc))",
        },
        SqlCase {
            name: "key longer than 3072 bytes",
            sql: "CREATE TABLE users(name VARCHAR(1000), KEY name_key (name))",
        },
        SqlCase {
            name: "prefix key on an integer",
            sql: "CREATE TABLE users(id INT, KEY id_key (id(4)))",
        },
        SqlCase {
            name: "prefix longer than the column",
            sql: "CREATE TABLE users(name VARCHAR(10), KEY name_key (name(11)))",
        },
        SqlCase {
            name: "duplicate key names",
            sql: "CREATE TABLE users(a INT, b INT, KEY k (a), KEY k (b))",
        },
        SqlCase {
            name: "key on a missing column",
            sql: "CREATE TABLE users(a INT, KEY k (b))",
        },
        SqlCase {
            name: "index named primary",
            sql: "CREATE INDEX `PRIMARY` ON users (id)",
        },
        SqlCase {
            name: "identifier longer than 64 characters",
            sql: "CREATE TABLE abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklm (id INT)",
        },
        SqlCase {
            name: "invalid date",
            sql: "SELECT DATE '2018-02-30'",
        },
        SqlCase {
            name: "comparison of two collations",
            sql: "SELECT 'a' COLLATE utf8mb4_0900_as_cs = 'A' COLLATE utf8mb4_0900_as_ci",
        },
        SqlCase {
            name: "collated STRCMP of a binary string",
            sql: "SELECT STRCMP('a' COLLATE utf8mb4_0900_as_cs, 0xff)",
        },
        SqlCase {
            name: "LIKE of a JSON value and a binary string that is not UTF-8",
            sql: "SELECT CAST(TRUE AS JSON) LIKE 0xFF",
        },
        SqlCase {
            name: "unknown storage engine",
            sql: "CREATE TABLE t (id INT) ENGINE=NoSuchEngine",
        },
        SqlCase {
            name: "SQLite STRICT table",
            sql: "CREATE TABLE t (a INT) STRICT",
        },
        SqlCase {
            name: "type name MySQL does not have",
            sql: "CREATE TABLE t (a INT, b ANY)",
        },
    ];

    for case in cases {
        let error = match parse_and_translate(case.sql) {
            Ok(_) => panic!("{} unexpectedly lowered\nSQL: {}", case.name, case.sql),
            Err(error) => error,
        };
        assert!(
            matches!(error, MysqlParserError::Rejected(_)),
            "{}: SQL: {}\nerror: {error}",
            case.name,
            case.sql
        );
    }
}

#[test]
fn reports_decimal_casts_that_mysql_rejects() {
    let cases = [
        SqlCase {
            name: "more decimals than digits",
            sql: "SELECT CAST(1 AS DECIMAL(2, 4))",
        },
        SqlCase {
            name: "more than 65 digits",
            sql: "SELECT CAST(1 AS DECIMAL(66))",
        },
        SqlCase {
            name: "more than 30 decimals",
            sql: "SELECT CONVERT(1, DECIMAL(65, 31))",
        },
        SqlCase {
            name: "beside syntax that is not lowered",
            sql: "SELECT 0x41 + CAST(1 AS DECIMAL(0, 4))",
        },
        SqlCase {
            name: "in a statement that is not lowered",
            sql: "INSERT INTO t VALUES (CAST(1 AS DEC(66)))",
        },
        SqlCase {
            name: "datetime with more than six fractional digits",
            sql: "SELECT CAST(1 AS DATETIME(7))",
        },
        SqlCase {
            name: "time with more than six fractional digits",
            sql: "SELECT CONVERT(1, TIME(7))",
        },
    ];

    for case in cases {
        let error = match parse_and_translate(case.sql) {
            Ok(_) => panic!("{} unexpectedly lowered\nSQL: {}", case.name, case.sql),
            Err(error) => error,
        };
        assert!(
            matches!(error, MysqlParserError::Rejected(_)),
            "{}: SQL: {}\nerror: {error}",
            case.name,
            case.sql
        );
    }
}

#[test]
fn lowers_anonymous_parameters_in_projection_order() {
    let statement =
        parse_and_translate("SELECT ?, ? AS named").expect("anonymous parameters must lower");
    let ast::Stmt::Select(select) = statement else {
        panic!("statement must lower to SELECT");
    };
    let ast::OneSelect::Select { columns, .. } = select.body.select else {
        panic!("query body must lower to SELECT");
    };
    assert_eq!(columns.len(), 2);

    let ast::ResultColumn::Expr(first_expr, Some(ast::As::ImplicitColumnName(first_name))) =
        &columns[0]
    else {
        panic!("first parameter must have an implicit metadata name");
    };
    let ast::Expr::Variable(first) = first_expr.as_ref() else {
        panic!("first projection must be a variable");
    };
    assert_eq!(first.index.get(), 1);
    assert_eq!(first.name, None);
    assert_eq!(first_name.as_str(), "?");

    let ast::ResultColumn::Expr(second_expr, Some(ast::As::As(second_name))) = &columns[1] else {
        panic!("second parameter must preserve its explicit alias");
    };
    let ast::Expr::Variable(second) = second_expr.as_ref() else {
        panic!("second projection must be a variable");
    };
    assert_eq!(second.index.get(), 2);
    assert_eq!(second.name, None);
    assert_eq!(second_name.as_str(), "named");
}

#[test]
fn rejects_non_anonymous_parameters() {
    let numbered = parse_one("SELECT ?1").expect("sqlparser accepts numbered placeholders");
    let error = translate(&numbered).expect_err("numbered placeholders must not lower");
    assert!(matches!(error, MysqlParserError::Unsupported(_)));

    for sql in ["SELECT :named", "SELECT @named"] {
        match parse_one(sql) {
            Ok(statement) => {
                let error = translate(&statement).expect_err("named placeholders must not lower");
                assert!(
                    matches!(error, MysqlParserError::Unsupported(_)),
                    "SQL: {sql}\nerror: {error}"
                );
            }
            Err(error) => assert!(
                matches!(error, MysqlParserError::Parse(_)),
                "SQL: {sql}\nerror: {error}"
            ),
        }
    }
}

#[test]
fn dollar_sign_starts_an_identifier_not_a_parameter() {
    let statement =
        parse_and_translate("SELECT $named").expect("MySQL identifiers may start with $");
    let ast::Stmt::Select(select) = statement else {
        panic!("statement must lower to SELECT");
    };
    let ast::OneSelect::Select { columns, .. } = select.body.select else {
        panic!("query body must lower to SELECT");
    };
    assert!(matches!(
        columns.as_slice(),
        [ast::ResultColumn::Expr(expr, _)] if matches!(expr.as_ref(), ast::Expr::Id(name) if name.as_str() == "$named")
    ));
}

#[test]
fn numbers_parameters_in_text_order() {
    let statement = parse_and_translate(
        "SELECT ? + 1 FROM t JOIN u ON t.id = ? WHERE a = ? GROUP BY b HAVING COUNT(*) > ? ORDER BY ? LIMIT ?, ?",
    )
    .expect("parameters inside expressions must lower");
    assert_eq!(
        statement.to_string(),
        "SELECT ?1 + 1 FROM t JOIN u ON t.id = ?2 WHERE a = ?3 GROUP BY b HAVING COUNT (*) > ?4 ORDER BY ?5 LIMIT ?7 OFFSET ?6"
    );
}

#[test]
fn lowers_select_statements_to_shared_ast() {
    let cases = [
        ("SELECT greeting", "SELECT greeting"),
        ("SELECT 1 + 2, -1, NOT TRUE", "SELECT 1 + 2, - 1, NOT TRUE"),
        ("SELECT VERSION()", "SELECT VERSION ()"),
        ("SELECT CAST(1 AS SIGNED)", "SELECT __mysql_cast_as_signed (1)"),
        (
            "SELECT CONVERT(18446744073709551615, UNSIGNED)",
            "SELECT __mysql_cast_as_unsigned ('18446744073709551615')",
        ),
        (
            "SELECT CAST(99999999999999999999 AS SIGNED)",
            "SELECT __mysql_cast_as_signed (CAST ('99999999999999999999' AS DECIMAL (20, 0)))",
        ),
        (
            "SELECT CASE WHEN TRUE THEN 1 ELSE 0 END",
            "SELECT CASE WHEN TRUE THEN 1 ELSE 0 END",
        ),
        ("SELECT *", "SELECT *"),
        ("SELECT users.* FROM users", "SELECT users.* FROM users"),
        ("SELECT 1 FROM dual", "SELECT 1"),
        ("SELECT 1 WHERE TRUE", "SELECT 1 WHERE TRUE"),
        ("SELECT DISTINCT 1", "SELECT DISTINCT 1"),
        ("SELECT 1 GROUP BY 1", "SELECT 1 GROUP BY 1"),
        (
            "SELECT a DIV 2 AS x, COUNT(*) FROM t GROUP BY x HAVING x > 1",
            "SELECT CAST (a / 2 AS INTEGER) AS x, COUNT (*) FROM t GROUP BY x HAVING x > 1",
        ),
        ("SELECT 1 HAVING TRUE", "SELECT 1 HAVING TRUE"),
        ("SELECT 1 ORDER BY 1", "SELECT 1 ORDER BY 1"),
        ("SELECT 1 LIMIT 1", "SELECT 1 LIMIT 1"),
        ("SELECT 1 LIMIT 2, 3", "SELECT 1 LIMIT 3 OFFSET 2"),
        ("SELECT (SELECT 1)", "SELECT (SELECT 1)"),
        ("SELECT 1 UNION SELECT 2", "SELECT 1 UNION SELECT 2"),
        (
            "WITH cte AS (SELECT 1) SELECT 1",
            "WITH cte AS (SELECT 1) SELECT 1",
        ),
        ("VALUES ROW(1), ROW(2)", "VALUES (1), (2)"),
        (
            "SELECT a FROM t1 LEFT JOIN t2 USING (a) NATURAL JOIN t3, t4",
            "SELECT a FROM t1 LEFT JOIN t2 USING (a) NATURAL JOIN t3, t4",
        ),
        ("SELECT 7 DIV 2", "SELECT CAST (7 / 2 AS INTEGER)"),
        ("SELECT 7 MOD 2", "SELECT 7 % 2"),
        (
            "SELECT -1.5, -.5, -a, - -2, +b, 18446744073709551616",
            "SELECT CAST ('-1.5' AS DECIMAL (2, 1)), CAST ('-.5' AS DECIMAL (1, 1)), ((a) * - 1), ((- 2) * - 1), b, CAST ('18446744073709551616' AS DECIMAL (20, 0))",
        ),
        (
            "SELECT CAST(a AS DECIMAL), CAST(a AS DECIMAL(5)), CONVERT(a, DECIMAL(0))",
            "SELECT CAST (a AS DECIMAL (10, 0)), CAST (a AS DECIMAL (5, 0)), CAST (a AS DECIMAL (10, 0))",
        ),
        ("SELECT a <=> b", "SELECT a IS b"),
        ("SELECT a && b || c", "SELECT a AND b OR c"),
        ("SELECT \"text\"", "SELECT 'text'"),
        ("SELECT 'a\\fb'", "SELECT 'afb'"),
        ("SELECT 0x41, b'1000010'", "SELECT x'41', x'42'"),
        (
            "SELECT _utf8mb4'abc', _binary'abc'",
            "SELECT 'abc', x'616263'",
        ),
        ("SELECT DATE '2018-5-1'", "SELECT '2018-05-01'"),
        ("SELECT DATE '2020-01-01' + 0", "SELECT 20200101 + 0"),
        (
            "SELECT 'a' = 'A', 'b' IN ('B'), _binary 'a' = 'A'",
            "SELECT 'a' COLLATE \"und-u-ks-level1\" = 'A', 'b' COLLATE \"und-u-ks-level1\" IN ('B'), 'a' = 'A'",
        ),
        (
            "SELECT 'a' COLLATE utf8mb4_0900_as_cs = 'A'",
            "SELECT 'a' COLLATE \"und-u-ks-level3\" = 'A'",
        ),
        (
            "SELECT 'a' = 'A' COLLATE utf8mb4_bin",
            "SELECT 'a' = 'A' COLLATE RTRIM",
        ),
        (
            "SELECT STRCMP('a' COLLATE utf8mb4_0900_as_ci, 'A')",
            "SELECT __mysql_collated_strcmp ('utf8mb4_0900_as_ci', 'a' COLLATE \"und-u-ks-level2\", 'A')",
        ),
        (
            "SELECT MAKEDATE(TIME '00:00:59', 1)",
            "SELECT MAKEDATE (000059, 1)",
        ),
        (
            "SELECT TIME '-01:00:00' < TIME '-10:00:00'",
            "SELECT - 010000 < - 100000",
        ),
        (
            "SELECT DATE '2020-1-1' = DATE '2020-01-01'",
            "SELECT '2020-01-01' = '2020-01-01'",
        ),
        (
            "SELECT TIME '10:04:58' | 1, ~DATE '2000-01-01'",
            "SELECT __mysql_unsigned_shown (__mysql_bit_or (__mysql_bit_operand (100458), 1)), __mysql_unsigned_shown (__mysql_bit_not (__mysql_bit_operand (20000101)))",
        ),
        (
            "SELECT ~0",
            "SELECT __mysql_unsigned_shown (__mysql_bit_not (0))",
        ),
        (
            "SELECT 1 FROM t WHERE (~ (a || 0)) IS NULL AND a | 1 IS NOT NULL AND ~a + 1 IS NULL",
            "SELECT 1 FROM t WHERE (__mysql_unsigned_shown (__mysql_bit_not (__mysql_bit_operand (a OR 0)))) ISNULL AND __mysql_unsigned_shown (__mysql_bit_or (__mysql_bit_operand (a), 1)) NOTNULL AND __mysql_unsigned_value (__mysql_bit_not (__mysql_bit_operand (a))) + 1 ISNULL",
        ),
        (
            "SELECT _utf8 'foobar' | 1, 0 & _latin1 ' 7x'",
            "SELECT __mysql_unsigned_shown (__mysql_bit_or (0, 1)), __mysql_unsigned_shown (__mysql_bit_and (0, 7))",
        ),
        (
            "SELECT 5 & 1.5, -1.5 >> '2.5x', 18446744073709540000e0 ^ b'1', 7 | 18446744073709551616, 9223372036854775808 << NULL",
            "SELECT __mysql_unsigned_shown (__mysql_bit_and (5, 2)), __mysql_unsigned_shown (__mysql_shift_right (- 2, 2)), __mysql_unsigned_shown (__mysql_bit_xor (9223372036854775807, 1)), __mysql_unsigned_shown (__mysql_bit_or (7, 9223372036854775807)), __mysql_unsigned_shown (__mysql_shift_left (- 9223372036854775808, NULL))",
        ),
        (
            "SELECT (a ^ RAND()) + 1, CAST(1 AS JSON) | 1 FROM t",
            "SELECT (__mysql_unsigned_value (__mysql_bit_xor (__mysql_bit_operand (a), __mysql_bit_operand (RAND ())))) + 1, __mysql_unsigned_value (__mysql_bit_or (__mysql_json_integer (__mysql_cast_as_json (1)), 1)) FROM t",
        ),
        ("SELECT TIME '-5 10:34:58' * 2", "SELECT - 1303458 * 2"),
        (
            "SELECT TIME '00:00:59.2132234' / 2",
            "SELECT 000059.213223 / 2",
        ),
        (
            "SELECT TIMESTAMP '2000-01-01 10:34:58.5' - 1",
            "SELECT 20000101103458.5 - 1",
        ),
        (
            "SELECT 1.5e0 <= TIME '-101:34:58'",
            "SELECT 1.5e0 <= - 1013458",
        ),
        (
            "SELECT TIMESTAMP '2000-01-01 10:34:58' = 20000101103458",
            "SELECT 20000101103458 = 20000101103458",
        ),
        (
            "SELECT DATE_ADD(d, INTERVAL 1 DAY), d - INTERVAL '1:1' MINUTE_SECOND",
            "SELECT DATE_ADD (d, 1, 'DAY'), DATE_SUB (d, '1:1', 'MINUTE_SECOND')",
        ),
        (
            "SELECT EXTRACT(YEAR_MONTH FROM d), TIMESTAMPDIFF(DAY, a, b)",
            "SELECT EXTRACT ('YEAR_MONTH', d), TIMESTAMPDIFF ('DAY', a, b)",
        ),
        (
            "SELECT j->'$.a', j->>'$.a'",
            "SELECT JSON_EXTRACT (j, '$.a'), JSON_UNQUOTE (JSON_EXTRACT (j, '$.a'))",
        ),
        (
            "SELECT GROUP_CONCAT(a, b SEPARATOR ';') FROM t",
            "SELECT GROUP_CONCAT (a || b, ';') FROM t",
        ),
        ("SELECT INTERVAL(5, 1, 10)", "SELECT INTERVAL (5, 1, 10)"),
        (
            "SELECT CAST(a AS JSON), CAST(a = 1 AS JSON), JSON_ARRAY(TRUE, a), 1 MEMBER OF (j)",
            "SELECT __mysql_cast_as_json (a), __mysql_cast_boolean_as_json (a = 1), JSON_ARRAY (__mysql_cast_boolean_as_json (TRUE), a), __mysql_member_of (1, j)",
        ),
        (
            "SELECT GET_FORMAT(DATE, 'USA'), UTC_DATE, LOCALTIMESTAMP()",
            "SELECT GET_FORMAT ('DATE', 'USA'), CURRENT_DATE, CURRENT_TIMESTAMP",
        ),
        (
            "SELECT MOD(7, 0xFF), IF(0x01, 'yes', 'no'), CONCAT(0x41)",
            "SELECT MOD (7, 255), \"IF\" (1, 'yes', 'no'), CONCAT (x'41')",
        ),
        (
            "SELECT -1.5 - CAST('[1,' AS JSON) AS v, CAST(CAST('x' AS JSON) AS DATE)",
            "SELECT __mysql_cast_as_json ('[1,') AS v, __mysql_cast_as_json ('x')",
        ),
        (
            "SELECT CAST('2000-01-01' AS DATE), CONVERT(1.5, DATETIME(3)), CAST('1:2' AS TIME), CAST(s AS CHAR(2)), CONVERT(s, NCHAR(1)), CAST(s AS NCHAR)",
            "SELECT __mysql_cast_as_date ('2000-01-01'), __mysql_cast_as_datetime (1.5, 3), __mysql_cast_as_time ('1:2', 0), __mysql_cast_as_char (s, 2), __mysql_cast_as_nchar (s, 1), CAST (s AS NCHAR)",
        ),
        (
            "SELECT LPAD(_latin1 0xFF, 3, 1), CONCAT('\u{4e2d}', _latin1 0x80), RPAD('\u{4e2d}', 2, _latin1 0xFF)",
            "SELECT LPAD ('\u{ff}', 3, 1), CONCAT ('\u{4e2d}', '\u{20ac}'), RPAD ('\u{4e2d}', 2, '\u{ff}')",
        ),
        (
            "SELECT CAST(CAST('[]' AS JSON) AS SIGNED), CONVERT(CAST(true AS JSON), DECIMAL(3, 1)), CAST(CAST(1.5 AS JSON) AS UNSIGNED), CAST(JSON_ARRAY() AS DOUBLE)",
            "SELECT __mysql_cast_as_signed (__mysql_json_integer (__mysql_cast_as_json ('[]'))), CAST (__mysql_json_decimal (__mysql_cast_boolean_as_json (TRUE)) AS DECIMAL (3, 1)), __mysql_cast_as_unsigned (1.5), CAST (__mysql_json_double (JSON_ARRAY ()) AS DOUBLE)",
        ),
        (
            "SELECT CAST(0xFF00 AS JSON), CAST(CAST(_binary 'a' AS JSON) AS SIGNED), UNIX_TIMESTAMP(CAST('[]' AS JSON)) + 1",
            "SELECT __mysql_cast_as_json (x'FF00'), __mysql_cast_as_signed (__mysql_json_integer (x'61')), CAST (UNIX_TIMESTAMP (__mysql_cast_as_json ('[]')) AS DECIMAL (65, 6)) + 1",
        ),
        (
            "SELECT COLLATION('a'), COLLATION(_latin1 'a' ), COLLATION(RANDOM_BYTES(1))",
            "SELECT 'utf8mb4_0900_ai_ci', 'latin1_swedish_ci', 'binary'",
        ),
        (
            "SELECT s REGEXP CAST('[1]' AS JSON)",
            "SELECT s REGEXP __mysql_cast_as_json ('[1]')",
        ),
        (
            "SELECT TRIM(LEADING 'ab' FROM s), TRIM(BOTH FROM s), TRIM(s), RTRIM(s)",
            "SELECT __mysql_ltrim (s, 'ab'), __mysql_trim (s, ' '), __mysql_trim (s, ' '), __mysql_rtrim (s, ' ')",
        ),
        (
            "SELECT ELT(0x2, 0x41), FIELD(0x61, 97), FIELD(0x61, 'a'), BIN(0x5), CONV(0xFF, 7, 0x10)",
            "SELECT ELT (2, x'41'), FIELD (97, 97), FIELD (x'61', 'a'), __mysql_conv_binary_literal (x'05', 10, 2), __mysql_conv_binary_literal (x'FF', 7, 16)",
        ),
        (
            "SELECT 0xAACC, HEX(0xAACC), -0x2, 0xff + b'1001', 0x1 / 7, X'40' + -1.5e0, ABS(_binary X'40'), _binary '1' + 1",
            "SELECT x'AACC', HEX (x'AACC'), -2e0, 255 + 9, 1 / 7, 64e0 + -1.5e0, ABS ('@'), 1e0 + 1",
        ),
        (
            "SELECT 9223372036854775808, -9223372036854775809, -9223372036854775808",
            "SELECT CAST ('9223372036854775808' AS DECIMAL (19, 0)), CAST ('-9223372036854775809' AS DECIMAL (19, 0)), - 9223372036854775808",
        ),
        (
            "SELECT CAST('[1]' AS JSON) = 1, JSON_ARRAY(1) <=> CAST(TRUE AS JSON), CAST(1 AS JSON) < DATE '2000-01-01'",
            "SELECT (__mysql_json_compare (__mysql_cast_as_json ('[1]'), 1) = 0), __mysql_json_null_safe_equal (JSON_ARRAY (1), __mysql_cast_boolean_as_json (TRUE)), (__mysql_json_compare (__mysql_cast_as_json (1), CAST ('2000-01-01' AS DATE)) < 0)",
        ),
        (
            "SELECT JSON_EXTRACT('0.4', '$') > 0, j -> '$.a' = 'x'",
            "SELECT (__mysql_json_compare (JSON_EXTRACT ('0.4', '$'), 0) > 0), (__mysql_json_compare (JSON_EXTRACT (j, '$.a'), 'x') = 0)",
        ),
        (
            "SELECT NOT CAST('0' AS JSON), -CAST('1' AS JSON), CAST('1' AS JSON) + 1, 1.5e0 % CAST('1.1' AS JSON)",
            "SELECT NOT __mysql_json_truth (__mysql_cast_as_json ('0')), ((__mysql_json_double (__mysql_cast_as_json ('1'))) * - 1), __mysql_json_double (__mysql_cast_as_json ('1')) + 1, __mysql_json_remainder (1.5e0, __mysql_json_double (__mysql_cast_as_json ('1.1')))",
        ),
        (
            "SELECT 0x61 LIKE 'A', 'a' NOT LIKE _binary 0xFF, 5 LIKE X'35'",
            "SELECT __mysql_like_bin (x'61', 'A', '\\'), NOT __mysql_like_bin ('a', x'FF', '\\'), __mysql_like_bin (5, x'35', '\\')",
        ),
        (
            "SELECT CAST('[1]' AS JSON) LIKE '[1]', CONCAT(CAST(TIME '12:34:56' AS JSON)), MD5(CAST(DATE '2000-01-01' AS JSON))",
            "SELECT __mysql_like_bin (__mysql_cast_as_json ('[1]'), '[1]', '\\'), CONCAT (__mysql_cast_as_json ('\"12:34:56.000000\"')), MD5 (__mysql_cast_as_json ('\"2000-01-01\"'))",
        ),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn lowering_rejects_each_unsupported_query_shape() {
    let cases = [
        SqlCase {
            name: "select modifier",
            sql: "SELECT SQL_NO_CACHE 1",
        },
        SqlCase {
            name: "straight join modifier",
            sql: "SELECT STRAIGHT_JOIN 1 FROM t",
        },
        SqlCase {
            name: "straight join",
            sql: "SELECT a FROM t1 STRAIGHT_JOIN t2",
        },
        SqlCase {
            name: "index hint",
            sql: "SELECT a FROM t USE INDEX (i)",
        },
        SqlCase {
            name: "partition selection",
            sql: "SELECT a FROM t PARTITION (p0)",
        },
        SqlCase {
            name: "into",
            sql: "SELECT 1 INTO @x",
        },
        SqlCase {
            name: "locking read",
            sql: "SELECT a FROM t FOR UPDATE",
        },
        SqlCase {
            name: "user variable",
            sql: "SELECT @a",
        },
        SqlCase {
            name: "system variable",
            sql: "SELECT @@version",
        },
        SqlCase {
            name: "setting hint",
            sql: "SELECT /*+ SET_VAR(sql_mode='') */ 1",
        },
        SqlCase {
            name: "collation",
            sql: "SELECT a COLLATE utf8mb4_general_ci FROM t",
        },
        SqlCase {
            name: "cast to date",
            sql: "SELECT CAST(a AS DATE) FROM t",
        },
        SqlCase {
            name: "latin1 text",
            sql: "SELECT _latin1'\u{e9}'",
        },
        SqlCase {
            name: "integer of a JSON string that holds digits",
            sql: r#"SELECT CAST(CAST('"12"' AS JSON) AS SIGNED)"#,
        },
        SqlCase {
            name: "byte length of latin1 text",
            sql: "SELECT LENGTH(_latin1 0xFF)",
        },
        SqlCase {
            name: "latin1 padded with text latin1 lacks",
            sql: "SELECT RPAD(_latin1 0xFF, 3, '\u{4e2d}')",
        },
        SqlCase {
            name: "latin1 padded to a length where its byte size decides the packet limit",
            sql: "SELECT LPAD(_latin1 0xFF, 20000000, 1)",
        },
        SqlCase {
            name: "latin1 text selected from a table",
            sql: "SELECT CONCAT(_latin1 0xFF) FROM t",
        },
        SqlCase {
            name: "latin1 text next to a binary string",
            sql: "SELECT REPLACE(0xC3BF, _latin1 0xFF, 'z')",
        },
        SqlCase {
            name: "odd hex digits",
            sql: "SELECT X'abc'",
        },
        SqlCase {
            name: "datetime literal",
            sql: "SELECT DATETIME '2020-01-01 00:00:00'",
        },
        SqlCase {
            name: "unsigned arithmetic below zero",
            sql: "SELECT 0x41 - 66",
        },
        SqlCase {
            name: "unsigned arithmetic inside arithmetic",
            sql: "SELECT (0x41 + 1) - 100",
        },
        SqlCase {
            name: "unsigned arithmetic on a column",
            sql: "SELECT 0x41 + a",
        },
        SqlCase {
            name: "bit operation on a negative temporal literal",
            sql: "SELECT TIME '-10:00:00' | 0",
        },
        SqlCase {
            name: "bit operation on a temporal literal with a fraction",
            sql: "SELECT TIME '10:00:00.5' & 1",
        },
        SqlCase {
            name: "LIKE under a case-sensitive collation",
            sql: "SELECT 'a' COLLATE utf8mb4_0900_as_cs LIKE 'A'",
        },
        SqlCase {
            name: "LOCATE under a case-sensitive collation",
            sql: "SELECT LOCATE('a' COLLATE utf8mb4_0900_as_cs, 'A')",
        },
        SqlCase {
            name: "collation of a latin1 string",
            sql: "SELECT _latin1 'a' COLLATE utf8mb4_0900_as_cs",
        },
        SqlCase {
            name: "ROUND of a BIGINT UNSIGNED literal",
            sql: "SELECT ROUND(18446744073709551615, -1)",
        },
        SqlCase {
            name: "decimal compared with a timestamp",
            sql: "SELECT 20000101103458.1234 >= TIMESTAMP '2000-01-01 10:34:58.123456'",
        },
        SqlCase {
            name: "time compared with a timestamp",
            sql: "SELECT TIMESTAMP '2000-01-01 10:34:58' <= TIME '00:00:59'",
        },
        SqlCase {
            name: "timestamps with different fractions compared",
            sql: "SELECT TIMESTAMP '2000-01-01 10:34:58.5' = TIMESTAMP '2000-01-01 10:34:58.50'",
        },
        SqlCase {
            name: "modulo of a temporal literal",
            sql: "SELECT 1.5e0 % TIMESTAMP '2000-01-01 10:34:58'",
        },
        SqlCase {
            name: "negated temporal literal with a fraction",
            sql: "SELECT -TIMESTAMP '2000-01-01 10:34:58.123456'",
        },
        SqlCase {
            name: "fraction that rounds into the seconds",
            sql: "SELECT TIME '10:00:00.9999995' + 0",
        },
        SqlCase {
            name: "div beyond bigint",
            sql: "SELECT 1 DIV 1e19",
        },
        SqlCase {
            name: "values without row",
            sql: "VALUES (1)",
        },
        SqlCase {
            name: "derived column list",
            sql: "SELECT * FROM (SELECT 1) AS x(a)",
        },
        SqlCase {
            name: "full join",
            sql: "SELECT a FROM t1 FULL OUTER JOIN t2 ON 1",
        },
        SqlCase {
            name: "except all",
            sql: "SELECT 1 EXCEPT ALL SELECT 1",
        },
        SqlCase {
            name: "temporal cast of a column whose type may be DATE or TIME",
            sql: "SELECT CAST(c AS DATE) FROM t",
        },
        SqlCase {
            name: "temporal cast of a TIME literal to a date",
            sql: "SELECT CAST(TIME '10:00:00' AS DATETIME)",
        },
        SqlCase {
            name: "temporal cast in arithmetic",
            sql: "SELECT CAST('2000-01-01' AS DATE) + 0",
        },
        SqlCase {
            name: "temporal cast in a comparison",
            sql: "SELECT CAST('2000-01-01' AS DATE) = '2000-1-1'",
        },
        SqlCase {
            name: "JSON cast of a temporal value",
            sql: "SELECT CAST(DATE '2000-01-01' AS JSON)",
        },
        SqlCase {
            name: "invalid JSON cast MySQL does not evaluate",
            sql: "SELECT 0 AND CAST('x' AS JSON)",
        },
        SqlCase {
            name: "invalid JSON cast evaluated once per row",
            sql: "SELECT CAST('x' AS JSON) + 1 FROM t",
        },
        SqlCase {
            name: "full-text search",
            sql: "SELECT MATCH (a) AGAINST ('x') FROM t",
        },
        SqlCase {
            name: "nulls first",
            sql: "SELECT a FROM t ORDER BY a NULLS FIRST",
        },
        SqlCase {
            name: "binary cast",
            sql: "SELECT BINARY s FROM t",
        },
        SqlCase {
            name: "recursive cte",
            sql: "WITH RECURSIVE c(n) AS (SELECT 1) SELECT n FROM c",
        },
        SqlCase {
            name: "empty regular expression",
            sql: "SELECT 'a' REGEXP ''",
        },
        SqlCase {
            name: "regular expression over bytes",
            sql: "SELECT 'a' REGEXP _binary 'a'",
        },
        SqlCase {
            name: "nested row value",
            sql: "SELECT ((1, 2), 3) = ((1, 2), 3)",
        },
        SqlCase {
            name: "uneven values rows",
            sql: "VALUES ROW(1), ROW(1, 2)",
        },
        SqlCase {
            name: "bytes that are not utf-8 joined with json",
            sql: "SELECT CONCAT(JSON_ARRAY(), 0xAACC)",
        },
        SqlCase {
            name: "bytes that are not utf-8 compared with text",
            sql: "SELECT 0xAACC = 'a'",
        },
        SqlCase {
            name: "a negated hex literal whose decimals show",
            sql: "SELECT -0x1 + 1.5",
        },
        SqlCase {
            name: "a binary string compared as a date",
            sql: "SELECT STRCMP(DATE '2000-01-01', 0xFF)",
        },
        SqlCase {
            name: "binary string in a bit operation",
            sql: "SELECT _binary '3' | 1",
        },
        SqlCase {
            name: "bit operation on a DOUBLE below the BIGINT range",
            sql: "SELECT -18446744073709540000e0 & 1",
        },
        SqlCase {
            name: "bit operation on a string with an exponent",
            sql: "SELECT '1e3' | 0",
        },
        SqlCase {
            name: "bit operation on a string above the BIGINT range",
            sql: "SELECT '18446744073709551615' | 0",
        },
        SqlCase {
            name: "column outside group by",
            sql: "SELECT a, b FROM t GROUP BY a",
        },
        SqlCase {
            name: "column beside an aggregate without group by",
            sql: "SELECT a, COUNT(*) FROM t",
        },
        SqlCase {
            name: "json compared with a number",
            sql: "SELECT CAST(a AS JSON) = 1",
        },
        SqlCase {
            name: "json in arithmetic",
            sql: "SELECT CAST(a AS JSON) + 1",
        },
        SqlCase {
            name: "json in a lazily evaluated function",
            sql: "SELECT CONCAT_WS(NULL, CAST(a AS JSON))",
        },
        SqlCase {
            name: "json compared with a column",
            sql: "SELECT CAST(1 AS JSON) = a",
        },
        SqlCase {
            name: "json of an integer above the signed range in a comparison",
            sql: "SELECT CAST(18446744073709551615 AS JSON) = 1",
        },
        SqlCase {
            name: "json time of more than a day read as text",
            sql: "SELECT CONCAT(CAST(TIME '25:00:00' AS JSON))",
        },
        SqlCase {
            name: "json date read as a date",
            sql: "SELECT DAYOFWEEK(CAST(DATE '2000-01-01' AS JSON))",
        },
        SqlCase {
            name: "temporal compared with an invalid string",
            sql: "SELECT DATE '2020-01-01' = 'x'",
        },
        SqlCase {
            name: "collations of a function result and a string",
            sql: "SELECT LENGTH('a' COLLATE utf8mb4_0900_as_cs) = 'a' COLLATE utf8mb4_0900_as_ci",
        },
        SqlCase {
            name: "invalid date in another format",
            sql: "SELECT DATE '2018/02/30'",
        },
    ];

    for case in cases {
        let error = match parse_and_translate(case.sql) {
            Ok(_) => panic!("{} unexpectedly lowered\nSQL: {}", case.name, case.sql),
            Err(error) => error,
        };
        assert!(
            matches!(error, MysqlParserError::Unsupported(_)),
            "SQL: {}\nerror: {error}",
            case.sql
        );
    }
}

#[test]
fn known_parser_gaps_return_errors() {
    let cases = ["SELECT 1 FOR UPDATE"];

    for sql in cases {
        let error = parse_one(sql).expect_err("known parser gap must return an error");
        assert!(
            matches!(error, MysqlParserError::Parse(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
}

#[test]
fn lowering_rejects_unsupported_statement_families() {
    let cases = [
        "TRUNCATE TABLE users",
        "CREATE OR REPLACE VIEW v AS SELECT 1",
        "DROP VIEW a, b",
        "ALTER TABLE users ADD INDEX name_key (name)",
        "ALTER TABLE users MODIFY COLUMN name VARCHAR(20)",
        "ALTER TABLE users ADD COLUMN a INT, ADD COLUMN b INT",
        "ALTER TABLE users ADD COLUMN a INT FIRST",
        "DROP TABLE a, b",
        "DROP TEMPORARY TABLE users",
        "CREATE INDEX name_key ON users (name(10))",
        "INSERT IGNORE INTO users VALUES (1)",
        "INSERT INTO users VALUES (DEFAULT)",
        "INSERT INTO users SET id = 1, name = id",
        "INSERT INTO users SET id = 1, id = 2",
        "INSERT INTO app.users VALUES (1)",
        "INSERT INTO users (id) VALUES ()",
        "INSERT INTO a SELECT * FROM b ON DUPLICATE KEY UPDATE a.i = b.j",
        "INSERT INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = a + 1, b = a",
        "INSERT INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = 1, a = 2",
        "INSERT INTO t VALUES (1, 2) AS new ON DUPLICATE KEY UPDATE a = new.a",
        "INSERT IGNORE INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = 1",
        "SHOW TABLES",
        "DESCRIBE users",
        "EXPLAIN SELECT 1",
        "USE app",
        "SET @answer = 42",
        "SET NAMES latin1",
        "SET GLOBAL time_zone = 'UTC'",
        "SET time_zone = '+01:00'",
        "SET sql_mode = ''",
        "SET autocommit = 0",
        "SET FOREIGN_KEY_CHECKS = 2",
        "SET FOREIGN_KEY_CHECKS = 0, FOREIGN_KEY_CHECKS = 1",
        "START TRANSACTION",
        "COMMIT",
        "ROLLBACK",
    ];

    for sql in cases {
        let error = parse_and_translate(sql).expect_err("unsupported statement must be rejected");
        assert!(
            matches!(error, MysqlParserError::Unsupported(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
}

#[test]
fn lowers_views() {
    let cases = [
        (
            "CREATE VIEW v AS SELECT id, name FROM users WHERE id > 1",
            "CREATE VIEW v AS SELECT id, name FROM users WHERE id > 1",
        ),
        (
            "CREATE ALGORITHM=MERGE DEFINER=`root`@`localhost` SQL SECURITY INVOKER VIEW `v` (`a`, b) AS SELECT 1, 2",
            "CREATE VIEW `v` (`a`, b) AS SELECT 1, 2",
        ),
        ("DROP VIEW v", "DROP VIEW v"),
        ("DROP VIEW IF EXISTS v", "DROP VIEW IF EXISTS v"),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn lowers_insert_forms() {
    let cases = [
        (
            "INSERT INTO t VALUES (1, 'a'), (2, \"b\")",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
        ),
        (
            "INSERT LOW_PRIORITY INTO t (a) VALUES (?)",
            "INSERT INTO t (a) VALUES (?1)",
        ),
        (
            "INSERT t SET a = 1, b = 'x'",
            "INSERT INTO t (a, b) VALUES (1, 'x')",
        ),
        (
            "INSERT INTO t (a) SELECT b FROM u WHERE b > 1",
            "INSERT INTO t (a) SELECT b FROM u WHERE b > 1",
        ),
        ("INSERT INTO t () VALUES ()", "INSERT INTO t DEFAULT VALUES"),
        (
            "REPLACE INTO t (a, b) VALUES (1, 'x')",
            "REPLACE INTO t (a, b) VALUES (1, 'x')",
        ),
        ("REPLACE t SET a = 1", "REPLACE INTO t (a) VALUES (1)"),
        (
            "REPLACE INTO t SELECT * FROM u",
            "REPLACE INTO t SELECT * FROM u",
        ),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn lowers_on_duplicate_key_update_to_an_upsert() {
    let cases = [
        (
            "INSERT INTO t (a, b) VALUES (1, 'x'), (?, \"y\") ON DUPLICATE KEY UPDATE b = VALUES(b), t.c = c + ?",
            "INSERT INTO t (a, b) VALUES (1, 'x'), (?1, 'y') ON CONFLICT DO UPDATE SET b = excluded.b, c = c + ?2",
        ),
        (
            "INSERT INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = a + 1, b = VALUES(a)",
            "INSERT INTO t VALUES (1, 2) ON CONFLICT DO UPDATE SET a = a + 1, b = excluded.a",
        ),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn lowers_single_table_update_and_delete() {
    let cases = [
        ("UPDATE users SET id = 2", "UPDATE users SET id = 2"),
        (
            "UPDATE users SET qty = qty + 1, name = 'Ada' WHERE id = 3",
            "UPDATE users SET qty = qty + 1, name = 'Ada' WHERE id = 3",
        ),
        (
            "UPDATE `users` SET `display``name` = 'x' WHERE `id` = 1",
            "UPDATE `users` SET `display``name` = 'x' WHERE `id` = 1",
        ),
        (
            "UPDATE users SET users.qty = 1 WHERE users.id > 2",
            "UPDATE users SET qty = 1 WHERE users.id > 2",
        ),
        (
            "UPDATE users AS u SET u.qty = 1 WHERE u.id > 2",
            "UPDATE users AS u SET qty = 1 WHERE u.id > 2",
        ),
        (
            "UPDATE users u SET qty = 1",
            "UPDATE users AS u SET qty = 1",
        ),
        (
            "UPDATE app.users SET qty = 1",
            "UPDATE app.users SET qty = 1",
        ),
        (
            "UPDATE users SET qty = ?, name = ? WHERE id = ?",
            "UPDATE users SET qty = ?1, name = ?2 WHERE id = ?3",
        ),
        (
            "UPDATE users SET qty = 1, name = other + 1",
            "UPDATE users SET qty = 1, name = other + 1",
        ),
        (
            "UPDATE users SET qty = (SELECT MAX(qty) FROM stock WHERE stock.id = users.id)",
            "UPDATE users SET qty = (SELECT MAX (qty) FROM stock WHERE stock.id = users.id)",
        ),
        (
            "UPDATE /*+ NO_RANGE_OPTIMIZATION(users) */ users SET qty = 1",
            "UPDATE users SET qty = 1",
        ),
        ("UPDATE quick SET qty = 1", "UPDATE quick SET qty = 1"),
        ("DELETE FROM users", "DELETE FROM users"),
        (
            "DELETE FROM users WHERE id = 3",
            "DELETE FROM users WHERE id = 3",
        ),
        (
            "DELETE FROM users AS u WHERE u.id = 3",
            "DELETE FROM users AS u WHERE u.id = 3",
        ),
        (
            "DELETE FROM app.users WHERE id = ?",
            "DELETE FROM app.users WHERE id = ?1",
        ),
        (
            "DELETE FROM users WHERE id IN (SELECT user_id FROM banned)",
            "DELETE FROM users WHERE id IN (SELECT user_id FROM banned)",
        ),
        ("DELETE FROM quick", "DELETE FROM quick"),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn rejects_update_and_delete_shapes_without_a_faithful_lowering() {
    let cases = [
        SqlCase {
            name: "assignment reads an earlier assignment",
            sql: "UPDATE users SET qty = 1, name = qty + 1",
        },
        SqlCase {
            name: "swap reads an earlier assignment",
            sql: "UPDATE users SET a = b, b = a",
        },
        SqlCase {
            name: "qualified read of an earlier assignment",
            sql: "UPDATE users SET qty = 1, name = users.qty",
        },
        SqlCase {
            name: "subquery after an assignment",
            sql: "UPDATE users SET qty = 1, name = (SELECT name FROM other)",
        },
        SqlCase {
            name: "same column twice",
            sql: "UPDATE users SET qty = 1, QTY = 2",
        },
        SqlCase {
            name: "default value",
            sql: "UPDATE users SET qty = DEFAULT",
        },
        SqlCase {
            name: "tuple assignment",
            sql: "UPDATE users SET (a, b) = (1, 2)",
        },
        SqlCase {
            name: "assignment to another table",
            sql: "UPDATE users SET other.qty = 1",
        },
        SqlCase {
            name: "assignment through the table name of an aliased target",
            sql: "UPDATE users AS u SET users.qty = 1",
        },
        SqlCase {
            name: "update order by",
            sql: "UPDATE users SET qty = 1 ORDER BY id",
        },
        SqlCase {
            name: "update limit",
            sql: "UPDATE users SET qty = 1 LIMIT 1",
        },
        SqlCase {
            name: "update low priority",
            sql: "UPDATE LOW_PRIORITY users SET qty = 1",
        },
        SqlCase {
            name: "update ignore",
            sql: "UPDATE IGNORE users SET qty = 1",
        },
        SqlCase {
            name: "multi-table update",
            sql: "UPDATE users JOIN other ON users.id = other.id SET users.qty = other.qty",
        },
        SqlCase {
            name: "update returning",
            sql: "UPDATE users SET qty = 1 RETURNING qty",
        },
        SqlCase {
            name: "update partition",
            sql: "UPDATE users PARTITION (p0) SET qty = 1",
        },
        SqlCase {
            name: "update index hint",
            sql: "UPDATE users USE INDEX (i) SET qty = 1",
        },
        SqlCase {
            name: "update setting hint",
            sql: "UPDATE /*+ SET_VAR(sql_mode='') */ users SET qty = 1",
        },
        SqlCase {
            name: "update subquery reads the target",
            sql: "UPDATE users SET qty = 1 WHERE id IN (SELECT id FROM users)",
        },
        SqlCase {
            name: "delete order by limit",
            sql: "DELETE FROM users ORDER BY id LIMIT 1",
        },
        SqlCase {
            name: "delete limit",
            sql: "DELETE FROM users LIMIT 1",
        },
        SqlCase {
            name: "delete low priority",
            sql: "DELETE LOW_PRIORITY FROM users",
        },
        SqlCase {
            name: "delete quick",
            sql: "DELETE QUICK FROM users",
        },
        SqlCase {
            name: "delete ignore",
            sql: "DELETE IGNORE FROM users",
        },
        SqlCase {
            name: "multi-table delete",
            sql: "DELETE users FROM users JOIN other ON users.id = other.id",
        },
        SqlCase {
            name: "delete using",
            sql: "DELETE FROM users USING users JOIN other",
        },
        SqlCase {
            name: "delete returning",
            sql: "DELETE FROM users RETURNING id",
        },
        SqlCase {
            name: "delete partition",
            sql: "DELETE FROM users PARTITION (p0)",
        },
        SqlCase {
            name: "delete subquery reads the target",
            sql: "DELETE FROM users WHERE id IN (SELECT id FROM users)",
        },
        SqlCase {
            name: "delete derived table reads the target",
            sql: "DELETE FROM users WHERE id IN (SELECT id FROM (SELECT id FROM users) AS d)",
        },
    ];

    for case in cases {
        let error = match parse_and_translate(case.sql) {
            Ok(statement) => panic!(
                "{} unexpectedly lowered to `{statement}`\nSQL: {}",
                case.name, case.sql
            ),
            Err(error) => error,
        };
        assert!(
            matches!(error, MysqlParserError::Unsupported(_)),
            "{}\nSQL: {}\nerror: {error}",
            case.name,
            case.sql
        );
    }
}

#[test]
fn lowers_session_settings_of_the_target_profile() {
    let cases = [
        ("SET NAMES utf8mb4", "SELECT 0 WHERE 0"),
        ("SET @@session.time_zone = 'UTC', autocommit = ON", "SELECT 0 WHERE 0"),
        (
            "SET sql_mode = 'NO_ENGINE_SUBSTITUTION,ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO'",
            "SELECT 0 WHERE 0",
        ),
        ("SET FOREIGN_KEY_CHECKS = 0", "PRAGMA foreign_keys = 0"),
        ("SET time_zone = '+00:00', foreign_key_checks = ON", "PRAGMA foreign_keys = 1"),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn lowers_numbers_with_the_same_text_as_the_sqlite_parser() {
    let cases = [
        "SELECT 1.5",
        "SELECT 1e3",
        "SELECT 18446744073709551615 + 1",
        "SELECT 9223372036854775808 * 2",
    ];

    for sql in cases {
        let statement = parse_and_translate(sql).expect("numeric literal must lower");
        assert_eq!(statement.to_string(), sql);
    }
}

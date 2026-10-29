use turso_mysql_parser::{
    parse, parse_and_translate, parse_and_translate_in, parse_one, translate, ColumnFacts,
    Environment, ForeignKeyOf, ForeignKeyTo, MysqlParserError,
};
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
            "CREATE TABLE t (e ENUM ('''x'',''y''') NOT NULL DEFAULT 'x', s \"SET\" ('''r'',''w''') DEFAULT 'r,w', j JSON, t TINYTEXT COLLATE \"und-u-ks-level1\") STRICT",
        ),
        (
            "CREATE TABLE t (e ENUM('a,b','c''d') COLLATE utf8mb4_0900_bin, s SET('x') COLLATE utf8mb4_0900_as_ci)",
            "CREATE TABLE t (e ENUM ('''a,b'',''c''''d''', 1), s \"SET\" ('''x''')) STRICT",
        ),
        (
            "CREATE TABLE t (d DATE DEFAULT '2020-1-2', ts DATETIME(3) DEFAULT '2020-01-02 03:04:05', c TIMESTAMP DEFAULT CURRENT_TIMESTAMP, r DATETIME DEFAULT '2020-12-31 23:59:59.5')",
            "CREATE TABLE t (d DATE DEFAULT '2020-01-02', ts DATETIME (3) DEFAULT '2020-01-02 03:04:05.000', c TIMESTAMP DEFAULT CURRENT_TIMESTAMP, r DATETIME DEFAULT '2021-01-01 00:00:00') STRICT",
        ),
        (
            "CREATE TABLE t (a DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6), b TIMESTAMP(3) DEFAULT NOW(3), c DATETIME DEFAULT CURRENT_TIMESTAMP(0))",
            "CREATE TABLE t (a DATETIME (6) NOT NULL DEFAULT (NOW (6)), b TIMESTAMP (3) DEFAULT (NOW (3)), c DATETIME DEFAULT CURRENT_TIMESTAMP) STRICT",
        ),
        (
            "CREATE TABLE t (id INT, email VARCHAR(50), KEY k (id), UNIQUE KEY uq (email), CONSTRAINT ck CHECK (id > 0), CONSTRAINT fk FOREIGN KEY (email) REFERENCES t (email) ON DELETE CASCADE)",
            "CREATE TABLE t (id INT, email VARCHAR (50) COLLATE \"und-u-ks-level1\", CONSTRAINT uq UNIQUE (email), CONSTRAINT ck CHECK (id > 0), CONSTRAINT fk FOREIGN KEY (email) REFERENCES t (email) ON DELETE CASCADE) STRICT",
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
            "CREATE TABLE t (j JSON, b INT GENERATED ALWAYS AS (JSON_UNQUOTE (JSON_EXTRACT (j, '$.b'))) VIRTUAL) STRICT",
        ),
        (
            "CREATE UNIQUE INDEX idx USING BTREE ON t (a DESC, b)",
            "CREATE UNIQUE INDEX idx ON t (a DESC, b)",
        ),
        (
            "CREATE UNIQUE INDEX idx ON t ((C1*10), c2, ((c1+c2)*-2) DESC)",
            "CREATE UNIQUE INDEX idx ON t (C1 * 10, c2, (c1 + c2) * - 2 DESC)",
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
            name: "column named rowid",
            sql: "CREATE TABLE users(RowId INT)",
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
            name: "auto increment on a double",
            sql: "CREATE TABLE users(id DOUBLE AUTO_INCREMENT PRIMARY KEY)",
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
            name: "set with more than 64 members",
            sql: "CREATE TABLE users(flags SET('m0','m1','m2','m3','m4','m5','m6','m7','m8','m9','m10','m11','m12','m13','m14','m15','m16','m17','m18','m19','m20','m21','m22','m23','m24','m25','m26','m27','m28','m29','m30','m31','m32','m33','m34','m35','m36','m37','m38','m39','m40','m41','m42','m43','m44','m45','m46','m47','m48','m49','m50','m51','m52','m53','m54','m55','m56','m57','m58','m59','m60','m61','m62','m63','m64'))",
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
            name: "current timestamp with other fractional digits than the column",
            sql: "CREATE TABLE users(created DATETIME(6) DEFAULT CURRENT_TIMESTAMP(3))",
        },
        SqlCase {
            name: "current timestamp beyond microseconds",
            sql: "CREATE TABLE users(created DATETIME(6) DEFAULT CURRENT_TIMESTAMP(7))",
        },
        SqlCase {
            name: "default reading a user variable",
            sql: "ALTER TABLE users ADD COLUMN i INT DEFAULT (@a)",
        },
        SqlCase {
            name: "generated column reading a system variable",
            sql: "CREATE TABLE users(i INT AS (@@version))",
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
            name: "key of strings and two BIGINTs longer than 3072 bytes",
            sql: "CREATE TABLE t (a VARCHAR(255), b VARCHAR(255), c VARCHAR(255), e BIGINT, f BIGINT, UNIQUE KEY k (a, b, c, e, f))",
        },
        SqlCase {
            name: "key of strings and a DATETIME(6) longer than 3072 bytes",
            sql: "CREATE TABLE t (a VARCHAR(766), e DATETIME(6), f TINYINT, UNIQUE KEY k (a, e, f))",
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
            name: "collated STRCMP of a date and a string that is not a date",
            sql: "SELECT STRCMP(DATE '2000-01-01', 'foobar' COLLATE utf8mb4_0900_as_cs)",
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
fn measures_key_parts_by_their_innodb_size() {
    for columns in [
        "e TINYINT, f TINYINT",
        "e INT, f INT",
        "e INT UNSIGNED, f MEDIUMINT",
        "e DECIMAL(10, 2), f YEAR",
        "e DATETIME(2), f DATE",
        "e TIMESTAMP(6), f FLOAT",
        "e TIME(6), f BIT(9)",
        "e DOUBLE",
        "e ENUM('x'), f SET('x', 'y')",
    ] {
        let sql = format!(
            "CREATE TABLE t (a VARCHAR(255), b VARCHAR(255), c VARCHAR(255), {columns}, UNIQUE KEY k (a, b, c, e{}))",
            if columns.contains("f ") { ", f" } else { "" }
        );
        parse_and_translate(&sql).unwrap_or_else(|error| panic!("{sql}: {error}"));
    }
}

#[test]
fn lowers_long_and_or_chains_as_balanced_trees() {
    for operator in ["OR", "AND", "||"] {
        let terms = (1..=2000)
            .map(|n| format!("(a = {n})"))
            .collect::<Vec<_>>()
            .join(&format!(" {operator} "));
        let sql = format!("DELETE FROM t WHERE {terms}");
        parse_and_translate(&sql).unwrap_or_else(|error| panic!("{operator}: {error}"));
    }
    let lowered = parse_and_translate("SELECT a OR b OR c OR d AND e AND f")
        .expect("a chain lowers")
        .to_string();
    assert_eq!(lowered, "SELECT a OR b OR c OR d AND e AND f");
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
        SqlCase {
            name: "binary string that is not UTF-8 next to a JSON value",
            sql: "SELECT CONCAT(CAST(1 AS JSON), 0xFF)",
        },
        SqlCase {
            name: "a binary string that is not UTF-8 under an explicit collation in LIKE",
            sql: "SELECT 0xFF LIKE 'a' COLLATE utf8mb4_0900_as_cs",
        },
        SqlCase {
            name: "a string latin1 lacks converted to a latin1 subject",
            sql: "SELECT LPAD(_latin1 0xFF, -1, '中')",
        },
        SqlCase {
            name: "a string latin1 lacks compared with a latin1 FIELD subject",
            sql: "SELECT FIELD(_latin1 0xFF, 'a', '中')",
        },
        SqlCase {
            name: "STRCMP of a timestamp and a string that is not a date",
            sql: "SELECT STRCMP(timestamp '2000-01-01 10:34:58', '20000101foo')",
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

    for sql in ["SELECT :named"] {
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
        (
            "SELECT @a, @@version, @@SESSION.autocommit",
            "SELECT __mysql_user_variable ('a'), '8.4.10', 1",
        ),
        (
            "SELECT JSON_ARRAYAGG(a), JSON_OBJECTAGG(b, a) FROM t",
            "SELECT __mysql_json_arrayagg (json_group_array (JSON_ARRAY (a))), __mysql_json_objectagg (json_group_array (JSON_OBJECT (b, a))) FROM t",
        ),
        (
            "SELECT /*+ MAX_EXECUTION_TIME(2000) SET_VAR(sort_buffer_size=262144) */ id FROM t",
            "SELECT id FROM t",
        ),
        (
            "SELECT i.id FROM t AS i USE INDEX (a) JOIN u FORCE INDEX FOR JOIN (PRIMARY, b) IGNORE KEY (c)",
            "SELECT i.id FROM t AS i JOIN u",
        ),
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
            "SELECT -0.0 + -0xffff, -X'00' + -X'00', -0 + -0xffffffffffffffff, CASE WHEN TRUE THEN 0 WHEN FALSE THEN 9e0 ELSE -0x1 END",
            "SELECT '-65535.0', '-0', '-18446744073709552000', CASE WHEN TRUE THEN 0e0 WHEN FALSE THEN 9e0 ELSE -1e0 END",
        ),
        (
            "SELECT 0xFF DIV 13, '64' DIV 0x1",
            "SELECT CAST (255 / 13 AS INTEGER), CAST ('64' / 1 AS INTEGER)",
        ),
        (
            "SELECT 64 = '64', 1e0 <=> ' 1.5e1x', -1 < _utf8mb4 '-0.5', '1e999' > 0",
            "SELECT 6.4e1 = 6.4e1, 1e0 IS 1.5e1, - 1e0 < - 5e-1, 1.7976931348623157e308 > 0e0",
        ),
        (
            "SELECT BINARY s, CAST(X'FF' AS BINARY(2)), CONVERT(-1.5, BINARY(1)), CAST(CAST('[1]' AS JSON) AS BINARY), CAST(1e0 AS CHAR), STRCMP(BINARY 'a', s) FROM t ORDER BY BINARY s",
            "SELECT CAST (CONCAT (s) AS BLOB), __mysql_cast_as_binary (x'FF', 2), __mysql_cast_as_binary (CAST (CONCAT (CAST ('-1.5' AS DECIMAL (2, 1))) AS BLOB), 1), CAST (CAST (CONCAT (__mysql_cast_as_json ('[1]')) AS TEXT) AS BLOB), CAST (CONCAT (1e0) AS TEXT), STRCMP (CAST (CONCAT ('a') AS BLOB), s) FROM t ORDER BY CAST (CONCAT (s) AS BLOB)",
        ),
        (
            "SELECT DATE_SUB(TIMESTAMP'2025-01-01 00:00:00', INTERVAL '1.0x' HOUR_MICROSECOND), DATE_ADD(20250101, INTERVAL 6/4 DAY_HOUR), TIMESTAMP '2025-01-01 00:00:00.1' + INTERVAL 1.5000 SECOND, DATE_ADD(DATE '2020-01-01', INTERVAL 1 DAY), DATE '2020-01-01' - INTERVAL 1 HOUR",
            "SELECT __mysql_cast_as_datetime (DATE_SUB ('2025-01-01 00:00:00', '1.0x', 'HOUR_MICROSECOND'), 6), DATE_ADD (20250101, CONCAT (6 / 4), 'DAY_HOUR'), __mysql_cast_as_datetime (DATE_ADD ('2025-01-01 00:00:00.1', 1.5000, 'SECOND'), 4), DATE_ADD ('2020-01-01', 1, 'DAY'), __mysql_cast_as_datetime (DATE_SUB ('2020-01-01', 1, 'HOUR'), 0)",
        ),
        (
            "SELECT SEC_TO_TIME(1.1), MAKETIME(1.5, 2.4, TIME '00:00:59.01011'), FROM_UNIXTIME(0.0), CONVERT_TZ(20000101103458.1234, '-01:00', '-00:00'), TIME(DATE '2000-01-01'), MAKEDATE(2000, -2.5), SEC_TO_TIME(s) FROM t",
            "SELECT __mysql_cast_as_time (SEC_TO_TIME (1.1), 1), __mysql_cast_as_time (MAKETIME (2, 2, 000059.01011), 5), __mysql_cast_as_datetime (FROM_UNIXTIME (0.0), 1), __mysql_cast_as_datetime (CONVERT_TZ ('20000101103458.1234', '-01:00', '-00:00'), 4), '00:00:00', MAKEDATE (2000, - 3), SEC_TO_TIME (s) FROM t",
        ),
        (
            "SELECT DISTINCT a AS x FROM t ORDER BY x, 1, a",
            "SELECT DISTINCT a AS x FROM t ORDER BY x, 1, a",
        ),
        (
            "SELECT a FROM t WHERE b = 3 GROUP BY a ORDER BY b, t.a",
            "SELECT a FROM t WHERE b = 3 GROUP BY a ORDER BY b, t.a",
        ),
        (
            "SELECT COUNT(*) FROM t ORDER BY b",
            "SELECT COUNT (*) FROM t ORDER BY b",
        ),
        (
            "SELECT MOD(18446744073709551615, 7), 1.5e0 % 1, ABS(9223372036854775808), 9223372036854775808 - 1",
            "SELECT MOD (CAST ('18446744073709551615' AS DECIMAL (20, 0)), 7), MOD (1.5e0, 1), ABS (CAST ('9223372036854775808' AS DECIMAL (19, 0))), 9223372036854775807",
        ),
        (
            "SELECT IF(a, 1.5, -0x1), CASE WHEN a THEN 7 ELSE -0x1 END, MOD(-0x1, 1.0), -255 % -0x1, MOD(3.141592653589793, -0x1), '64' % -0x1 FROM t",
            "SELECT \"IF\" (a, '1.5', '-1.0'), CASE WHEN a THEN '7' ELSE '-1' END, '-0.0', '-0', '0.141592653589793', MOD ('64', -1e0) FROM t",
        ),
        (
            "SELECT TIME '10:04:58' * 0b1001, NULL * 1 + x'09', 18446744073709551615 + 0x0, -9223372036854775808 % 0x9, TIME '5 10:34:58.9' / 0xFF666F6F626172FF, TRUE AND 0xFF666F6F626172FF, _utf8 'foobar' + x'09'",
            "SELECT 904122, NULL * 1 + 9, CAST ('18446744073709551615' AS DECIMAL (20, 0)), -8, 1303458.9 / CAST ('18403519451478717183' AS DECIMAL (20, 0)), TRUE AND 1, 0e0 + 9e0",
        ),
        (
            "SELECT 'abc' COLLATE utf8mb4_0900_as_cs LIKE 'A%', 1.5 NOT LIKE _utf8mb4 '1._' COLLATE utf8mb4_bin",
            "SELECT __mysql_like_bin ('abc', 'A%', '\\'), NOT __mysql_like_bin (1.5, '1._', '\\')",
        ),
        (
            "SELECT PERIOD_ADD(18446744073709551616, 1), PERIOD_DIFF(1, -9223372036854775809), PERIOD_ADD(9223372036854775808, 1)",
            "SELECT PERIOD_ADD (9223372036854775807, 1), PERIOD_DIFF (1, (-9223372036854775807 - 1)), PERIOD_ADD (9223372036854775808, 1)",
        ),
        (
            "SELECT UNIX_TIMESTAMP(CAST(TIME '12:00:00' AS JSON)), UNIX_TIMESTAMP(1.10) + 1, UNIX_TIMESTAMP(-1.5e0), UNIX_TIMESTAMP(20000101103458.1234), UNIX_TIMESTAMP('foo'), UNIX_TIMESTAMP(_binary '2000-01-01 00:00:00.12'), UNIX_TIMESTAMP('2000-01-01'), UNIX_TIMESTAMP(1)",
            "SELECT CAST (UNIX_TIMESTAMP (0e0) AS DECIMAL (65, 6)), CAST (UNIX_TIMESTAMP (1.10) AS DECIMAL (13, 2)) + 1, CAST (UNIX_TIMESTAMP (-1.5e0) AS DECIMAL (17, 6)), CAST (UNIX_TIMESTAMP ('20000101103458.1234') AS DECIMAL (15, 4)), CAST (UNIX_TIMESTAMP ('foo') AS DECIMAL (17, 6)), CAST (UNIX_TIMESTAMP ('2000-01-01 00:00:00.12') AS DECIMAL (13, 2)), UNIX_TIMESTAMP ('2000-01-01'), UNIX_TIMESTAMP (1)",
        ),
        (
            "SELECT -'foobar', - _utf8 '0000-01-03', -('1'), '' * TIME '-10:04:58', ' 255 ' * 2, '1' / 3, 'a' + 'b', a IN (-''), -'' = 0 FROM t",
            "SELECT - 0e0, - 0e0, - 1e0, 0e0 * - 100458, 2.55e2 * 2, 1e0 / 3, 0e0 + 0e0, a IN ((('') * - 1)), - 0e0 = 0 FROM t",
        ),
        (
            "SELECT ROUND(X'ff', 255), TRUNCATE(0b1, -1), ROUND(0x40)",
            "SELECT ROUND (255e0, 255), TRUNCATE (1e0, - 1), ROUND (64e0)",
        ),
        (
            "SELECT CASE 'FOO' WHEN 'foo' THEN 1 ELSE 0 END, CASE 'FOO' WHEN _latin1 'foo' THEN 1 WHEN 'x y' THEN 2 END, _latin1 'FOO' = 'foo', _latin1 'a b' = 'A B', CASE a WHEN 'x' THEN 1 END FROM t",
            "SELECT CASE 'FOO' COLLATE \"und-u-ks-level1\" WHEN 'foo' THEN 1 ELSE 0 END, CASE 'FOO' COLLATE \"und-u-ks-level1\" WHEN 'foo' THEN 1 WHEN 'x y' THEN 2 END, 'FOO' COLLATE \"und-u-ks-level1\" = 'foo', 'a b' = 'A B', CASE a WHEN 'x' THEN 1 END FROM t",
        ),
        (
            "SELECT FIELD(18446744073709551615, -1, 9), FIELD(18446744073709551615, 1.0, 18446744073709551616), FIELD(9223372036854775807, 1.0e0, 9223372036854775808), 18446744073709551615 IN (-9223372036854775808, 18446744073709551616), 2 NOT IN (1, 2.0), CASE 18446744073709551615 WHEN 18446744073709551616 THEN 1 ELSE 0 END, -1 IN (18446744073709551615), 9223372036854775808 IN (9223372036854775807, 9e0), -9223372036854775809 NOT IN (-9223372036854775808, 1.5e0)",
            "SELECT 1, 0, 2, 0, 0, CASE WHEN 0 THEN 1 ELSE 0 END, - 1 IN (18446744073709551615), 0, 1",
        ),
        (
            "SELECT BIN(18446744073709551616), OCT(-9223372036854775810.4), CONV(9223372036854775808, 13, 9e0), CONV(18446744073709551615, -10, 10), BIN(9223372036854775807), HEX(9223372036854775808), BIT_COUNT(18446744073709551615), HEX(255)",
            "SELECT BIN ('18446744073709551616'), OCT ('-9223372036854775810.4'), CONV ('9223372036854775808', 13, 9e0), CONV ('18446744073709551615', - 10, 10), BIN (9223372036854775807), '8000000000000000', 64, HEX (255)",
        ),
        (
            "SELECT TIMESTAMPADD(SECOND, 1.5, TIMESTAMP '2020-12-31 23:59:59'), TIMESTAMPADD(second, '1.0', DATE '2018-05-01'), TIMESTAMPADD(SECOND, 6/4, TIMESTAMP '2020-12-31 23:59:59.12'), TIMESTAMPADD(SECOND, 1, '2020-01-01'), TIMESTAMPADD(SECOND, 1.5, a), TIMESTAMPADD(second, '1.0', '20250101') FROM t",
            "SELECT __mysql_cast_as_datetime (TIMESTAMPADD ('SECOND', 1.5, '2020-12-31 23:59:59'), 1), __mysql_cast_as_datetime (TIMESTAMPADD ('SECOND', '1.0', '2018-05-01'), 6), __mysql_cast_as_datetime (TIMESTAMPADD ('SECOND', 6 / 4, '2020-12-31 23:59:59.12'), 4), TIMESTAMPADD ('SECOND', 1, '2020-01-01'), TIMESTAMPADD ('SECOND', 1.5, a), TIMESTAMPADD ('SECOND', '1.0', '20250101') FROM t",
        ),
        (
            "SELECT MICROSECOND(20000101103458.1234), CAST(20000101103458.1234 AS DATETIME(6)), DAY(20000101103458.5), MINUTE(2000010110345.1234)",
            "SELECT MICROSECOND ('20000101103458.1234'), __mysql_cast_as_datetime ('20000101103458.1234', 6), DAY (20000101103458.5), MINUTE (2000010110345.1234)",
        ),
        (
            "SELECT 18446744073709551615 * 1 - NULL",
            "SELECT CAST ('18446744073709551615' AS DECIMAL (20, 0)) - NULL",
        ),
        (
            "SELECT ROUND(18446744073709551615, -1.5), TRUNCATE(9223372036854775808, -0x1), ROUND(9223372036854775808, 'a')",
            "SELECT CAST ('18446744073709551600' AS DECIMAL (20, 0)), 9223372036854775800, CAST ('9223372036854775808' AS DECIMAL (19, 0))",
        ),
        (
            "SELECT '1.5' % TIME '-10:04:58', 1.5e0 % TIMESTAMP '2000-01-01 10:34:58.5'",
            "SELECT MOD ('1.5', - 100458), MOD (1.5e0, 20000101103458.5)",
        ),
        (
            "SELECT 0xff REGEXP 1, TIME '10:04:58' REGEXP _binary 'a'",
            "SELECT x'FF'REGEXP 1, x'31303A30343A3538'REGEXP x'61'",
        ),
        (
            "SELECT CAST(CAST(0.000 AS JSON) AS DATE)",
            "SELECT NULL",
        ),
        (
            "SELECT 'FOO' IN ('foo', 1), 64 NOT IN ('64', 'bar'), CASE 'abc' WHEN 'xyz' THEN 1 WHEN 0 THEN 2 END, GREATEST(-1, 0x1, TIMESTAMP '2020-12-31 23:59:59')",
            "SELECT ('FOO' COLLATE \"und-u-ks-level1\" = 'foo' OR 0e0 = 1e0), NOT (6.4e1 = 6.4e1 OR 6.4e1 = 0e0), CASE 0e0 WHEN 0e0 THEN 1 WHEN 0e0 THEN 2 END, '2020-12-31 23:59:59'",
        ),
        (
            "SELECT GREATEST(DATE '2023-02-01', 'Abc' COLLATE utf8mb4_0900_as_ci, CAST(2 AS UNSIGNED)), GREATEST('foobar', TIMESTAMP '2020-12-31 23:59:59'), GREATEST(-1.0, 9223372036854775807, DATE '2100-02-01', TIMESTAMP '2020-12-31 23:59:59'), GREATEST(DATE '2024-02-18', 'x', NULL)",
            "SELECT '2023-02-01', '2020-12-31 23:59:59.000000', '2100-02-01 00:00:00.0', NULL",
        ),
        (
            "SELECT LEAST(DATE '2100-02-01', 'Abc' COLLATE utf8mb4_0900_as_ci, 'FOOBAR'), STRCMP(TIME '00:00:59', '12$4' COLLATE utf8mb4_0900_ai_ci)",
            "SELECT __mysql_collated_least ('utf8mb4_0900_as_ci', '2100-02-01', 'Abc' COLLATE \"und-u-ks-level2\", 'FOOBAR'), __mysql_collated_strcmp ('utf8mb4_0900_ai_ci', '00:00:59', '12$4' COLLATE \"und-u-ks-level1\")",
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
            "SELECT TIME '-10:04:58.5' | 1, TIME '00:59:59.5' & 1, TIMESTAMP '2000-01-01 10:34:58.4' ^ 0",
            "SELECT __mysql_unsigned_shown (__mysql_bit_or (__mysql_bit_operand (- 100459), 1)), __mysql_unsigned_shown (__mysql_bit_and (__mysql_bit_operand (10000), 1)), __mysql_unsigned_shown (__mysql_bit_xor (__mysql_bit_operand (20000101103458), 0))",
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
            "SELECT __mysql_cast_as_date ('2000-01-01'), __mysql_cast_as_datetime (1.5, 3), __mysql_cast_as_time ('1:2', 0), __mysql_cast_as_char (CONCAT (s), 2), __mysql_cast_as_nchar (CONCAT (s), 1), __mysql_cast_as_nchar (CONCAT (s), 16777216)",
        ),
        (
            "SELECT REPLACE(0xC3BF, _latin1 0xFF, 'z'), LPAD(_binary 'ab', 3, _latin1 0xFF), LOCATE(_latin1 0xFF, 0xAAFF, 1) FROM t",
            "SELECT \"REPLACE\" (x'C3BF', x'FF', 'z'), LPAD (x'6162', 3, x'FF'), LOCATE (x'FF', x'AAFF', 1) FROM t",
        ),
        (
            "SELECT LPAD(_latin1 0xFF, 3, 0xAACC), INSERT(_latin1 0xFF, X'00', 1, _binary 'a'), LOCATE(_latin1 0xFF, 1, 1.5e0), FIELD(_latin1 0xFF, 'a', ''), CONCAT_WS(_latin1 0xFF, TIME '10:00:00', NULL * 1, _utf8mb4 'a')",
            "SELECT LPAD ('\u{ff}', 3, '\u{aa}\u{cc}'), \"INSERT\" ('\u{ff}', 0, 1, 'a'), LOCATE ('\u{ff}', 1, 1.5e0), FIELD ('\u{ff}', 'a', ''), CONCAT_WS ('\u{ff}', '10:00:00', NULL * 1, 'a')",
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
            "SELECT x'AACC', HEX (x'AACC'), -2e0, 264, 1 / 7, 64e0 + -1.5e0, ABS ('@'), 1e0 + 1",
        ),
        (
            "SELECT 9223372036854775808, -9223372036854775809, -9223372036854775808",
            "SELECT CAST ('9223372036854775808' AS DECIMAL (19, 0)), CAST ('-9223372036854775809' AS DECIMAL (19, 0)), (-9223372036854775807 - 1)",
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
            "SELECT CAST(TIME '5 12:34:58' AS JSON), CONCAT(CAST(TIME '-32:34:58.5' AS JSON)), CAST(CAST(TIMESTAMP '2000-01-01 10:00:00' AS JSON) AS CHAR)",
            "SELECT __mysql_cast_as_json ('\"04:34:58.000000\"'), CONCAT (__mysql_cast_as_json ('\"-00:34:58.500000\"')), CAST (__mysql_cast_as_json ('\"2000-01-01 10:00:00.000000\"') AS CHAR)",
        ),
        (
            "SELECT MAKEDATE(CAST(DATE '2000-01-01' AS JSON), 2), WEEK(CAST(TIMESTAMP '2000-01-01 10:00:00' AS JSON), CAST(TIME '10:00:00' AS JSON)), HOUR(CAST(TIME '32:00:00' AS JSON)), DATE(CAST(TIME '10:00:00' AS JSON)), TIME(CAST(DATE '2000-01-01' AS JSON))",
            "SELECT MAKEDATE (0e0, 2), WEEK (__mysql_cast_as_datetime ('2000-01-01 10:00:00.000000', 6), 0e0), HOUR (__mysql_cast_as_time ('00:00:00.000000', 6)), DATE (NULL), TIME (NULL)",
        ),
        (
            "SELECT CAST(18446744073709551615 AS JSON) = 1, 7 DIV CAST(1.5 AS JSON), CONCAT_WS(CAST(0 AS JSON), 'a', _binary 'b'), CAST(CAST(NULL - 1 AS JSON) AS SIGNED)",
            "SELECT (__mysql_json_compare (__mysql_cast_as_json ('18446744073709551615'), 1) = 0), CAST (7 / __mysql_json_decimal (__mysql_cast_as_json (1.5)) AS INTEGER), CONCAT_WS (__mysql_cast_as_json (0), 'a', x'62'), __mysql_cast_as_signed (__mysql_json_integer (__mysql_cast_as_json (NULL - 1)))",
        ),
        (
            "SELECT CONCAT_WS(0xAACC, 'a', 'b'), 'a' < 0xFF, X'00' <=> _binary 'x', CAST(0xFF AS CHAR), CAST(0xFF AS DATE), FROM_UNIXTIME(0xFF)",
            "SELECT CONCAT_WS (x'AACC', 'a', 'b'), x'61'< x'FF', x'00'IS x'78', NULL, __mysql_cast_as_date (x'FF'), FROM_UNIXTIME (x'FF')",
        ),
        (
            "SELECT 0x0 <= CAST('{}' AS JSON), CAST(0 AS JSON) LIKE 0x0",
            "SELECT (__mysql_json_compare (x'00', __mysql_cast_as_json ('{}')) <= 0), __mysql_like_bin (__mysql_cast_as_json (0), x'00', '\\')",
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
            name: "partition selection",
            sql: "SELECT a FROM t PARTITION (p0)",
        },
        SqlCase {
            name: "into",
            sql: "SELECT 1 INTO @x",
        },
        SqlCase {
            name: "locking read of a named table",
            sql: "SELECT a FROM t FOR UPDATE OF t",
        },
        SqlCase {
            name: "global system variable",
            sql: "SELECT @@global.sql_mode",
        },
        SqlCase {
            name: "system variable outside the target profile",
            sql: "SELECT @@innodb_buffer_pool_size",
        },
        SqlCase {
            name: "json aggregate as a window function",
            sql: "SELECT JSON_ARRAYAGG(a) OVER () FROM t",
        },
        SqlCase {
            name: "setting hint",
            sql: "SELECT /*+ SET_VAR(sql_mode='') */ 1",
        },
        SqlCase {
            name: "setting hint after a plan setting",
            sql: "SELECT /*+ SET_VAR(sort_buffer_size=262144) SET_VAR(div_precision_increment=8) */ 1",
        },
        SqlCase {
            name: "malformed setting hint",
            sql: "SELECT /*+ SET_VAR */ 1",
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
            name: "latin1 padded with text latin1 lacks to a column's length",
            sql: "SELECT RPAD(_latin1 0xFF, a, '\u{4e2d}') FROM t",
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
            name: "latin1 text in a call next to a binary string",
            sql: "SELECT CONCAT(0xAA, UPPER(_latin1 0xFF))",
        },
        SqlCase {
            name: "latin1 located after a position with an empty needle",
            sql: "SELECT LOCATE('', _latin1 0xFF, 3)",
        },
        SqlCase {
            name: "latin1 located among letters with the same base letter",
            sql: "SELECT LOCATE('y', _latin1 0xFF)",
        },
        SqlCase {
            name: "latin1 inserted at a position past its characters but within its UTF-8 bytes",
            sql: "SELECT INSERT(_latin1 0xFF, 2, 1, 'a')",
        },
        SqlCase {
            name: "latin1 field compared with trailing spaces",
            sql: "SELECT FIELD(_latin1 0xFF, 'a ')",
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
            name: "bit operation on a TIME literal rounded past its range",
            sql: "SELECT TIME '838:59:59.5' | 0",
        },
        SqlCase {
            name: "bit operation on a TIMESTAMP literal rounded into the next day",
            sql: "SELECT TIMESTAMP '2000-01-01 23:59:59.5' & 1",
        },
        SqlCase {
            name: "LIKE under a case-sensitive collation beyond printable ASCII",
            sql: "SELECT 'a\u{e9}' COLLATE utf8mb4_0900_as_cs LIKE 'A%'",
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
            name: "lateral derived column list",
            sql: "SELECT * FROM t, LATERAL (SELECT t.a) AS x(a)",
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
            name: "JSON date in a function without a known read",
            sql: "SELECT ABS(CAST(DATE '2000-01-01' AS JSON))",
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
            name: "recursive cte",
            sql: "WITH RECURSIVE c(n) AS (SELECT 1) SELECT n FROM c",
        },
        SqlCase {
            name: "empty regular expression",
            sql: "SELECT 'a' REGEXP ''",
        },
        SqlCase {
            name: "regular expression over bytes and a column",
            sql: "SELECT a REGEXP _binary 'a' FROM t",
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
            name: "a DECIMAL a double cannot hold in a date function",
            sql: "SELECT SEC_TO_TIME(12345678901.123456789)",
        },
        SqlCase {
            name: "a DATE_ADD of a TIME literal",
            sql: "SELECT DATE_ADD(TIME '10:00:00', INTERVAL 1 HOUR)",
        },
        SqlCase {
            name: "a DATE_ADD whose interval decimals are not known",
            sql: "SELECT DATE_ADD(TIMESTAMP '2020-01-01 00:00:00', INTERVAL c SECOND) FROM t",
        },
        SqlCase {
            name: "a decimal a JSON double cannot show",
            sql: "SELECT CAST(1.50 AS JSON)",
        },
        SqlCase {
            name: "a binary string cast to JSON",
            sql: "SELECT CAST(BINARY 'a' AS JSON)",
        },
        SqlCase {
            name: "BIGINT UNSIGNED arithmetic outside its range",
            sql: "SELECT 18446744073709551615 + 1",
        },
        SqlCase {
            name: "ORDER BY a column outside GROUP BY",
            sql: "SELECT BINARY s FROM t GROUP BY BINARY s ORDER BY s",
        },
        SqlCase {
            name: "ORDER BY a column outside a DISTINCT select list",
            sql: "SELECT DISTINCT BINARY t1.id AS id FROM t AS t1 ORDER BY t1.id",
        },
        SqlCase {
            name: "a binary string cast in a comparison",
            sql: "SELECT BINARY 1 = 1",
        },
        SqlCase {
            name: "a binary string cast compared with a column",
            sql: "SELECT 1 FROM t WHERE b = CAST('d' AS BINARY(3))",
        },
        SqlCase {
            name: "a binary string cast in a comparing function",
            sql: "SELECT GREATEST(BINARY s, s) FROM t",
        },
        SqlCase {
            name: "a negated hex literal whose decimals show",
            sql: "SELECT CONCAT(-0x1 + 1.5)",
        },
        SqlCase {
            name: "GREATEST of a date and a string MySQL may read as a date",
            sql: "SELECT GREATEST(DATE '2100-02-01', '2020-01-01')",
        },
        SqlCase {
            name: "GREATEST of a date and a time",
            sql: "SELECT GREATEST(DATE '2100-02-01', TIME '10:00:00', 'x')",
        },
        SqlCase {
            name: "collated LEAST of a date and a string MySQL may read as a date",
            sql: "SELECT LEAST(DATE '2100-02-01', '2000-01-01' COLLATE utf8mb4_0900_as_ci)",
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
            sql: "SELECT CAST(18446744073709551616 AS JSON) = 1",
        },
        SqlCase {
            name: "json date as the hour of MAKETIME with DECIMAL seconds",
            sql: "SELECT MAKETIME(CAST(DATE '2000-01-01' AS JSON), 0, 1.5)",
        },
        SqlCase {
            name: "negative json time that prints as zero",
            sql: "SELECT CAST(TIME '-32:00:00' AS JSON)",
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
        "CREATE INDEX k ON users ((name))",
        "CREATE INDEX k ON users ((LOWER(name)))",
        "CREATE INDEX k ON users ((a / 2))",
        "ALTER TABLE users MODIFY COLUMN name VARCHAR(20)",
        "ALTER TABLE users ADD COLUMN a INT, ADD COLUMN b INT",
        "ALTER TABLE users ADD COLUMN a INT FIRST",
        "DROP TABLE a, b",
        "DROP TEMPORARY TABLE users",
        "CREATE INDEX name_key ON users (name(10))",
        "INSERT IGNORE INTO users () VALUES ()",
        "INSERT INTO users SET id = 1, name = id",
        "INSERT INTO users SET id = 1, id = 2",
        "INSERT INTO app.users VALUES (1)",
        "INSERT INTO users (id) VALUES ()",
        "INSERT INTO a SELECT * FROM b ON DUPLICATE KEY UPDATE a.i = b.j",
        "INSERT INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = a + 1, b = a",
        "INSERT INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = 1, a = 2",
        "INSERT INTO t VALUES (1, 2) AS new ON DUPLICATE KEY UPDATE a = new.a",
        "INSERT IGNORE INTO t VALUES (1, 2) ON DUPLICATE KEY UPDATE a = 1",
        "REPLACE IGNORE INTO t VALUES (1, 2)",
        "SHOW TABLES FROM app",
        "SHOW TABLES WHERE Tables_in_main = 'a'",
        r"SHOW TABLES LIKE 'a\_b'",
        "SHOW COLUMNS FROM users",
        "DESCRIBE users",
        "EXPLAIN SELECT 1",
        "USE app",
        "SET @a = 1, @b = @a + 1",
        "SET @a = 1, FOREIGN_KEY_CHECKS = 0",
        "SET NAMES latin1",
        "SET GLOBAL time_zone = 'UTC'",
        "SET time_zone = '+01:00'",
        "SET sql_mode = ''",
        "SET autocommit = 0",
        "SET FOREIGN_KEY_CHECKS = 2",
        "SET FOREIGN_KEY_CHECKS = 0, FOREIGN_KEY_CHECKS = 1",
        "START TRANSACTION READ ONLY",
        "COMMIT AND CHAIN",
        "ROLLBACK AND CHAIN",
        "SET TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "SET GLOBAL TRANSACTION ISOLATION LEVEL READ COMMITTED",
        "SET SESSION TRANSACTION READ ONLY",
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE",
    ];

    for sql in cases {
        let error = parse_and_translate(sql).expect_err("unsupported statement must be rejected");
        assert!(
            matches!(error, MysqlParserError::Unsupported(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
}

/// A session outside a transaction whose only index is `idx` of table `t`.
struct IndexOfT;

impl Environment for IndexOfT {
    fn in_transaction(&self) -> bool {
        false
    }

    fn index_table(&self, name: &str) -> Result<Option<String>, MysqlParserError> {
        Ok(name.eq_ignore_ascii_case("idx").then(|| "t".to_string()))
    }

    fn has_triggers(&self) -> bool {
        false
    }

    fn triggers_on(&self, _table: &str) -> Result<Vec<String>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_keys_to(&self, _table: &str) -> Result<Vec<ForeignKeyTo>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn columns_of(&self, _table: &str) -> Result<Option<Vec<ColumnFacts>>, MysqlParserError> {
        Ok(None)
    }

    fn table_sql(&self, _table: &str) -> Result<Option<String>, MysqlParserError> {
        Ok(None)
    }

    fn unique_keys_of(&self, _table: &str) -> Result<Vec<Vec<String>>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_key_names(&self) -> Result<Vec<String>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_key_checks(&self) -> bool {
        true
    }

    fn foreign_keys_of(&self, _table: &str) -> Result<Vec<ForeignKeyOf>, MysqlParserError> {
        Ok(Vec::new())
    }
}

/// A session outside a transaction on tables `a (id AUTO_INCREMENT, x, g
/// generated)`, `b` and `c`, where `c` references `b` with `ON DELETE
/// CASCADE`, `a` has trigger `a1` (after insert, writing `b`) and `c` has `c1`
/// (after delete, writing `log`).
struct TriggerSchema;

impl Environment for TriggerSchema {
    fn in_transaction(&self) -> bool {
        false
    }

    fn index_table(&self, _name: &str) -> Result<Option<String>, MysqlParserError> {
        Ok(None)
    }

    fn has_triggers(&self) -> bool {
        true
    }

    fn triggers_on(&self, table: &str) -> Result<Vec<String>, MysqlParserError> {
        Ok(match table {
            "a" => vec![
                "CREATE TRIGGER a1 AFTER INSERT ON a FOR EACH ROW BEGIN INSERT INTO b VALUES (NEW.x); END"
                    .to_string(),
            ],
            "c" => vec![
                "CREATE TRIGGER c1 AFTER DELETE ON c FOR EACH ROW BEGIN INSERT INTO log VALUES (OLD.y); END"
                    .to_string(),
            ],
            _ => Vec::new(),
        })
    }

    fn foreign_keys_to(&self, table: &str) -> Result<Vec<ForeignKeyTo>, MysqlParserError> {
        Ok(match table {
            "b" => vec![ForeignKeyTo {
                table: "c".to_string(),
                on_delete: ast::RefAct::Cascade,
                on_update: ast::RefAct::NoAction,
            }],
            _ => Vec::new(),
        })
    }

    fn columns_of(&self, table: &str) -> Result<Option<Vec<ColumnFacts>>, MysqlParserError> {
        let column = |name: &str, auto_increment, generated| ColumnFacts {
            name: name.to_string(),
            auto_increment,
            generated,
            default: None,
            not_null: false,
        };
        Ok(match table {
            "a" => Some(vec![
                column("id", true, false),
                column("x", false, false),
                column("g", false, true),
            ]),
            "b" | "c" | "log" => Some(vec![column("y", false, false)]),
            _ => None,
        })
    }

    fn table_sql(&self, _table: &str) -> Result<Option<String>, MysqlParserError> {
        Ok(None)
    }

    fn unique_keys_of(&self, _table: &str) -> Result<Vec<Vec<String>>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_key_names(&self) -> Result<Vec<String>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_key_checks(&self) -> bool {
        true
    }

    fn foreign_keys_of(&self, _table: &str) -> Result<Vec<ForeignKeyOf>, MysqlParserError> {
        Ok(Vec::new())
    }
}

#[test]
fn lowers_triggers_that_fire_as_mysql_s() {
    let cases = [
        (
            "CREATE TRIGGER b1 BEFORE INSERT ON b FOR EACH ROW INSERT INTO log VALUES (NEW.y * 2)",
            "CREATE TRIGGER b1 BEFORE INSERT ON b FOR EACH ROW BEGIN INSERT INTO log VALUES (NEW.y * 2); END",
        ),
        (
            "CREATE TRIGGER a2 BEFORE UPDATE ON a FOR EACH ROW UPDATE b SET y = y + OLD.x + NEW.x",
            "CREATE TRIGGER a2 BEFORE UPDATE ON a FOR EACH ROW BEGIN UPDATE b SET y = y + OLD.x + NEW.x; END",
        ),
        (
            "CREATE TRIGGER a3 AFTER DELETE ON a FOR EACH ROW DELETE FROM b WHERE y = OLD.x",
            "CREATE TRIGGER a3 AFTER DELETE ON a FOR EACH ROW BEGIN DELETE FROM b WHERE y = OLD.x; END",
        ),
        ("DROP TRIGGER a1", "DROP TRIGGER a1"),
        ("DROP TRIGGER IF EXISTS a1", "DROP TRIGGER IF EXISTS a1"),
        ("INSERT INTO a (x) VALUES (1)", "INSERT INTO a (x) VALUES (1)"),
        ("DELETE FROM c", "DELETE FROM c"),
    ];
    for (sql, expected) in cases {
        let statement = parse_and_translate_in(sql, &TriggerSchema)
            .unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        let text = statement.to_string();
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(text, expected, "SQL: {sql}");
    }

    let unsupported = [
        "CREATE TRIGGER a4 AFTER INSERT ON a FOR EACH ROW INSERT INTO log VALUES (1)",
        "CREATE TRIGGER a4 BEFORE INSERT ON a FOR EACH ROW INSERT INTO log VALUES (NEW.id)",
        "CREATE TRIGGER a4 BEFORE UPDATE ON a FOR EACH ROW INSERT INTO log VALUES (NEW.g)",
        "CREATE TRIGGER a4 BEFORE INSERT ON a FOR EACH ROW SET NEW.x = NEW.x + 1",
        "CREATE TRIGGER a4 BEFORE INSERT ON a FOR EACH ROW FOLLOWS a1 INSERT INTO log VALUES (1)",
        "INSERT INTO a (x) SELECT y FROM b",
        "INSERT IGNORE INTO a (x) VALUES (1)",
        "REPLACE INTO a (x) VALUES (1)",
        "DELETE FROM b",
        "ALTER TABLE b ADD COLUMN z INT",
        "RENAME TABLE b TO d",
    ];
    for sql in unsupported {
        let error = parse_and_translate_in(sql, &TriggerSchema)
            .expect_err("a trigger that would fire unlike MySQL's is refused");
        assert!(
            matches!(
                error,
                MysqlParserError::Unsupported(_) | MysqlParserError::Parse(_)
            ),
            "SQL: {sql}\nerror: {error}"
        );
    }

    let rejected = [
        "CREATE TRIGGER b2 AFTER INSERT ON b FOR EACH ROW INSERT INTO log VALUES (OLD.y)",
        "CREATE TRIGGER b2 AFTER DELETE ON b FOR EACH ROW INSERT INTO log VALUES (NEW.y)",
        "CREATE TRIGGER b2 BEFORE UPDATE ON b FOR EACH ROW SET OLD.y = 1",
        "CREATE TRIGGER b2 AFTER UPDATE ON b FOR EACH ROW SET NEW.y = 1",
        "CREATE TRIGGER b2 AFTER UPDATE ON b FOR EACH ROW SELECT 1",
    ];
    for sql in rejected {
        let error = parse_and_translate_in(sql, &TriggerSchema).expect_err("MySQL rejects it");
        assert!(
            matches!(error, MysqlParserError::Rejected(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
}

#[test]
fn rejects_what_mysql_rejects_when_it_resolves_names() {
    for sql in [
        "INSERT INTO t (a, b, A) VALUES (1, 2, 3)",
        "REPLACE INTO t (a, a) SELECT 1, 2",
        "SELECT a FROM t WHERE a IN (SELECT a FROM u LIMIT 2)",
        "SELECT a FROM t WHERE a NOT IN (SELECT a FROM u ORDER BY a LIMIT 1)",
        "SELECT a FROM t WHERE a = ANY (SELECT a FROM u LIMIT 1)",
        "SELECT * FROM t AS x, u AS x",
        "SELECT * FROM t, t",
        "SELECT * FROM (SELECT 1 AS a) AS d, (SELECT 2 AS a) AS d",

        "SELECT * FROM t AS x, x",
        "SELECT * FROM t UNION SELECT * FROM u AS x JOIN v AS x ON 1",
        "SELECT a AS c, b AS c FROM t ORDER BY c",
        "SELECT * FROM t t0, t t1, t t2, t t3, t t4, t t5, t t6, t t7, t t8, t t9, t t10, t t11, t t12, t t13, t t14, t t15, t t16, t t17, t t18, t t19, t t20, t t21, t t22, t t23, t t24, t t25, t t26, t t27, t t28, t t29, t t30, t t31, t t32, t t33, t t34, t t35, t t36, t t37, t t38, t t39, t t40, t t41, t t42, t t43, t t44, t t45, t t46, t t47, t t48, t t49, t t50, t t51, t t52, t t53, t t54, t t55, t t56, t t57, t t58, t t59, t t60, t t61",
    ] {
        let error = parse_and_translate(sql).expect_err("MySQL rejects it");
        assert!(
            matches!(error, MysqlParserError::Rejected(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
    for sql in [
        "SELECT a FROM t WHERE a IN (SELECT a FROM (SELECT a FROM u LIMIT 2) AS d)",
        "SELECT a FROM t WHERE EXISTS (SELECT a FROM u LIMIT 1)",
        "SELECT * FROM t AS x JOIN t AS y ON x.a = y.a",
        "SELECT * FROM t JOIN (SELECT 1 AS a) AS t",
        "SELECT * FROM (SELECT 1 AS a) AS t JOIN t",
        "SELECT a AS c, a AS c FROM t ORDER BY c",
        "SELECT a AS c, b AS d FROM t ORDER BY c",
    ] {
        parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
    }
}

#[test]
fn drops_an_index_of_the_named_table_only() {
    for sql in [
        "DROP INDEX idx ON t",
        "DROP INDEX IDX ON T",
        "ALTER TABLE t DROP INDEX idx",
    ] {
        let statement = parse_and_translate_in(sql, &IndexOfT)
            .unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert!(
            statement.to_string().eq_ignore_ascii_case("DROP INDEX idx"),
            "SQL: {sql}"
        );
    }
    for sql in [
        "DROP INDEX idx ON u",
        "ALTER TABLE u DROP INDEX idx",
        "DROP INDEX other ON t",
    ] {
        let error = parse_and_translate_in(sql, &IndexOfT).expect_err(sql);
        assert!(
            matches!(error, MysqlParserError::Rejected(_)),
            "SQL: {sql}\nerror: {error}"
        );
    }
    let error =
        parse_and_translate("DROP INDEX idx ON t").expect_err("an empty database has no index");
    assert!(matches!(error, MysqlParserError::Rejected(_)), "{error}");
}

#[test]
fn lowers_transactions_for_a_session_outside_one() {
    let cases = [
        ("START TRANSACTION", "BEGIN IMMEDIATE"),
        ("BEGIN", "BEGIN IMMEDIATE"),
        ("BEGIN WORK", "BEGIN IMMEDIATE"),
        ("COMMIT", "SELECT 0 WHERE 0"),
        ("ROLLBACK", "SELECT 0 WHERE 0"),
        ("SAVEPOINT a", "SELECT 0 WHERE 0"),
        ("SELECT a FROM t FOR UPDATE", "SELECT a FROM t"),
        ("SELECT a FROM t FOR SHARE SKIP LOCKED", "SELECT a FROM t"),
        (
            "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
            "SELECT 0 WHERE __mysql_set_transaction_isolation ('READ-COMMITTED')",
        ),
        (
            "set local transaction isolation level serializable",
            "SELECT 0 WHERE __mysql_set_transaction_isolation ('SERIALIZABLE')",
        ),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }

    for sql in ["ROLLBACK TO SAVEPOINT a", "RELEASE SAVEPOINT a"] {
        let error =
            parse_and_translate(sql).expect_err("no savepoint exists outside a transaction");
        assert!(
            matches!(error, MysqlParserError::Rejected(_)),
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
fn lowers_insert_ignore_to_an_upsert_that_does_nothing() {
    let cases = [
        (
            "INSERT IGNORE INTO t VALUES (1, 2), (3, 4)",
            "INSERT INTO t VALUES (1, 2), (3, 4) ON CONFLICT DO NOTHING",
        ),
        (
            "INSERT IGNORE t (a) SELECT b FROM u",
            "INSERT INTO t (a) SELECT b FROM u ON CONFLICT DO NOTHING",
        ),
        (
            "INSERT IGNORE INTO t SET a = 1",
            "INSERT INTO t (a) VALUES (1) ON CONFLICT DO NOTHING",
        ),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        assert_eq!(statement.to_string(), expected, "SQL: {sql}");
    }
}

#[test]
fn lowers_show_tables_to_a_query_of_the_schema() {
    let names = "SELECT __mysql_schema_name(sql) AS name, type FROM sqlite_schema \
                 WHERE type IN ('table', 'view') AND lower(substr(name, 1, 7)) <> 'sqlite_' \
                 AND substr(name, 1, 16) <> '__turso_internal'";
    let cases = [
        (
            "SHOW TABLES".to_string(),
            format!("SELECT name AS Tables_in_main FROM ({names}) ORDER BY name"),
        ),
        (
            "SHOW FULL TABLES LIKE 'a%_[*?'".to_string(),
            format!(
                "SELECT name AS Tables_in_main, CASE type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END AS Table_type \
                 FROM ({names}) WHERE name GLOB 'a*?[[][*][?]' ORDER BY name"
            ),
        ),
    ];

    for (sql, expected) in cases {
        let statement =
            parse_and_translate(&sql).unwrap_or_else(|error| panic!("{sql} must lower: {error}"));
        let Ok(Some(ast::Cmd::Stmt(expected))) =
            turso_parser::parser::Parser::new(expected.as_bytes()).next_cmd()
        else {
            panic!("the expected SQL of {sql} must parse as a statement");
        };
        assert_eq!(statement.to_string(), expected.to_string(), "SQL: {sql}");
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
        (
            "UPDATE users USE INDEX (i) SET qty = 1",
            "UPDATE users SET qty = 1",
        ),
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
        (
            "UPDATE users SET qty = ? WHERE id > ? ORDER BY name DESC, id LIMIT ?",
            "UPDATE users SET qty = ?1 WHERE rowid IN (SELECT rowid FROM users WHERE id > ?2 ORDER BY name DESC, id LIMIT ?3)",
        ),
        (
            "UPDATE users AS u SET qty = 1 ORDER BY u.id LIMIT 2",
            "UPDATE users AS u SET qty = 1 WHERE rowid IN (SELECT rowid FROM users AS u ORDER BY u.id LIMIT 2)",
        ),
        (
            "DELETE FROM users WHERE qty = 0 ORDER BY id LIMIT 1",
            "DELETE FROM users WHERE rowid IN (SELECT rowid FROM users WHERE qty = 0 ORDER BY id LIMIT 1)",
        ),
        (
            "DELETE users FROM users JOIN banned ON banned.user_id = users.id WHERE banned.until > ?",
            "DELETE FROM users WHERE rowid IN (SELECT users.rowid FROM users JOIN banned ON banned.user_id = users.id WHERE banned.until > ?1)",
        ),
        (
            "DELETE u FROM users AS u LEFT JOIN orders o ON o.user_id = u.id WHERE o.id IS NULL",
            "DELETE FROM users WHERE rowid IN (SELECT u.rowid FROM users AS u LEFT JOIN orders o ON o.user_id = u.id WHERE o.id ISNULL)",
        ),
        (
            "DELETE FROM users USING users JOIN banned ON banned.user_id = users.id",
            "DELETE FROM users WHERE rowid IN (SELECT users.rowid FROM users JOIN banned ON banned.user_id = users.id)",
        ),
        (
            "UPDATE users JOIN stock ON stock.id = users.id SET users.qty = stock.qty WHERE stock.qty > ?",
            "UPDATE users AS __mysql_target SET qty = __mysql_rows.__mysql_value_0 FROM (SELECT users.rowid AS __mysql_rowid, stock.qty AS __mysql_value_0, count (1) AS __mysql_matches FROM users JOIN stock ON stock.id = users.id WHERE (stock.qty > ?1) AND users.rowid NOTNULL GROUP BY users.rowid) AS __mysql_rows WHERE __mysql_target.rowid = __mysql_rows.__mysql_rowid AND __mysql_one_joined_row (__mysql_rows.__mysql_matches)",
        ),
        (
            "UPDATE users u JOIN stock s ON s.id = u.id SET u.qty = u.qty + 1, u.name = 'x'",
            "UPDATE users AS __mysql_target SET qty = __mysql_rows.__mysql_value_0, name = __mysql_rows.__mysql_value_1 FROM (SELECT u.rowid AS __mysql_rowid, u.qty + 1 AS __mysql_value_0, 'x' AS __mysql_value_1, count (1) AS __mysql_matches FROM users u JOIN stock s ON s.id = u.id WHERE u.rowid NOTNULL GROUP BY u.rowid) AS __mysql_rows WHERE __mysql_target.rowid = __mysql_rows.__mysql_rowid",
        ),
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
            name: "update ordered by a column it assigns",
            sql: "UPDATE users SET id = id + 1 ORDER BY id DESC",
        },
        SqlCase {
            name: "update ordered by a position",
            sql: "UPDATE users SET qty = 1 ORDER BY 1 LIMIT 1",
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
            name: "multi-table update of two tables",
            sql: "UPDATE users JOIN other ON users.id = other.id SET users.qty = 1, other.qty = 2",
        },
        SqlCase {
            name: "multi-table update of an unqualified column",
            sql: "UPDATE users JOIN other ON users.id = other.id SET qty = 1",
        },
        SqlCase {
            name: "multi-table update of a derived table",
            sql: "UPDATE users JOIN (SELECT 1 AS id) AS d ON users.id = d.id SET d.id = 2",
        },
        SqlCase {
            name: "multi-table update with an aggregate",
            sql: "UPDATE users JOIN other ON users.id = other.id SET users.qty = COUNT(*)",
        },
        SqlCase {
            name: "multi-table update whose subquery reads the target",
            sql: "UPDATE users JOIN other ON users.id = other.id SET users.qty = (SELECT MAX(qty) FROM users)",
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
            name: "update setting hint",
            sql: "UPDATE /*+ SET_VAR(sql_mode='') */ users SET qty = 1",
        },
        SqlCase {
            name: "update subquery reads the target",
            sql: "UPDATE users SET qty = 1 WHERE id IN (SELECT id FROM users)",
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
            name: "delete from two tables",
            sql: "DELETE users, other FROM users JOIN other ON users.id = other.id",
        },
        SqlCase {
            name: "delete target spelled in another case than its table",
            sql: "DELETE USERS FROM users JOIN other ON users.id = other.id",
        },
        SqlCase {
            name: "delete through the table name of an aliased table",
            sql: "DELETE users FROM users AS u JOIN other ON u.id = other.id",
        },
        SqlCase {
            name: "delete using from two tables",
            sql: "DELETE FROM users, other USING users JOIN other",
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
        (
            "SET @Answer = 42, @s = 'x', @t = CONCAT('a', 'b'), time_zone = 'UTC'",
            "SELECT 0 WHERE __mysql_set_user_variables ('answer', 0, 42, 's', 1, 'x', 't', 0, CONCAT ('a', 'b'))",
        ),
        (
            "SET character_set_client = @saved, @a = 1",
            "SELECT 0 WHERE __mysql_keep_session_settings ('character_set_client', __mysql_user_variable_value ('saved')) OR __mysql_set_user_variables ('a', 0, 1)",
        ),
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
        "SELECT 18446744073709551615 + c",
        "SELECT 9223372036854775808 * c",
    ];

    for sql in cases {
        let statement = parse_and_translate(sql).expect("numeric literal must lower");
        assert_eq!(statement.to_string(), sql);
    }
}

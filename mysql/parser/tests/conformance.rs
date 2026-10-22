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
         id INT PRIMARY KEY NOT NULL CHECK (id BETWEEN - 2147483648 AND 2147483647), \
         name VARCHAR (100) CHECK (char_length (rtrim (name)) <= 100), \
         bio VARCHAR (1000) CHECK (char_length (rtrim (bio)) <= 1000))"
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
    assert_eq!(options, ast::TableOptions::empty());
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
            ast::NamedColumnConstraint {
                name: None,
                constraint: ast::ColumnConstraint::Check { .. },
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
         `primary` INT PRIMARY KEY NOT NULL CHECK (`primary` BETWEEN - 2147483648 AND 2147483647), \
         `display``name` VARCHAR (100) CHECK (char_length (rtrim (`display``name`)) <= 100))"
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
            "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL CHECK (id != 0) CHECK (id >= 0))",
        ),
        (
            "CREATE TABLE t (a TINYINT(1) DEFAULT '1', b DECIMAL(10,2) UNSIGNED, c FLOAT(7,3), d BIT(3))",
            "CREATE TABLE t (a TINYINT DEFAULT 1 CHECK (a BETWEEN - 128 AND 127), b DECIMAL UNSIGNED (10, 2) CHECK (b >= 0), c FLOAT (7, 3) CHECK (c = round (c, 3) AND abs (c) < 1e4), d BIT (3) CHECK (d BETWEEN 0 AND 7))",
        ),
        (
            "CREATE TABLE t (e ENUM('x','y') NOT NULL, s SET('r','w') DEFAULT 'w,r', j JSON, t TINYTEXT)",
            "CREATE TABLE t (e TEXT NOT NULL DEFAULT 'x' CHECK (e IN ('x', 'y')), s TEXT DEFAULT 'r,w' CHECK (s IN ('', 'r', 'w', 'r,w')), j JSON TEXT CHECK (json_valid (j)), t TINYTEXT CHECK (octet_length (t) <= 255))",
        ),
        (
            "CREATE TABLE t (d DATE DEFAULT '2020-1-2', ts DATETIME(3) DEFAULT '2020-01-02 03:04:05', c TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
            "CREATE TABLE t (d DATE DEFAULT '2020-01-02', ts DATETIME (3) DEFAULT '2020-01-02 03:04:05.000', c TIMESTAMP DEFAULT CURRENT_TIMESTAMP)",
        ),
        (
            "CREATE TABLE t (id INT, email VARCHAR(50), KEY k (id), UNIQUE KEY uq (email), CONSTRAINT ck CHECK (id > 0), CONSTRAINT fk FOREIGN KEY (id) REFERENCES p (id) ON DELETE CASCADE)",
            "CREATE TABLE t (id INT CHECK (id BETWEEN - 2147483648 AND 2147483647), email VARCHAR (50) CHECK (char_length (rtrim (email)) <= 50), CONSTRAINT uq UNIQUE (email), CONSTRAINT ck CHECK (id > 0), CONSTRAINT fk FOREIGN KEY (id) REFERENCES p (id) ON DELETE CASCADE)",
        ),
        (
            "CREATE TABLE t (a INT, b INT AS (a + 1) STORED, c INT GENERATED ALWAYS AS (a * 2))",
            "CREATE TABLE t (a INT CHECK (a BETWEEN - 2147483648 AND 2147483647), b INT CHECK (b BETWEEN - 2147483648 AND 2147483647) GENERATED ALWAYS AS (a + 1) STORED, c INT CHECK (c BETWEEN - 2147483648 AND 2147483647) GENERATED ALWAYS AS (a * 2) VIRTUAL)",
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
            "ALTER TABLE t ADD COLUMN c INT UNSIGNED DEFAULT 0 CHECK (c BETWEEN 0 AND 4294967295)",
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
            name: "temporary",
            sql: "CREATE TEMPORARY TABLE users(id INTEGER)",
        },
        SqlCase {
            name: "qualified name",
            sql: "CREATE TABLE app.users(id INTEGER)",
        },
        SqlCase {
            name: "like",
            sql: "CREATE TABLE users LIKE source_users",
        },
        SqlCase {
            name: "ctas",
            sql: "CREATE TABLE users AS SELECT 1",
        },
        SqlCase {
            name: "ctas without as",
            sql: "CREATE TABLE users SELECT 1",
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
            name: "binary pads values",
            sql: "CREATE TABLE users(code BINARY(4))",
        },
        SqlCase {
            name: "geometry",
            sql: "CREATE TABLE users(location POINT)",
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
            sql: "CREATE TABLE users(name VARCHAR(10) COLLATE utf8mb4_0900_as_cs)",
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
fn rejects_non_anonymous_and_expression_parameters() {
    let numbered = parse_one("SELECT ?1").expect("sqlparser accepts numbered placeholders");
    let error = translate(&numbered).expect_err("numbered placeholders must not lower");
    assert!(matches!(error, MysqlParserError::Unsupported(_)));

    for sql in ["SELECT :named", "SELECT @named", "SELECT $named"] {
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

    let error = parse_and_translate("SELECT ? + 1")
        .expect_err("expressions containing parameters must not lower");
    assert!(matches!(error, MysqlParserError::Unsupported(_)));
}

#[test]
fn lowering_rejects_each_unsupported_query_shape() {
    let cases = [
        SqlCase {
            name: "identifier expression",
            sql: "SELECT greeting",
        },
        SqlCase {
            name: "binary expression",
            sql: "SELECT 1 + 2",
        },
        SqlCase {
            name: "unary expression",
            sql: "SELECT -1",
        },
        SqlCase {
            name: "function",
            sql: "SELECT VERSION()",
        },
        SqlCase {
            name: "cast",
            sql: "SELECT CAST(1 AS SIGNED)",
        },
        SqlCase {
            name: "case",
            sql: "SELECT CASE WHEN TRUE THEN 1 ELSE 0 END",
        },
        SqlCase {
            name: "wildcard",
            sql: "SELECT *",
        },
        SqlCase {
            name: "qualified wildcard",
            sql: "SELECT users.* FROM users",
        },
        SqlCase {
            name: "from",
            sql: "SELECT 1 FROM dual",
        },
        SqlCase {
            name: "where",
            sql: "SELECT 1 WHERE TRUE",
        },
        SqlCase {
            name: "distinct",
            sql: "SELECT DISTINCT 1",
        },
        SqlCase {
            name: "mysql select modifier",
            sql: "SELECT SQL_NO_CACHE 1",
        },
        SqlCase {
            name: "group by",
            sql: "SELECT 1 GROUP BY 1",
        },
        SqlCase {
            name: "having",
            sql: "SELECT 1 HAVING TRUE",
        },
        SqlCase {
            name: "order by",
            sql: "SELECT 1 ORDER BY 1",
        },
        SqlCase {
            name: "limit",
            sql: "SELECT 1 LIMIT 1",
        },
        SqlCase {
            name: "subquery",
            sql: "SELECT (SELECT 1)",
        },
        SqlCase {
            name: "union",
            sql: "SELECT 1 UNION SELECT 2",
        },
        SqlCase {
            name: "cte",
            sql: "WITH cte AS (SELECT 1) SELECT 1",
        },
        SqlCase {
            name: "values",
            sql: "VALUES (1)",
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
        "CREATE VIEW v AS SELECT 1",
        "ALTER TABLE users ADD INDEX name_key (name)",
        "ALTER TABLE users MODIFY COLUMN name VARCHAR(20)",
        "ALTER TABLE users ADD COLUMN a INT, ADD COLUMN b INT",
        "ALTER TABLE users ADD COLUMN a INT FIRST",
        "DROP TABLE a, b",
        "DROP TEMPORARY TABLE users",
        "CREATE INDEX name_key ON users (name(10))",
        "INSERT INTO users VALUES (1)",
        "INSERT IGNORE INTO users VALUES (1)",
        "REPLACE INTO users VALUES (1)",
        "UPDATE users SET id = 2",
        "DELETE FROM users",
        "SHOW TABLES",
        "DESCRIBE users",
        "EXPLAIN SELECT 1",
        "USE app",
        "SET @answer = 42",
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
fn lowering_rejects_numbers_without_exact_mysql_semantics() {
    let cases = [
        "SELECT 1.5",
        "SELECT 1e3",
        "SELECT 18446744073709551615",
        "SELECT 9223372036854775808",
    ];

    for sql in cases {
        let error = parse_and_translate(sql).expect_err("inexact numeric literal must be rejected");
        assert!(
            error
                .to_string()
                .contains("exact MySQL decimal or unsigned semantics"),
            "SQL: {sql}\nerror: {error}"
        );
    }
}

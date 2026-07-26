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
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name VARCHAR (100), bio VARCHAR (1000))"
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
    assert_eq!(tbl_name.db_name, None);
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
    assert_eq!(columns[0].col_type.as_ref().unwrap().name, "INTEGER");
    assert!(matches!(
        columns[0].constraints.as_slice(),
        [ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::PrimaryKey {
                order: None,
                conflict_clause: None,
                auto_increment: false,
            },
        }]
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
        "CREATE TABLE IF NOT EXISTS `select` (`primary` INTEGER PRIMARY KEY, `display``name` VARCHAR (100))"
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
fn rejects_create_table_shapes_outside_the_basic_subset() {
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
            name: "int alias",
            sql: "CREATE TABLE users(id INT)",
        },
        SqlCase {
            name: "bigint",
            sql: "CREATE TABLE users(id BIGINT)",
        },
        SqlCase {
            name: "text",
            sql: "CREATE TABLE users(name TEXT)",
        },
        SqlCase {
            name: "integer precision",
            sql: "CREATE TABLE users(id INTEGER(11))",
        },
        SqlCase {
            name: "unsigned",
            sql: "CREATE TABLE users(id INTEGER UNSIGNED)",
        },
        SqlCase {
            name: "zerofill",
            sql: "CREATE TABLE users(id INTEGER ZEROFILL)",
        },
        SqlCase {
            name: "auto increment",
            sql: "CREATE TABLE users(id INTEGER AUTO_INCREMENT)",
        },
        SqlCase {
            name: "varchar missing length",
            sql: "CREATE TABLE users(name VARCHAR)",
        },
        SqlCase {
            name: "varchar zero length",
            sql: "CREATE TABLE users(name VARCHAR(0))",
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
            name: "varchar unsafe length",
            sql: "CREATE TABLE users(name VARCHAR(16384))",
        },
        SqlCase {
            name: "null",
            sql: "CREATE TABLE users(id INTEGER NULL)",
        },
        SqlCase {
            name: "not null",
            sql: "CREATE TABLE users(id INTEGER NOT NULL)",
        },
        SqlCase {
            name: "default",
            sql: "CREATE TABLE users(id INTEGER DEFAULT 1)",
        },
        SqlCase {
            name: "unique",
            sql: "CREATE TABLE users(id INTEGER UNIQUE)",
        },
        SqlCase {
            name: "check",
            sql: "CREATE TABLE users(id INTEGER CHECK (id > 0))",
        },
        SqlCase {
            name: "references",
            sql: "CREATE TABLE users(id INTEGER REFERENCES parents(id))",
        },
        SqlCase {
            name: "collate",
            sql: "CREATE TABLE users(name VARCHAR(100) COLLATE utf8mb4_bin)",
        },
        SqlCase {
            name: "generated",
            sql: "CREATE TABLE users(id INTEGER, next_id INTEGER AS (id + 1))",
        },
        SqlCase {
            name: "named constraint",
            sql: "CREATE TABLE users(id INTEGER CONSTRAINT users_pk PRIMARY KEY)",
        },
        SqlCase {
            name: "primary key on varchar",
            sql: "CREATE TABLE users(name VARCHAR(100) PRIMARY KEY)",
        },
        SqlCase {
            name: "two primary keys",
            sql: "CREATE TABLE users(id INTEGER PRIMARY KEY, other INTEGER PRIMARY KEY)",
        },
        SqlCase {
            name: "table primary key",
            sql: "CREATE TABLE users(id INTEGER, PRIMARY KEY (id))",
        },
        SqlCase {
            name: "table index",
            sql: "CREATE TABLE users(id INTEGER, INDEX users_id (id))",
        },
        SqlCase {
            name: "table key",
            sql: "CREATE TABLE users(id INTEGER, KEY users_id (id))",
        },
        SqlCase {
            name: "engine",
            sql: "CREATE TABLE users(id INTEGER) ENGINE=InnoDB",
        },
        SqlCase {
            name: "charset",
            sql: "CREATE TABLE users(id INTEGER) DEFAULT CHARSET=utf8mb4",
        },
        SqlCase {
            name: "table collation",
            sql: "CREATE TABLE users(id INTEGER) COLLATE=utf8mb4_0900_ai_ci",
        },
        SqlCase {
            name: "partitioning",
            sql: "CREATE TABLE users(id INTEGER) PARTITION BY HASH(id) PARTITIONS 2",
        },
        SqlCase {
            name: "table comment",
            sql: "CREATE TABLE users(id INTEGER) COMMENT='users'",
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
        "CREATE TABLE users(id INT)",
        "ALTER TABLE users ADD COLUMN name TEXT",
        "DROP TABLE users",
        "TRUNCATE TABLE users",
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

use turso_mysql_parser::{parse, parse_and_translate, parse_one, MysqlParserError};

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

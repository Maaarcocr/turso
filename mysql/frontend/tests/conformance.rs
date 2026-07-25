use turso_core::{Numeric, Value};
use turso_mysql::open_memory;

#[derive(Debug, Clone, Copy)]
enum ExpectedValue {
    Null,
    Integer(i64),
    Text(&'static str),
}

struct ExecutionCase {
    name: &'static str,
    sql: &'static str,
    expected: &'static [ExpectedValue],
}

fn assert_value(case: &ExecutionCase, column: usize, actual: &Value, expected: ExpectedValue) {
    match (actual, expected) {
        (Value::Null, ExpectedValue::Null) => {}
        (Value::Numeric(Numeric::Integer(actual)), ExpectedValue::Integer(expected)) => {
            assert_eq!(*actual, expected, "case={} column={column}", case.name);
        }
        (Value::Text(actual), ExpectedValue::Text(expected)) => {
            assert_eq!(
                actual.as_str(),
                expected,
                "case={} column={column}",
                case.name
            );
        }
        _ => panic!(
            "case={} column={column}: expected {expected:?}, got {actual:?}",
            case.name
        ),
    }
}

#[test]
fn executes_owned_literal_conformance_cases() -> turso_core::Result<()> {
    let cases = [
        ExecutionCase {
            name: "zero",
            sql: "SELECT 0",
            expected: &[ExpectedValue::Integer(0)],
        },
        ExecutionCase {
            name: "positive integer",
            sql: "SELECT 42",
            expected: &[ExpectedValue::Integer(42)],
        },
        ExecutionCase {
            name: "maximum signed integer",
            sql: "SELECT 9223372036854775807",
            expected: &[ExpectedValue::Integer(i64::MAX)],
        },
        ExecutionCase {
            name: "leading zeroes",
            sql: "SELECT 00042",
            expected: &[ExpectedValue::Integer(42)],
        },
        ExecutionCase {
            name: "true",
            sql: "SELECT TRUE",
            expected: &[ExpectedValue::Integer(1)],
        },
        ExecutionCase {
            name: "false",
            sql: "SELECT FALSE",
            expected: &[ExpectedValue::Integer(0)],
        },
        ExecutionCase {
            name: "lowercase boolean",
            sql: "SELECT true, false",
            expected: &[ExpectedValue::Integer(1), ExpectedValue::Integer(0)],
        },
        ExecutionCase {
            name: "null",
            sql: "SELECT NULL",
            expected: &[ExpectedValue::Null],
        },
        ExecutionCase {
            name: "empty string",
            sql: "SELECT ''",
            expected: &[ExpectedValue::Text("")],
        },
        ExecutionCase {
            name: "ascii string",
            sql: "SELECT 'hello world'",
            expected: &[ExpectedValue::Text("hello world")],
        },
        ExecutionCase {
            name: "doubled quote escape",
            sql: "SELECT 'I''m fine'",
            expected: &[ExpectedValue::Text("I'm fine")],
        },
        ExecutionCase {
            name: "backslash quote escape",
            sql: r"SELECT 'I\'m fine'",
            expected: &[ExpectedValue::Text("I'm fine")],
        },
        ExecutionCase {
            name: "double quoted string",
            sql: r#"SELECT "double quoted""#,
            expected: &[ExpectedValue::Text("double quoted")],
        },
        ExecutionCase {
            name: "utf8mb4 text",
            sql: "SELECT 'Grüße 世界 🦀'",
            expected: &[ExpectedValue::Text("Grüße 世界 🦀")],
        },
        ExecutionCase {
            name: "mixed projection",
            sql: "SELECT 1 AS one, 'two' AS two, NULL AS three, TRUE AS four",
            expected: &[
                ExpectedValue::Integer(1),
                ExpectedValue::Text("two"),
                ExpectedValue::Null,
                ExpectedValue::Integer(1),
            ],
        },
        ExecutionCase {
            name: "backtick alias",
            sql: "SELECT 'value' AS `select`",
            expected: &[ExpectedValue::Text("value")],
        },
        ExecutionCase {
            name: "trailing semicolon",
            sql: "SELECT 7;",
            expected: &[ExpectedValue::Integer(7)],
        },
        ExecutionCase {
            name: "comments",
            sql: "/* mysql */ SELECT 8 /* tail */;",
            expected: &[ExpectedValue::Integer(8)],
        },
    ];

    let connection = open_memory()?;
    for case in &cases {
        let mut statement = connection.prepare(case.sql).unwrap_or_else(|error| {
            panic!(
                "case={} failed to prepare: {error}\nSQL: {}",
                case.name, case.sql
            )
        });
        let rows = statement.run_collect_rows().unwrap_or_else(|error| {
            panic!(
                "case={} failed to execute: {error}\nSQL: {}",
                case.name, case.sql
            )
        });
        assert_eq!(rows.len(), 1, "case={}", case.name);
        assert_eq!(rows[0].len(), case.expected.len(), "case={}", case.name);
        for (column, (actual, expected)) in rows[0].iter().zip(case.expected.iter()).enumerate() {
            assert_value(case, column, actual, *expected);
        }
    }
    connection.close()?;
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
fn rejects_unsupported_features_instead_of_discarding_them() -> turso_core::Result<()> {
    let cases = [
        "SELECT greeting",
        "SELECT 1 + 2",
        "SELECT -1",
        "SELECT VERSION()",
        "SELECT *",
        "SELECT 1 FROM dual",
        "SELECT 1 WHERE TRUE",
        "SELECT DISTINCT 1",
        "SELECT SQL_NO_CACHE 1",
        "SELECT 1 GROUP BY 1",
        "SELECT 1 HAVING TRUE",
        "SELECT 1 ORDER BY 1",
        "SELECT 1 LIMIT 1",
        "SELECT (SELECT 1)",
        "SELECT 1 UNION SELECT 2",
        "WITH cte AS (SELECT 1) SELECT 1",
        "VALUES (1)",
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

    let connection = open_memory()?;
    for sql in cases {
        let error = connection
            .prepare(sql)
            .expect_err("unsupported SQL must return an error");
        assert!(
            error.to_string().contains("unsupported MySQL syntax"),
            "SQL: {sql}\nerror: {error}"
        );
    }
    connection.close()?;
    Ok(())
}

#[test]
fn known_parser_gaps_are_explicit() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let error = connection
        .prepare("SELECT 1 FOR UPDATE")
        .expect_err("known parser gap must return an error");
    assert!(error.to_string().contains("sql parser error"));
    connection.close()?;
    Ok(())
}

#[test]
fn rejects_numeric_values_that_would_lose_mysql_precision() -> turso_core::Result<()> {
    let cases = [
        "SELECT 1.5",
        "SELECT 1e3",
        "SELECT 18446744073709551615",
        "SELECT 9223372036854775808",
    ];
    let connection = open_memory()?;
    for sql in cases {
        let error = connection
            .prepare(sql)
            .expect_err("inexact numeric literal must fail");
        assert!(
            error
                .to_string()
                .contains("exact MySQL decimal or unsigned semantics"),
            "SQL: {sql}\nerror: {error}"
        );
    }
    connection.close()?;
    Ok(())
}

#[test]
fn rejects_multiple_statements_until_consumed_offsets_are_supported() -> turso_core::Result<()> {
    let connection = open_memory()?;
    let error = connection
        .prepare("SELECT 1; SELECT 2")
        .expect_err("multiple statements must not be partially executed");
    assert!(error
        .to_string()
        .contains("expected one MySQL statement, found 2"));
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

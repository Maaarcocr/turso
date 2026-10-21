use super::{BackendError, DatabaseFileHandle, DatabaseInstance, QueryResult, SqlBackend};
use crate::parser::ast::{Backend, Capability, DatabaseConfig, DatabaseLocation};
use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::Arc;
use tempfile::NamedTempFile;
use turso_core::{Connection, Numeric, Value};

/// Backend that runs tests in process on Turso with the MySQL frontend
/// dialect. Statements are split the way the `mysql` client splits them, and
/// values are rendered as MySQL's text protocol sends them, so expected
/// output recorded from MySQL compares directly.
pub struct TursomysqlBackend;

#[async_trait]
impl SqlBackend for TursomysqlBackend {
    fn name(&self) -> &str {
        "tursomysql"
    }

    fn backend_type(&self) -> Backend {
        Backend::Tursomysql
    }

    fn capabilities(&self) -> HashSet<Capability> {
        // The MySQL frontend's feature surface is distinct from the sqlite
        // capabilities tests can require.
        HashSet::new()
    }

    async fn create_database(
        &self,
        config: &DatabaseConfig,
    ) -> Result<Box<dyn DatabaseInstance>, BackendError> {
        if !config.is_writable() {
            return Err(BackendError::CreateDatabase(format!(
                "the tursomysql backend only creates new :memory: and :temp: databases, not {}",
                config.location
            )));
        }
        let (conn, temp_file) = match config.location {
            DatabaseLocation::TempFile => {
                let temp = NamedTempFile::new()
                    .map_err(|e| BackendError::CreateDatabase(e.to_string()))?;
                let path = temp.path().to_str().ok_or_else(|| {
                    BackendError::CreateDatabase(format!(
                        "temporary database path is not UTF-8: {}",
                        temp.path().display()
                    ))
                })?;
                (turso_mysql::open_file(path), Some(temp))
            }
            _ => (turso_mysql::open_memory(), None),
        };
        let conn = conn.map_err(|e| BackendError::CreateDatabase(e.to_string()))?;
        Ok(Box::new(TursomysqlDatabaseInstance {
            conn,
            _temp_file: temp_file,
        }))
    }
}

/// A database instance with one in-process MySQL-dialect connection.
pub struct TursomysqlDatabaseInstance {
    conn: Arc<Connection>,
    /// Keep temp file alive - deleted when dropped
    _temp_file: Option<NamedTempFile>,
}

impl TursomysqlDatabaseInstance {
    /// Runs every statement in order and concatenates their rows. The first
    /// failing statement ends the block, as in the `mysql` client.
    fn run(&self, sql: &str) -> Result<Vec<Vec<String>>, String> {
        let mut rows = Vec::new();
        for range in turso_mysql_parser::split_statements(sql).map_err(|e| e.to_string())? {
            // The dialect parses one statement per call; keep consuming so
            // no text of the statement could be skipped.
            let mut remaining = &sql[range];
            while let Some((mut stmt, consumed)) = self
                .conn
                .consume_stmt(remaining)
                .map_err(|e| e.to_string())?
            {
                stmt.run_with_row_callback(|row| {
                    rows.push(row.get_values().map(format_value).collect());
                    Ok(())
                })
                .map_err(|e| e.to_string())?;
                remaining = &remaining[consumed..];
            }
        }
        Ok(rows)
    }
}

#[async_trait]
impl DatabaseInstance for TursomysqlDatabaseInstance {
    async fn execute(&mut self, sql: &str) -> Result<QueryResult, BackendError> {
        Ok(match self.run(sql) {
            Ok(rows) => QueryResult::success(rows),
            Err(message) => QueryResult::error(message),
        })
    }

    async fn close(self: Box<Self>) -> Result<DatabaseFileHandle, BackendError> {
        match self._temp_file {
            Some(tf) => Ok(DatabaseFileHandle::temp(tf)),
            None => Ok(DatabaseFileHandle::none()),
        }
    }
}

/// Renders a value as MySQL's text protocol sends it.
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Numeric(Numeric::Integer(i)) => i.to_string(),
        Value::Numeric(Numeric::Float(f)) => format_double((*f).into()),
        Value::Text(text) => text.as_str().to_string(),
        Value::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// Renders a DOUBLE as MySQL 8.4 does when the value has no fixed number of
/// decimals (for example `1e0/3`, but not `PI()` or a `DOUBLE(10,2)` column).
///
/// MySQL prints the shortest digit string that round-trips. With the decimal
/// exponent `e` of the first digit (`d.ddd × 10^e`), it uses scientific
/// notation when `e < -15`, or when `e >= 15` and the value is an integer;
/// otherwise it uses plain notation without a trailing `.0`. Scientific
/// notation has no `+` and no exponent padding. Recorded from MySQL 8.4.10:
/// `1e15` → `1e15`, `999999999999999e0` → `999999999999999`,
/// `1234567890123456.7e0` → `1234567890123456.8`, `1e-15` →
/// `0.000000000000001`, `1e-16` → `1e-16`, `-0e0` → `-0`. The rule also
/// matched MySQL on 325 random values from 1e-25 to 1e25.
fn format_double(value: f64) -> String {
    if !value.is_finite() {
        // MySQL raises an out-of-range error instead of producing these.
        return value.to_string();
    }
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0");
    }
    // Rust's `{:e}` gives the shortest round-trip digits, which MySQL prints.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` output has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let digits = mantissa.replace('.', "");
    let integer_digits = exponent + 1;
    let is_integer = digits.len() as i32 <= integer_digits;
    if exponent < -15 || (exponent >= 15 && is_integer) {
        format!("{sign}{mantissa}e{exponent}")
    } else if exponent < 0 {
        let zeros = "0".repeat((-integer_digits) as usize);
        format!("{sign}0.{zeros}{digits}")
    } else if is_integer {
        let zeros = "0".repeat(integer_digits as usize - digits.len());
        format!("{sign}{digits}{zeros}")
    } else {
        let (integer, fraction) = digits.split_at(integer_digits as usize);
        format!("{sign}{integer}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values and outputs recorded from MySQL 8.4.10 with
    /// `mysql --batch --raw -e 'SELECT <value>'`.
    #[test]
    fn formats_doubles_like_mysql() {
        let cases: &[(f64, &str)] = &[
            (1e0 / 3.0, "0.3333333333333333"),
            (0.1 + 0.2, "0.30000000000000004"),
            (2f64.sqrt(), "1.4142135623730951"),
            (std::f64::consts::E, "2.718281828459045"),
            (1.0000000000000002, "1.0000000000000002"),
            (2.5, "2.5"),
            (-0.5, "-0.5"),
            (100.0, "100"),
            (125.0, "125"),
            (1e14, "100000000000000"),
            (123456789012345.0, "123456789012345"),
            (999999999999999.0, "999999999999999"),
            (123456789012345.6, "123456789012345.6"),
            (999999999999999.9, "999999999999999.9"),
            (12345678901234.567, "12345678901234.566"),
            (1234567890123456.7, "1234567890123456.8"),
            (1e15, "1e15"),
            (-1e15, "-1e15"),
            (1.5e15, "1.5e15"),
            (1234567890123456.0, "1.234567890123456e15"),
            (9007199254740993.0, "9.007199254740992e15"),
            (1e16, "1e16"),
            (12345678901234567.5, "1.2345678901234568e16"),
            (99999999999999999.0, "1e17"),
            (123456789012345678.0, "1.2345678901234568e17"),
            (1e21, "1e21"),
            (123456e17, "1.23456e22"),
            (1e100, "1e100"),
            (1.5e300, "1.5e300"),
            (1.7976931348623157e308, "1.7976931348623157e308"),
            (0.1, "0.1"),
            (1e-4, "0.0001"),
            (1e-5, "0.00001"),
            (1e-7, "0.0000001"),
            (-1e-7, "-0.0000001"),
            (1e-10, "0.0000000001"),
            (1.234e-10, "0.0000000001234"),
            (1.2345678901234567e-10, "0.00000000012345678901234568"),
            (1.2345678901234567e-5, "0.000012345678901234568"),
            (-1.2345678901234567e-5, "-0.000012345678901234568"),
            (1.2345678901234567e-3, "0.0012345678901234567"),
            (1e-15, "0.000000000000001"),
            (-1e-15, "-0.000000000000001"),
            (1.5e-15, "0.0000000000000015"),
            (1.2345678901234567e-15, "0.0000000000000012345678901234568"),
            (9.99e-16, "9.99e-16"),
            (1e-16, "1e-16"),
            (-1e-16, "-1e-16"),
            (1.2345678901234567e-16, "1.2345678901234568e-16"),
            (1e-100, "1e-100"),
            (2.2250738585072014e-308, "2.2250738585072014e-308"),
            (5e-324, "5e-324"),
            (0.0, "0"),
            (-0.0, "-0"),
        ];
        for (value, expected) in cases {
            assert_eq!(format_double(*value), *expected, "{value:e}");
        }
    }

    #[test]
    fn formats_values_like_the_mysql_text_protocol() {
        assert_eq!(format_value(&Value::Null), "NULL");
        assert_eq!(format_value(&Value::from_i64(-42)), "-42");
        assert_eq!(format_value(&Value::from_f64(1e15)), "1e15");
        assert_eq!(format_value(&Value::build_text("Grüße")), "Grüße");
        assert_eq!(format_value(&Value::from_blob(b"abc".to_vec())), "abc");
        assert_eq!(
            format_value(&Value::from_blob(vec![0x61, 0xFF])),
            "a\u{FFFD}"
        );
    }

    async fn run(location: DatabaseLocation, sql: &str) -> QueryResult {
        let config = DatabaseConfig {
            location,
            readonly: false,
        };
        let mut db = TursomysqlBackend.create_database(&config).await.unwrap();
        let result = db.execute(sql).await.unwrap();
        db.close().await.unwrap();
        result
    }

    #[tokio::test]
    async fn runs_statements_in_order_and_concatenates_rows() {
        for location in [DatabaseLocation::Memory, DatabaseLocation::TempFile] {
            let result = run(
                location,
                "SELECT 'a;b', NULL; -- c;\nCREATE TABLE t(x INTEGER);\nSELECT 1e0/3 # d;\n;",
            )
            .await;
            assert_eq!(result.error, None);
            assert_eq!(
                result.rows,
                vec![vec!["a;b", "NULL"], vec!["0.3333333333333333"]]
            );
        }
    }

    #[tokio::test]
    async fn first_error_ends_the_block() {
        let result = run(
            DatabaseLocation::Memory,
            "CREATE TABLE t(x INTEGER); CREATE TABLE t(x INTEGER); SELECT 1",
        )
        .await;
        assert!(result.error.unwrap().contains("already exists"));
        assert!(result.rows.is_empty());

        let result = run(DatabaseLocation::Memory, "SELECT 1; SELECT 'open").await;
        assert!(result.is_error());
    }

    #[tokio::test]
    async fn setup_persists_on_the_instance() {
        let config = DatabaseConfig {
            location: DatabaseLocation::Memory,
            readonly: false,
        };
        let mut db = TursomysqlBackend.create_database(&config).await.unwrap();
        db.execute_setup("CREATE TABLE t(x INTEGER);")
            .await
            .unwrap();
        let duplicate = db.execute("CREATE TABLE t(x INTEGER)").await.unwrap();
        assert!(duplicate.is_error());
        assert!(db.execute_setup("SELECT 'open").await.is_err());
    }

    #[tokio::test]
    async fn rejects_databases_it_cannot_create() {
        let config = DatabaseConfig {
            location: DatabaseLocation::Default,
            readonly: true,
        };
        assert!(TursomysqlBackend.create_database(&config).await.is_err());
    }
}

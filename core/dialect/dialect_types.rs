#[derive(Debug)]
pub struct DialectType {
    pub name: &'static str,
    pub storage: StorageType,
    pub parameters: &'static str,
    pub behaviour: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageType {
    Integer,
    Real,
    Text,
    Blob,
    RangedInteger { min: i64, max: i64 },
    Uint64,
    DateDays,
    DatetimeMicros,
    TimeMicros,
    Enumeration,
    LabelSet,
    Numeric,
}

impl DialectType {
    pub(crate) fn create_type_sql(&self) -> String {
        let parameters: String = [self.storage.parameters(), self.parameters]
            .into_iter()
            .filter(|parameters| !parameters.is_empty())
            .map(|parameters| format!(", {parameters}"))
            .collect();
        let name = self.name.replace('"', "\"\"");
        format!(
            "CREATE TYPE \"{name}\"(value {}{parameters}) BASE {}{} {}",
            self.storage.input_class(),
            self.storage.stored_class(),
            self.storage.conversions(self.name),
            self.behaviour
        )
    }
}

impl StorageType {
    fn input_class(self) -> &'static str {
        match self {
            StorageType::Integer | StorageType::RangedInteger { .. } => "integer",
            StorageType::Real => "real",
            StorageType::Text
            | StorageType::DateDays
            | StorageType::DatetimeMicros
            | StorageType::TimeMicros
            | StorageType::Enumeration
            | StorageType::LabelSet => "text",
            StorageType::Blob => "blob",
            StorageType::Uint64 | StorageType::Numeric => "any",
        }
    }

    fn stored_class(self) -> &'static str {
        match self {
            StorageType::Integer
            | StorageType::RangedInteger { .. }
            | StorageType::DateDays
            | StorageType::DatetimeMicros
            | StorageType::TimeMicros
            | StorageType::Enumeration => "integer",
            StorageType::Real => "real",
            StorageType::Text => "text",
            StorageType::Blob | StorageType::Numeric => "blob",
            StorageType::Uint64 | StorageType::LabelSet => "any",
        }
    }

    pub(crate) fn takes_label_list(self) -> bool {
        matches!(self, StorageType::Enumeration | StorageType::LabelSet)
    }

    fn parameters(self) -> &'static str {
        match self {
            StorageType::DatetimeMicros | StorageType::TimeMicros => "fsp integer DEFAULT 0",
            StorageType::Enumeration | StorageType::LabelSet => "labels text",
            StorageType::Numeric => "precision integer DEFAULT NULL, scale integer DEFAULT NULL",
            _ => "",
        }
    }

    fn conversions(self, type_name: &str) -> String {
        match self {
            StorageType::Integer | StorageType::Real | StorageType::Text | StorageType::Blob => {
                String::new()
            }
            StorageType::RangedInteger { min, max } => {
                ranged_integer_conversions(min, max, type_name)
            }
            StorageType::Uint64 => {
                " ENCODE uint64_encode(value) DECODE uint64_decode(value) KEY uint64_encode(value)"
                    .to_string()
            }
            StorageType::DateDays => " ENCODE date_days_encode(value) \
                DECODE date_days_decode(value) KEY date_days_encode(value)"
                .to_string(),
            StorageType::DatetimeMicros => " ENCODE datetime_micros_encode(value, fsp) \
                DECODE datetime_micros_decode(value, fsp) KEY datetime_micros_encode(value, 6)"
                .to_string(),
            StorageType::TimeMicros => " ENCODE time_micros_encode(value, fsp) \
                DECODE time_micros_decode(value, fsp) KEY time_micros_encode(value, 6)"
                .to_string(),
            StorageType::Enumeration => " ENCODE enumeration_encode(value, labels) \
                DECODE enumeration_decode(value, labels) KEY enumeration_encode(value, labels)"
                .to_string(),
            StorageType::Numeric => " ENCODE numeric_encode(value, precision, scale) \
                DECODE numeric_decode(value) KEY numeric_key(value)"
                .to_string(),
            StorageType::LabelSet => " ENCODE label_set_encode(value, labels) \
                DECODE label_set_decode(value, labels) KEY label_set_encode(value, labels)"
                .to_string(),
        }
    }
}

fn ranged_integer_conversions(min: i64, max: i64, type_name: &str) -> String {
    let mut limits = Vec::with_capacity(2);
    if min > i64::MIN {
        limits.push(format!("value >= {min}"));
    }
    if max < i64::MAX {
        limits.push(format!("value <= {max}"));
    }
    if limits.is_empty() {
        return String::new();
    }
    format!(
        " ENCODE CASE WHEN {} THEN value \
         ELSE conversion_error('out of range', 'integer out of range for {}') END DECODE value",
        limits.join(" AND "),
        type_name.replace('\'', "''")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::test_dialect::{ints, marking, open_db, query};
    use crate::{ConnectOpts, MemoryIO, Value, IO};
    use std::sync::Arc;

    #[test]
    fn ranged_integers_check_only_the_limits_they_have() {
        let tinyint = DialectType {
            name: "tinyint",
            storage: StorageType::RangedInteger {
                min: -128,
                max: 127,
            },
            parameters: "width integer DEFAULT NULL",
            behaviour: "",
        };
        assert_eq!(
            tinyint.create_type_sql(),
            "CREATE TYPE \"tinyint\"(value integer, width integer DEFAULT NULL) BASE integer \
             ENCODE CASE WHEN value >= -128 AND value <= 127 THEN value \
             ELSE conversion_error('out of range', 'integer out of range for tinyint') END \
             DECODE value "
        );
        let unsigned_bigint = DialectType {
            name: "bigint unsigned",
            storage: StorageType::RangedInteger {
                min: 0,
                max: i64::MAX,
            },
            parameters: "",
            behaviour: "",
        };
        assert!(unsigned_bigint
            .create_type_sql()
            .contains("CASE WHEN value >= 0 THEN value"));
        let bigint = DialectType {
            name: "bigint",
            storage: StorageType::RangedInteger {
                min: i64::MIN,
                max: i64::MAX,
            },
            parameters: "",
            behaviour: "",
        };
        assert_eq!(
            bigint.create_type_sql(),
            "CREATE TYPE \"bigint\"(value integer) BASE integer "
        );
    }

    #[test]
    fn columns_refuse_values_outside_their_range_also_after_reopening() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(&io, "types-range.db", marking()).unwrap();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a TINYINT, b INT) STRICT")
                .unwrap();
            conn.execute("INSERT INTO t VALUES (1, 127, 2147483647)")
                .unwrap();
            conn.execute("INSERT INTO t VALUES (2, -128, -2147483648)")
                .unwrap();
            for row in ["(3, 128, 0)", "(4, -129, 0)", "(5, 0, 2147483648)"] {
                let error = conn
                    .execute(format!("INSERT INTO t VALUES {row}"))
                    .unwrap_err();
                assert!(error.to_string().contains("out of range"), "{row}: {error}");
            }
            conn.close().unwrap();
        }
        let db = open_db(&io, "types-range.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert!(conn.execute("INSERT INTO t VALUES (6, 200, 0)").is_err());
        assert!(conn.execute("UPDATE t SET a = a + 1 WHERE id = 1").is_err());
        assert_eq!(
            query(&conn, "SELECT id, a, b FROM t ORDER BY id"),
            ints(&[&[1, 127, 2147483647], &[2, -128, -2147483648]])
        );
    }

    #[test]
    fn the_dialect_type_wins_over_a_built_in_type_of_the_same_name() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "types-date.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, d DATE) STRICT")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'not a date')")
            .unwrap();
        assert_eq!(
            query(&conn, "SELECT d FROM t"),
            vec![vec![Value::build_text("not a date")]]
        );
    }

    #[test]
    fn every_declared_type_goes_through_the_name_table() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "types-unknown.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        for column_type in ["REAL", "ANY", "BLOB", "INET", "NUMERIC(10,2)"] {
            let error = conn
                .execute(format!("CREATE TABLE t(v {column_type}) STRICT"))
                .unwrap_err();
            assert!(
                error.to_string().contains("unknown datatype"),
                "{column_type}: {error}"
            );
        }
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY) STRICT")
            .unwrap();
        let error = conn.execute("ALTER TABLE t ADD COLUMN r REAL").unwrap_err();
        assert!(error.to_string().contains("unknown datatype"), "{error}");
        let error = conn
            .execute("CREATE TABLE v(b VARBINARY) STRICT")
            .unwrap_err();
        assert!(error.to_string().contains("parameter"), "{error}");
    }

    #[test]
    fn unsigned_64_bit_values_are_stored_sorted_deduplicated_and_found_through_an_index() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let big = |text: &str| Value::build_text(text.to_string());
        {
            let db = open_db(&io, "types-uint64.db", marking()).unwrap();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, u UBIGINT) STRICT")
                .unwrap();
            conn.execute("CREATE INDEX t_u ON t(u)").unwrap();
            for row in [
                "(1, '18446744073709551615')",
                "(2, '9223372036854775808')",
                "(3, 5)",
                "(4, '9223372036854775807')",
                "(5, '18446744073709551615')",
            ] {
                conn.execute(format!("INSERT INTO t VALUES {row}")).unwrap();
            }
            for row in ["(6, -1)", "(7, '18446744073709551616')"] {
                assert!(
                    conn.execute(format!("INSERT INTO t VALUES {row}")).is_err(),
                    "{row}"
                );
            }
            conn.close().unwrap();
        }
        let db = open_db(&io, "types-uint64.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(
            query(&conn, "SELECT u FROM t NOT INDEXED ORDER BY u, id"),
            vec![
                vec![Value::from_i64(5)],
                vec![Value::from_i64(i64::MAX)],
                vec![big("9223372036854775808")],
                vec![big("18446744073709551615")],
                vec![big("18446744073709551615")],
            ]
        );
        assert_eq!(
            query(&conn, "SELECT count(DISTINCT u) FROM t"),
            ints(&[&[4]])
        );
        assert_eq!(
            query(&conn, "SELECT id FROM t WHERE u = '9223372036854775808'"),
            ints(&[&[2]])
        );
        assert_eq!(
            query(
                &conn,
                "SELECT id FROM t INDEXED BY t_u WHERE u = '18446744073709551615' ORDER BY id"
            ),
            ints(&[&[1], &[5]])
        );
        assert_eq!(
            query(&conn, "SELECT min(u), max(u) FROM t"),
            vec![vec![Value::from_i64(5), big("18446744073709551615")]]
        );
        assert_eq!(
            query(&conn, "SELECT u + 1 FROM t WHERE id = 4"),
            vec![vec![big("9223372036854775808")]]
        );
        assert!(conn
            .prepare("SELECT u + 1 FROM t WHERE id = 1")
            .unwrap()
            .run_collect_rows()
            .is_err());
    }

    #[test]
    fn dates_and_times_are_stored_as_integers_and_sorted_as_times() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let text = |value: &str| Value::build_text(value.to_string());
        {
            let db = open_db(&io, "types-time.db", marking()).unwrap();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            conn.execute(
                "CREATE TABLE t(id INTEGER PRIMARY KEY, d DATE_DAYS, dt DATETIME_MICROS(2), \
                 tm TIME_MICROS) STRICT",
            )
            .unwrap();
            for row in [
                "(1, '2024-05-01', '2024-05-01 10:30:00.25', '100:00:00')",
                "(2, '1969-12-31', '1969-12-31 23:59:59.5', '20:00:00')",
                "(3, '0001-01-01', '2024-05-01 10:30:00.125', '-01:00:00')",
                "(4, '9999-12-31', '2024-05-01 10:30:00', '-02:00:00')",
            ] {
                conn.execute(format!("INSERT INTO t VALUES {row}")).unwrap();
            }
            for row in [
                "(5, '2024-00-10', NULL, NULL)",
                "(6, NULL, '2024-05-01 24:00:00', NULL)",
                "(7, NULL, NULL, '10:61:00')",
            ] {
                assert!(
                    conn.execute(format!("INSERT INTO t VALUES {row}")).is_err(),
                    "{row}"
                );
            }
            conn.close().unwrap();
        }
        let db = open_db(&io, "types-time.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(
            query(&conn, "SELECT d, dt FROM t ORDER BY id"),
            vec![
                vec![text("2024-05-01"), text("2024-05-01 10:30:00.25")],
                vec![text("1969-12-31"), text("1969-12-31 23:59:59.50")],
                vec![text("0001-01-01"), text("2024-05-01 10:30:00.13")],
                vec![text("9999-12-31"), text("2024-05-01 10:30:00.00")],
            ]
        );
        assert_eq!(
            query(&conn, "SELECT tm FROM t ORDER BY tm"),
            vec![
                vec![text("-02:00:00")],
                vec![text("-01:00:00")],
                vec![text("20:00:00")],
                vec![text("100:00:00")],
            ]
        );
        assert_eq!(
            query(&conn, "SELECT min(tm), max(tm), min(d), max(d) FROM t"),
            vec![vec![
                text("-02:00:00"),
                text("100:00:00"),
                text("0001-01-01"),
                text("9999-12-31"),
            ]]
        );
    }

    #[cfg(all(feature = "fs", not(target_family = "wasm")))]
    #[test]
    fn labels_are_stored_as_positions_and_bit_masks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("labels.db");
        let io: Arc<dyn IO> = Arc::new(crate::io::PlatformIO::new().unwrap());
        let open = || {
            crate::Database::open_file_with_flags(
                io.clone(),
                path.to_str().unwrap(),
                crate::OpenFlags::Create,
                crate::DatabaseOpts::new().with_vacuum(true),
                None,
                marking(),
            )
            .unwrap()
        };
        let text = |value: &str| Value::build_text(value.to_string());
        {
            let db = open();
            let conn = db.connect(ConnectOpts::new()).unwrap();
            conn.execute(
                "/* turso_dialect:marking rowid_alias */ CREATE TABLE t(id INTEGER PRIMARY KEY, \
                 size ENUMERATION('small','large'), tags LABEL_SET('a','b','c')) STRICT",
            )
            .unwrap();
            for row in [
                "(1, 'large', 'c,a')",
                "(2, 'small', '')",
                "(3, 'large', 'b')",
            ] {
                conn.execute(format!("INSERT INTO t VALUES {row}")).unwrap();
            }
            for row in ["(4, 'medium', '')", "(5, 'small', 'a,d')"] {
                assert!(
                    conn.execute(format!("INSERT INTO t VALUES {row}")).is_err(),
                    "{row}"
                );
            }
            let error = conn
                .execute("CREATE TABLE u(size ENUMERATION) STRICT")
                .unwrap_err();
            assert!(error.to_string().contains("at least one label"), "{error}");
            conn.execute("VACUUM").unwrap();
            conn.close().unwrap();
        }
        let db = open();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(
            query(&conn, "SELECT id, size, tags FROM t ORDER BY size, id"),
            vec![
                vec![Value::from_i64(2), text("small"), text("")],
                vec![Value::from_i64(1), text("large"), text("a,c")],
                vec![Value::from_i64(3), text("large"), text("b")],
            ]
        );
        assert_eq!(
            query(&conn, "SELECT min(size), max(size) FROM t"),
            vec![vec![text("large"), text("small")]]
        );
        assert_eq!(
            query(&conn, "SELECT id FROM t WHERE size = 'small'"),
            ints(&[&[2]])
        );
    }

    fn column_parameters(conn: &Arc<crate::Connection>, sql: &str) -> Vec<String> {
        let stmt = conn.prepare(sql).unwrap();
        stmt.get_column_type_info(0).unwrap().unwrap().parameters
    }

    #[test]
    fn integer_arithmetic_reports_out_of_range_through_class_signatures() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "class-integers.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(
            query(&conn, "SELECT 2 + 3, 7 - 9, 6 * 7, -(4)"),
            ints(&[&[5, -2, 42, -4]])
        );
        for sql in [
            "SELECT 9223372036854775807 + 1",
            "SELECT -9223372036854775807 - 2",
            "SELECT 4611686018427387904 * 2",
        ] {
            let error = conn.prepare(sql).unwrap().run_collect_rows().unwrap_err();
            assert!(error.to_string().contains("out of range"), "{sql}: {error}");
        }
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, a INTEGER) STRICT")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, -9223372036854775808)")
            .unwrap();
        assert!(conn
            .prepare("SELECT -a FROM t")
            .unwrap()
            .run_collect_rows()
            .is_err());

        let sqlite_db = open_db(
            &io,
            "class-integers-sqlite.db",
            Arc::new(crate::dialect::SqliteDialect),
        )
        .unwrap();
        let sqlite_conn = sqlite_db.connect(ConnectOpts::new()).unwrap();
        assert_eq!(
            query(&sqlite_conn, "SELECT 9223372036854775807 + 1"),
            vec![vec![Value::from_f64(9223372036854775808.0)]]
        );
    }

    #[test]
    fn integer_division_has_the_scale_of_the_result_rule() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "class-division.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        let text = |value: &str| Value::build_text(value.to_string());
        assert_eq!(
            query(&conn, "SELECT 7 / 2"),
            vec![vec![text("3.5000000000000000")]]
        );
        assert_eq!(column_parameters(&conn, "SELECT 7 / 2"), vec!["23", "4"]);
        let rows = query(&conn, "SELECT 64 / 77 * 77");
        let Value::Text(value) = &rows[0][0] else {
            panic!("expected a decimal text, got {:?}", rows[0][0]);
        };
        assert!(value.as_str().starts_with("63.9999999999"), "{value}");
        assert_eq!(
            column_parameters(&conn, "SELECT 64 / 77 * 77"),
            vec!["42", "4"]
        );
        assert_eq!(column_parameters(&conn, "SELECT 1.50"), vec!["3", "2"]);
    }

    #[test]
    fn comparisons_convert_by_the_class_rules() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "class-comparisons.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, tiny TINYINT, i INT) STRICT")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 1, 1), (2, 2, 2)")
            .unwrap();
        assert_eq!(
            query(&conn, "SELECT id FROM t WHERE tiny = 1000"),
            ints(&[])
        );
        assert_eq!(query(&conn, "SELECT id FROM t WHERE i = 1.5"), ints(&[]));
        assert_eq!(
            query(&conn, "SELECT id FROM t WHERE i = 2.0"),
            ints(&[&[2]])
        );
        assert_eq!(
            query(&conn, "SELECT 'abc' = 3, 'abc' = 0"),
            ints(&[&[1, 0]])
        );
    }

    #[test]
    fn dialect_functions_receive_the_static_types_of_their_arguments() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "argument-types.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, tiny TINYINT(1), d DECIMAL(10,2)) STRICT",
        )
        .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 1, 2.5)").unwrap();
        assert_eq!(
            query(
                &conn,
                "SELECT test_argument_types(1.5, 3, 'x', tiny, d, NULL, 7 / 2) FROM t"
            ),
            vec![vec![Value::build_text(
                "decimal(2,1) integer text tinyint(1) decimal(10,2) unknown decimal(23,4)"
                    .to_string()
            )]]
        );
    }

    fn conversion_error(error: crate::LimboError) -> crate::TypeConversionError {
        match error {
            crate::LimboError::TypeConversion(conversion) => *conversion,
            other => panic!("expected a conversion error, got {other:?}"),
        }
    }

    #[test]
    fn conversion_errors_name_their_kind_column_and_row() {
        use crate::ConversionErrorKind;
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "conversion-errors.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, tiny TINYINT, u UBIGINT) STRICT")
            .unwrap();

        let error = conversion_error(
            conn.execute("INSERT INTO t VALUES (1, 1, 1), (2, 200, 1), (3, 1, 1)")
                .unwrap_err(),
        );
        assert_eq!(error.kind, ConversionErrorKind::OutOfRange);
        assert_eq!(error.column.as_deref(), Some("tiny"));
        assert_eq!(error.row, Some(2));
        assert_eq!(error.message, "integer out of range for tinyint");
        assert_eq!(query(&conn, "SELECT count(*) FROM t"), ints(&[&[0]]));

        let error = conversion_error(
            conn.execute("INSERT INTO t VALUES (1, 1, 'abc')")
                .unwrap_err(),
        );
        assert_eq!(error.kind, ConversionErrorKind::InvalidValue);
        assert_eq!((error.column.as_deref(), error.row), (Some("u"), Some(1)));

        conn.execute("INSERT INTO t VALUES (1, 1, 1), (2, 100, 1)")
            .unwrap();
        let error = conversion_error(conn.execute("UPDATE t SET tiny = tiny + 28").unwrap_err());
        assert_eq!(error.kind, ConversionErrorKind::OutOfRange);
        assert_eq!(
            (error.column.as_deref(), error.row),
            (Some("tiny"), Some(2))
        );
        assert_eq!(
            query(&conn, "SELECT tiny FROM t ORDER BY id"),
            ints(&[&[1], &[100]])
        );

        let error = conversion_error(
            conn.prepare("SELECT conversion_error('too long', 'value too long')")
                .unwrap()
                .run_collect_rows()
                .unwrap_err(),
        );
        assert_eq!(error.kind, ConversionErrorKind::TooLong);
        assert_eq!((error.column, error.row), (None, None));
        assert!(conn
            .prepare("SELECT conversion_error('broken', 'x')")
            .unwrap()
            .run_collect_rows()
            .is_err());
    }

    #[test]
    fn assignment_converts_values_before_triggers_and_storage() {
        use crate::ConversionErrorKind;
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "assignment.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, i INT) STRICT")
            .unwrap();
        conn.execute("CREATE TABLE seen(i INTEGER, kind TEXT) STRICT")
            .unwrap();
        conn.execute(
            "CREATE TRIGGER log BEFORE INSERT ON t BEGIN \
             INSERT INTO seen VALUES (NEW.i, typeof(NEW.i)); END",
        )
        .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 1.5), (2, '7')")
            .unwrap();
        conn.execute("UPDATE t SET i = 2.5 WHERE id = 2").unwrap();
        assert_eq!(
            query(&conn, "SELECT id, i FROM t ORDER BY id"),
            ints(&[&[1, 2], &[2, 3]])
        );
        let text = |value: &str| Value::build_text(value.to_string());
        assert_eq!(
            query(&conn, "SELECT i, kind FROM seen ORDER BY i"),
            vec![
                vec![Value::from_i64(2), text("integer")],
                vec![Value::from_i64(7), text("integer")],
            ]
        );
        let error = conversion_error(
            conn.execute("INSERT INTO t VALUES (3, 1), (4, '12abc')")
                .unwrap_err(),
        );
        assert_eq!(error.kind, ConversionErrorKind::InvalidValue);
        assert_eq!((error.column.as_deref(), error.row), (Some("i"), Some(2)));
        assert_eq!(query(&conn, "SELECT CAST('12abc' AS INT)"), ints(&[&[12]]));
    }

    #[test]
    fn assignment_and_implicit_conversions_receive_the_type_parameters() {
        use crate::ConversionErrorKind;
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "assignment-parameters.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, d DECIMAL(4,2)) STRICT")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, 2.349), (2, 12.5)")
            .unwrap();
        let text = |value: &str| Value::build_text(value.to_string());
        assert_eq!(
            query(&conn, "SELECT d FROM t ORDER BY id"),
            vec![vec![text("2.35")], vec![text("12.50")]]
        );
        let error = conversion_error(conn.execute("INSERT INTO t VALUES (3, 123.4)").unwrap_err());
        assert_eq!(error.kind, ConversionErrorKind::OutOfRange);
        assert!(error.message.contains("decimal(4,2)"), "{}", error.message);
        assert_eq!(
            query(&conn, "SELECT id FROM t WHERE d = '2.349'"),
            ints(&[&[1]])
        );
    }

    #[test]
    fn virtual_generated_columns_and_defaults_are_assigned_to_the_column_type() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let file = io
            .open_file("assignment-generated.db", crate::OpenFlags::Create, true)
            .unwrap();
        let db = crate::Database::open(
            io.clone(),
            "assignment-generated.db",
            crate::OpenOptions::new(marking())
                .storage(Arc::new(crate::storage::database::DatabaseFile::new(file)))
                .db_opts(crate::DatabaseOpts::new().with_generated_columns(true)),
        )
        .unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute(
            "CREATE TABLE t(id INTEGER PRIMARY KEY, d DOUBLE, g INT AS (d) VIRTUAL, \
             i INT DEFAULT 4.5) STRICT",
        )
        .unwrap();
        conn.execute("INSERT INTO t(id, d) VALUES (1, 2.5)")
            .unwrap();
        assert_eq!(query(&conn, "SELECT g, i FROM t"), ints(&[&[3, 5]]));
        conn.execute("UPDATE t SET d = 6.5").unwrap();
        assert_eq!(query(&conn, "SELECT g, i FROM t"), ints(&[&[7, 5]]));
        conn.execute("ALTER TABLE t ADD COLUMN j INT DEFAULT 8.5")
            .unwrap();
        assert_eq!(query(&conn, "SELECT j FROM t"), ints(&[&[9]]));
        let error = conn
            .execute("ALTER TABLE t ADD COLUMN k INT DEFAULT 'abc'")
            .unwrap_err();
        assert!(error.to_string().contains("invalid value"), "{error}");
        assert_eq!(query(&conn, "SELECT j FROM t"), ints(&[&[9]]));
    }

    #[test]
    fn affinity_comes_from_the_storage_type() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "types-affinity.db", marking()).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, dt DATETIME, x DOUBLE) STRICT")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, '2024', 1)").unwrap();
        assert_eq!(
            query(&conn, "SELECT typeof(dt), dt, typeof(x) FROM t"),
            vec![vec![
                Value::build_text("text"),
                Value::build_text("2024"),
                Value::build_text("real"),
            ]]
        );
    }
}

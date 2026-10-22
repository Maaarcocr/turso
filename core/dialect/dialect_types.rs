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
}

impl DialectType {
    pub(crate) fn create_type_sql(&self) -> String {
        let parameters = if self.parameters.is_empty() {
            String::new()
        } else {
            format!(", {}", self.parameters)
        };
        let class = self.storage.class();
        let name = self.name.replace('"', "\"\"");
        let conversions = self.storage.conversions(self.name);
        format!(
            "CREATE TYPE \"{name}\"(value {class}{parameters}) BASE {class}{conversions} {}",
            self.behaviour
        )
    }
}

impl StorageType {
    fn class(self) -> &'static str {
        match self {
            StorageType::Integer | StorageType::RangedInteger { .. } => "integer",
            StorageType::Real => "real",
            StorageType::Text => "text",
            StorageType::Blob => "blob",
            StorageType::Uint64 => "any",
        }
    }

    fn conversions(self, type_name: &str) -> String {
        if self == StorageType::Uint64 {
            return " ENCODE uint64_encode(value) DECODE uint64_decode(value) KEY uint64_encode(value)"
                .to_string();
        }
        let StorageType::RangedInteger { min, max } = self else {
            return String::new();
        };
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
            " ENCODE CASE WHEN {} THEN value ELSE RAISE(ABORT, 'integer out of range for {}') END DECODE value",
            limits.join(" AND "),
            type_name.replace('\'', "''")
        )
    }
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
             ELSE RAISE(ABORT, 'integer out of range for tinyint') END DECODE value "
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

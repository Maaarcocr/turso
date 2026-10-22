use super::{sqlite, ClassConversion, Dialect, DialectType, SqliteDialect, StorageType, TypeRules};
use crate::schema::BTreeTable;
use crate::storage::database::DatabaseFile;
use crate::{Connection, Database, OpenFlags, Result, Value, IO};
use std::sync::Arc;
use turso_parser::ast;

pub(crate) struct MarkingDialect {
    pub(crate) name: &'static str,
}

pub(crate) static TEST_TYPE_RULES: TypeRules = TypeRules {
    builtin_behaviour_overrides: &[],
    cast_uses_parameter_defaults: true,
    class_operators: "OPERATOR '+' (integer, integer) RETURNS integer int64_add \
        OPERATOR '-' (integer, integer) RETURNS integer int64_sub \
        OPERATOR '*' (integer, integer) RETURNS integer int64_mul \
        OPERATOR '-' (integer) RETURNS integer int64_neg \
        OPERATOR '/' (integer, integer) RETURNS decimal numeric_div \
        OPERATOR '=' (integer, integer) \
        OPERATOR '<' (integer, integer) \
        OPERATOR '=' (real, real) \
        OPERATOR '<' (real, real) \
        OPERATOR '=' (text, text) \
        OPERATOR '<' (text, text)",
    class_conversions: &[
        ClassConversion {
            from: "text",
            to: "real",
            function: Some("test_text_length"),
        },
        ClassConversion {
            from: "integer",
            to: "real",
            function: None,
        },
    ],
    decimal_literal_type: Some("decimal"),
    string_literals_are_text: true,
    result_parameters: Some(test_result_parameters),
    dialect_types: &[
        DialectType {
            name: "decimal",
            storage: StorageType::Numeric,
            parameters: "",
            behaviour: "CAST FROM integer, int, tinyint AS IMPLICIT \
                OPERATOR '+' (decimal, decimal) RETURNS decimal numeric_add \
                OPERATOR '-' (decimal, decimal) RETURNS decimal numeric_sub \
                OPERATOR '*' (decimal, decimal) RETURNS decimal numeric_mul \
                OPERATOR '/' (decimal, decimal) RETURNS decimal numeric_div \
                OPERATOR '-' (decimal) RETURNS decimal numeric_neg \
                OPERATOR '<' (decimal, decimal) numeric_lt \
                OPERATOR '=' (decimal, decimal) numeric_eq",
        },
        DialectType {
            name: "integer",
            storage: StorageType::Integer,
            parameters: "",
            behaviour: "CAST TO integer AS IMPLICIT \
                OPERATOR '<' (integer, integer) OPERATOR '=' (integer, integer)",
        },
        DialectType {
            name: "int",
            storage: StorageType::RangedInteger {
                min: -2147483648,
                max: 2147483647,
            },
            parameters: "",
            behaviour:
                "CAST TO integer AS IMPLICIT OPERATOR '<' (int, int) OPERATOR '=' (int, int)",
        },
        DialectType {
            name: "tinyint",
            storage: StorageType::RangedInteger {
                min: -128,
                max: 127,
            },
            parameters: "width integer DEFAULT NULL",
            behaviour: "CAST TO integer AS IMPLICIT \
                OPERATOR '<' (tinyint, tinyint) OPERATOR '=' (tinyint, tinyint)",
        },
        DialectType {
            name: "ubigint",
            storage: StorageType::Uint64,
            parameters: "",
            behaviour: "CAST FROM integer, text AS IMPLICIT \
                OPERATOR '<' (ubigint, ubigint) uint64_lt \
                OPERATOR '=' (ubigint, ubigint) uint64_eq \
                OPERATOR '+' (ubigint, ubigint) RETURNS ubigint uint64_add",
        },
        DialectType {
            name: "date_days",
            storage: StorageType::DateDays,
            parameters: "",
            behaviour: "OPERATOR '<' (date_days, date_days) OPERATOR '=' (date_days, date_days)",
        },
        DialectType {
            name: "datetime_micros",
            storage: StorageType::DatetimeMicros,
            parameters: "",
            behaviour: "OPERATOR '<' (datetime_micros, datetime_micros) \
                OPERATOR '=' (datetime_micros, datetime_micros)",
        },
        DialectType {
            name: "time_micros",
            storage: StorageType::TimeMicros,
            parameters: "",
            behaviour: "OPERATOR '<' (time_micros, time_micros) time_micros_lt \
                OPERATOR '=' (time_micros, time_micros)",
        },
        DialectType {
            name: "enumeration",
            storage: StorageType::Enumeration,
            parameters: "",
            behaviour: "OPERATOR '<' (enumeration, enumeration) \
                OPERATOR '=' (enumeration, enumeration)",
        },
        DialectType {
            name: "label_set",
            storage: StorageType::LabelSet,
            parameters: "",
            behaviour: "OPERATOR '<' (label_set, label_set) OPERATOR '=' (label_set, label_set)",
        },
        DialectType {
            name: "text",
            storage: StorageType::Text,
            parameters: "",
            behaviour: "OPERATOR '<' (text, text) OPERATOR '=' (text, text)",
        },
        DialectType {
            name: "date",
            storage: StorageType::Text,
            parameters: "",
            behaviour: "OPERATOR '<' (date, date) OPERATOR '=' (date, date)",
        },
        DialectType {
            name: "datetime",
            storage: StorageType::Text,
            parameters: "",
            behaviour: "OPERATOR '<' (datetime, datetime) OPERATOR '=' (datetime, datetime)",
        },
        DialectType {
            name: "double",
            storage: StorageType::Real,
            parameters: "",
            behaviour: "OPERATOR '<' (double, double) OPERATOR '=' (double, double)",
        },
        DialectType {
            name: "varbinary",
            storage: StorageType::Blob,
            parameters: "length integer",
            behaviour: "OPERATOR '<' (varbinary, varbinary) OPERATOR '=' (varbinary, varbinary)",
        },
    ],
};

impl Dialect for MarkingDialect {
    fn name(&self) -> &'static str {
        self.name
    }

    fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        if sql.trim_start().starts_with("/*") {
            crate::bail_parse_error!("the marking dialect's parser does not read comments");
        }
        sqlite::parse(sql)
    }

    fn parse_table_sql(&self, sql: &str, root_page: i64) -> Result<BTreeTable> {
        BTreeTable::from_sql(sql, root_page)
    }

    fn parse_table_sql_ast(&self, sql: &str) -> Result<ast::Stmt> {
        sqlite::parse_table_sql_ast(sql)
    }

    fn table_sql_for_replay(&self, sql: &str) -> Result<String> {
        sqlite::table_sql_for_replay(sql)
    }

    fn format_table_sql(
        &self,
        input: &str,
        tbl_name: &ast::QualifiedName,
        body: &ast::CreateTableBody,
    ) -> Result<String> {
        SqliteDialect.format_table_sql(input, tbl_name, body)
    }

    fn register_catalog(
        &self,
        schema: &mut crate::schema::Schema,
        enable_custom_types: bool,
    ) -> Result<()> {
        sqlite::register_builtin_catalog(schema, enable_custom_types)
    }

    fn resolve_function(
        &self,
        name: &str,
        arg_count: usize,
    ) -> Result<Option<crate::function::Func>> {
        if name.eq_ignore_ascii_case("test_text_length") && arg_count == 1 {
            return Ok(Some(crate::function::Func::Dialect(
                "test_text_length".to_string(),
            )));
        }
        sqlite::resolve_builtin_function(name, arg_count)
    }

    fn exec_scalar_function(
        &self,
        _conn: &Connection,
        name: &str,
        args: &[Value],
    ) -> Result<Value> {
        assert_eq!(name, "test_text_length");
        match &args[0] {
            Value::Text(text) => Ok(Value::from_f64(text.as_str().chars().count() as f64)),
            other => Ok(other.clone()),
        }
    }

    fn requires_custom_types(&self) -> bool {
        true
    }

    fn type_rules(&self) -> &'static TypeRules {
        &TEST_TYPE_RULES
    }

    fn marks_tables(&self) -> bool {
        true
    }
}

fn test_result_parameters(operator: &str, operands: [Option<&[i64]>; 2]) -> Option<Vec<i64>> {
    let precision = |params: Option<&[i64]>| params.and_then(|p| p.first().copied()).unwrap_or(19);
    let scale = |params: Option<&[i64]>| params.and_then(|p| p.get(1).copied()).unwrap_or(0);
    let [lhs, rhs] = operands;
    match operator {
        "/" => Some(vec![precision(lhs) + 4, scale(lhs) + 4]),
        "*" => Some(vec![
            precision(lhs) + precision(rhs),
            scale(lhs) + scale(rhs),
        ]),
        "+" | "-" => {
            let scale = scale(lhs).max(scale(rhs));
            Some(vec![precision(lhs).max(precision(rhs)) + 1, scale])
        }
        _ => None,
    }
}

pub(crate) fn marking() -> Arc<dyn Dialect> {
    Arc::new(MarkingDialect { name: "marking" })
}

pub(crate) fn open_db(
    io: &Arc<dyn IO>,
    path: &str,
    dialect: Arc<dyn Dialect>,
) -> Result<Arc<Database>> {
    let file = io.open_file(path, OpenFlags::Create, true)?;
    let db_file = Arc::new(DatabaseFile::new(file));
    Database::open(
        io.clone(),
        path,
        crate::OpenOptions::new(dialect).storage(db_file),
    )
}

pub(crate) fn query(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    conn.prepare(sql).unwrap().run_collect_rows().unwrap()
}

pub(crate) fn stored_sql(conn: &Arc<Connection>, name: &str) -> String {
    let rows = query(
        conn,
        &format!("SELECT sql FROM sqlite_schema WHERE name = '{name}'"),
    );
    assert_eq!(rows.len(), 1, "no schema row for {name}");
    rows[0][0].to_string().trim_matches('\'').to_string()
}

pub(crate) fn ints(rows: &[&[i64]]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|row| row.iter().map(|v| Value::from_i64(*v)).collect())
        .collect()
}

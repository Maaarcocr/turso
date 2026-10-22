use super::{sqlite, Dialect, DialectType, SqliteDialect, StorageType, TypeRules};
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
    dialect_types: &[
        DialectType {
            name: "integer",
            storage: StorageType::Integer,
            parameters: "",
            behaviour: "OPERATOR '<' (integer, integer) OPERATOR '=' (integer, integer)",
        },
        DialectType {
            name: "int",
            storage: StorageType::RangedInteger {
                min: -2147483648,
                max: 2147483647,
            },
            parameters: "",
            behaviour: "OPERATOR '<' (int, int) OPERATOR '=' (int, int)",
        },
        DialectType {
            name: "tinyint",
            storage: StorageType::RangedInteger {
                min: -128,
                max: 127,
            },
            parameters: "width integer DEFAULT NULL",
            behaviour: "OPERATOR '<' (tinyint, tinyint) OPERATOR '=' (tinyint, tinyint)",
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
        sqlite::resolve_builtin_function(name, arg_count)
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

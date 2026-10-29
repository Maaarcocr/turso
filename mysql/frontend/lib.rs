use std::sync::Arc;
use turso_core::dialect::{ArgumentType, TypeRules};
use turso_core::schema::{BTreeTable, Schema};
use turso_core::{
    Connection, Database, Dialect, Func, LimboError, Result, SqliteDialect, Value, IO,
};
use turso_parser::ast;

mod catalog;
mod errors;
mod format;
mod functions;
mod session;
mod types;

pub use errors::{mysql_error, MysqlError};
pub use format::format_text;

#[derive(Debug)]
pub struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        parse_statement(sql, &turso_mysql_parser::Autocommit)
    }

    fn parse_for(&self, connection: &Connection, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        let (cmd, end) = parse_statement(sql, &Session(connection))?;
        if let Some(ast::Cmd::Stmt(stmt)) = &cmd {
            check_default_functions(stmt, connection)?;
        }
        Ok((cmd, end))
    }

    fn parse_reads_transaction_state(&self) -> bool {
        true
    }

    fn parse_table_sql(&self, sql: &str, root_page: i64) -> Result<BTreeTable> {
        BTreeTable::from_sql(sql, root_page)
    }

    fn parse_table_sql_ast(&self, sql: &str) -> Result<ast::Stmt> {
        turso_core::dialect::sqlite::parse_table_sql_ast(sql)
    }

    fn table_sql_for_replay(&self, sql: &str) -> Result<String> {
        turso_core::dialect::sqlite::table_sql_for_replay(sql)
    }

    fn format_table_sql(
        &self,
        input: &str,
        tbl_name: &ast::QualifiedName,
        body: &ast::CreateTableBody,
    ) -> Result<String> {
        SqliteDialect.format_table_sql(input, tbl_name, body)
    }

    fn register_catalog(&self, schema: &mut Schema, enable_custom_types: bool) -> Result<()> {
        SqliteDialect.register_catalog(schema, enable_custom_types)
    }

    fn register_native_extensions(
        &self,
        options: turso_core::OpenOptions,
    ) -> turso_core::OpenOptions {
        functions::register_variances(options)
    }

    fn resolve_function(&self, name: &str, arg_count: usize) -> Result<Option<Func>> {
        if let Some(function) = session::resolve(name, arg_count) {
            return Ok(Some(function));
        }
        if let Some(function) = catalog::resolve(name, arg_count) {
            return Ok(Some(function));
        }
        if let Some(conversion) = types::resolve(name, arg_count) {
            return Ok(Some(conversion));
        }
        if let Some(function) = functions::resolve(name, arg_count) {
            return Ok(Some(function));
        }
        SqliteDialect.resolve_function(name, arg_count)
    }

    fn exec_scalar_function(&self, conn: &Connection, name: &str, args: &[Value]) -> Result<Value> {
        if let Some(result) = session::execute(conn, name, args, None) {
            return result;
        }
        catalog::execute(name, args)
            .or_else(|| types::execute(name, args))
            .or_else(|| functions::execute(name, args))
            .unwrap_or_else(|| Err(LimboError::ParseError(format!("no such function: {name}"))))
    }

    fn exec_typed_scalar_function(
        &self,
        conn: &Connection,
        name: &str,
        args: &[Value],
        arg_types: &[ArgumentType],
    ) -> Result<Value> {
        if let Some(result) = session::execute(conn, name, args, Some(arg_types)) {
            return result;
        }
        catalog::execute(name, args)
            .or_else(|| types::execute(name, args))
            .or_else(|| functions::execute_typed(name, args, arg_types))
            .unwrap_or_else(|| Err(LimboError::ParseError(format!("no such function: {name}"))))
    }

    fn requires_custom_types(&self) -> bool {
        true
    }

    fn type_rules(&self) -> &'static TypeRules {
        types::rules()
    }

    fn marks_tables(&self) -> bool {
        true
    }
}

/// The first statement of `sql`, lowered for a session described by
/// `environment`, and the number of bytes it takes.
fn parse_statement(
    sql: &str,
    environment: &dyn turso_mysql_parser::Environment,
) -> Result<(Option<ast::Cmd>, usize)> {
    let statement = match turso_mysql_parser::first_statement(sql) {
        Ok(Some(statement)) => statement,
        Ok(None) => return Ok((None, sql.len())),
        Err(error) => return Err(LimboError::ParseError(error.to_string())),
    };
    let stmt =
        turso_mysql_parser::parse_and_translate_in(&sql[statement.text.clone()], environment)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
    Ok((Some(ast::Cmd::Stmt(stmt)), statement.end))
}

/// Refuses a column `DEFAULT` that calls a function Turso does not have, as
/// Turso would fail only the first `INSERT` that uses it where MySQL fails
/// the statement when the function does not exist (and accepts it when it
/// is a MySQL function Turso does not have).
fn check_default_functions(stmt: &ast::Stmt, connection: &Connection) -> Result<()> {
    let columns: Vec<&ast::ColumnDefinition> = match stmt {
        ast::Stmt::CreateTable {
            body: ast::CreateTableBody::ColumnsAndConstraints { columns, .. },
            ..
        } => columns.iter().collect(),
        ast::Stmt::AlterTable(alter) => match &alter.body {
            ast::AlterTableBody::AddColumn(column) => vec![column],
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let external = connection.get_syms_functions();
    let exists = |name: &str, arg_count: usize| -> Result<bool> {
        Ok(MySqlDialect.resolve_function(name, arg_count)?.is_some()
            || external.iter().any(|(external, _, external_count, _)| {
                external.eq_ignore_ascii_case(name)
                    && (*external_count < 0 || *external_count as usize == arg_count)
            }))
    };
    for column in columns {
        for constraint in &column.constraints {
            let ast::ColumnConstraint::Default(expr) = &constraint.constraint else {
                continue;
            };
            turso_core::walk_expr(expr, &mut |expr: &ast::Expr| {
                let call = match expr {
                    ast::Expr::FunctionCall { name, args, .. } => Some((name, args.len())),
                    ast::Expr::FunctionCallStar { name, .. } => Some((name, 0)),
                    _ => None,
                };
                if let Some((name, arg_count)) = call {
                    if !exists(name.as_str(), arg_count)? {
                        return Err(LimboError::ParseError(
                            turso_mysql_parser::MysqlParserError::Unsupported(format!(
                                "function `{}` in the default of column `{}`, which Turso does not have",
                                name.as_str(),
                                column.col_name.as_str()
                            ))
                            .to_string(),
                        ));
                    }
                }
                Ok(turso_core::WalkControl::Continue)
            })?;
        }
    }
    Ok(())
}

/// The session of a connection, as the lowering of its statements sees it.
struct Session<'a>(&'a Connection);

impl turso_mysql_parser::Environment for Session<'_> {
    fn in_transaction(&self) -> bool {
        !self.0.get_auto_commit()
    }

    fn index_table(&self, name: &str) -> LoweringResult<Option<String>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema.indexes.iter().find_map(|(table, indexes)| {
                indexes
                    .iter()
                    .any(|index| index.name.eq_ignore_ascii_case(name))
                    .then(|| table.clone())
            })
        }))
    }

    fn has_triggers(&self) -> bool {
        self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema
                .triggers
                .values()
                .any(|triggers| !triggers.is_empty())
        })
    }

    fn triggers_on(&self, table: &str) -> LoweringResult<Vec<String>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema
                .get_triggers_for_table(table)
                .map(|trigger| trigger.sql.clone())
                .collect()
        }))
    }

    fn foreign_keys_to(
        &self,
        table: &str,
    ) -> LoweringResult<Vec<turso_mysql_parser::ForeignKeyTo>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema
                .tables
                .values()
                .filter_map(|child| child.btree())
                .flat_map(|child| {
                    child
                        .foreign_keys
                        .iter()
                        .filter(|foreign_key| foreign_key.parent_table.eq_ignore_ascii_case(table))
                        .map(|foreign_key| turso_mysql_parser::ForeignKeyTo {
                            table: child.name.clone(),
                            on_delete: foreign_key.on_delete,
                            on_update: foreign_key.on_update,
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        }))
    }

    fn columns_of(
        &self,
        table: &str,
    ) -> LoweringResult<Option<Vec<turso_mysql_parser::ColumnFacts>>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema.get_btree_table(table).map(|table| {
                table
                    .columns()
                    .iter()
                    .map(|column| turso_mysql_parser::ColumnFacts {
                        name: column.name.clone().unwrap_or_default(),
                        auto_increment: table.has_autoincrement && column.is_rowid_alias(),
                        generated: column.is_generated(),
                        default: column.default.as_deref().cloned(),
                        not_null: column.notnull() || column.primary_key(),
                    })
                    .collect()
            })
        }))
    }

    fn table_sql(&self, table: &str) -> LoweringResult<Option<String>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema.get_btree_table(table).map(|table| table.to_sql())
        }))
    }

    fn unique_keys_of(&self, table: &str) -> LoweringResult<Vec<Vec<String>>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            let primary_key = schema.get_btree_table(table).and_then(|table| {
                (!table.primary_key_columns.is_empty()).then(|| {
                    table
                        .primary_key_columns
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect::<Vec<_>>()
                })
            });
            let unique_indexes = schema
                .get_indices(table)
                .filter(|index| {
                    index.unique
                        && index.where_clause.is_none()
                        && index.columns.iter().all(|column| column.expr.is_none())
                })
                .map(|index| {
                    index
                        .columns
                        .iter()
                        .map(|column| column.name.clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();
            primary_key.into_iter().chain(unique_indexes).collect()
        }))
    }

    fn foreign_key_names(&self) -> LoweringResult<Vec<String>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema
                .tables
                .values()
                .filter_map(|table| table.btree())
                .flat_map(|table| {
                    table
                        .foreign_keys
                        .iter()
                        .filter_map(|foreign_key| foreign_key.name.clone())
                        .collect::<Vec<_>>()
                })
                .collect()
        }))
    }

    fn foreign_key_checks(&self) -> bool {
        self.0.foreign_keys_enabled()
    }

    fn foreign_keys_of(
        &self,
        table: &str,
    ) -> LoweringResult<Vec<turso_mysql_parser::ForeignKeyOf>> {
        Ok(self.0.with_schema(turso_core::MAIN_DB_ID, |schema| {
            schema
                .get_btree_table(table)
                .map_or_else(Vec::new, |table| {
                    table
                        .foreign_keys
                        .iter()
                        .map(|foreign_key| turso_mysql_parser::ForeignKeyOf {
                            child_columns: foreign_key.child_columns.to_vec(),
                            parent_table: foreign_key.parent_table.clone(),
                            parent_columns: foreign_key.parent_columns.to_vec(),
                        })
                        .collect()
                })
        }))
    }

    fn prepared_statement(&self, name: &str) -> Option<String> {
        self.0
            .context::<session::Session>()?
            .prepared_statement(name)
    }
}

type LoweringResult<T> = std::result::Result<T, turso_mysql_parser::MysqlParserError>;

/// Opens the database at `path` on `io` with the MySQL dialect and returns a
/// connection to it that checks foreign keys, as a MySQL session starts with
/// `FOREIGN_KEY_CHECKS = 1`, and keeps the session's user variables.
pub fn open_with_io(io: Arc<dyn IO>, path: &str) -> Result<Arc<Connection>> {
    let database = Database::open(
        io,
        path,
        turso_core::OpenOptions::new(Arc::new(MySqlDialect))
            .db_opts(turso_core::DatabaseOpts::new().with_generated_columns(true)),
    )?;
    let connection = database.connect_with_context(Arc::new(session::Session::default()))?;
    connection
        .context::<session::Session>()
        .expect("the connection has its session")
        .attach(&connection);
    connection.set_foreign_keys_enabled(true);
    Ok(connection)
}

/// Opens (creating it if needed) the database file at `path` with the MySQL
/// dialect.
pub fn open_file(path: &str) -> Result<Arc<Connection>> {
    open_with_io(Database::io_for_path(path)?, path)
}

/// Opens a new in-memory database with the MySQL dialect.
pub fn open_memory() -> Result<Arc<Connection>> {
    open_with_io(Arc::new(turso_core::MemoryIO::new()), ":memory:")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn consumed(sql: &str) -> (bool, usize) {
        let (cmd, consumed) = MySqlDialect.parse(sql).expect("SQL must parse");
        (cmd.is_some(), consumed)
    }

    #[test]
    fn parses_the_first_statement_and_reports_its_end() {
        assert_eq!(consumed("SELECT 1; SELECT 2"), (true, 9));
        assert_eq!(consumed("SELECT 'a;b' /* ; */ ;\nSELECT 2"), (true, 22));
        assert_eq!(consumed("SELECT 1 -- done"), (true, 16));
        for empty in ["", "  \n", "-- comment", "# comment\n;;"] {
            assert_eq!(consumed(empty), (false, empty.len()), "{empty:?}");
        }
    }

    #[test]
    fn executes_multi_statement_text_statement_by_statement() -> Result<()> {
        let connection = open_memory()?;
        connection.execute("CREATE TABLE a(x INTEGER); # one;\nCREATE TABLE b(x INTEGER);")?;
        for table in ["a", "b"] {
            let error = connection
                .execute(format!("CREATE TABLE {table}(x INTEGER)"))
                .expect_err("table must exist");
            assert!(error.to_string().contains("already exists"), "{error}");
        }
        connection.close()?;
        Ok(())
    }

    #[test]
    fn hello_world_runs_through_mysql_dialect() -> Result<()> {
        let connection = open_memory()?;
        let mut statement = connection.prepare("SELECT 'hello world' AS greeting")?;
        let rows = statement.run_collect_rows()?;
        assert_eq!(rows, vec![vec![Value::build_text("hello world")]]);
        connection.close()?;
        Ok(())
    }
}

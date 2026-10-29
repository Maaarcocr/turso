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
        parse_statement(sql, &Session(connection))
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

/// The session of a connection, as the lowering of its statements sees it.
struct Session<'a>(&'a Connection);

impl turso_mysql_parser::Environment for Session<'_> {
    fn in_transaction(&self) -> bool {
        !self.0.get_auto_commit()
    }
}

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

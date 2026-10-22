use std::sync::Arc;
use turso_core::schema::{BTreeTable, Schema};
use turso_core::{
    Connection, Database, Dialect, Func, LimboError, Result, SqliteDialect, Value, IO,
};
use turso_parser::ast;

mod functions;

#[derive(Debug)]
pub struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        // Parse only the first statement and report the input consumed
        // through its semicolon, so Connection can iterate multi-statement
        // text. Text the MySQL tokenizer rejects is not MySQL; engine helper
        // SQL is canonical SQLite text, so parse it as SQLite.
        let statement = match turso_mysql_parser::first_statement(sql) {
            Ok(Some(statement)) => statement,
            Ok(None) => return Ok((None, sql.len())),
            Err(_) => return turso_core::dialect::sqlite::parse(sql),
        };
        let text = &sql[statement.text.clone()];
        match turso_mysql_parser::parse_and_translate(text) {
            Ok(stmt) => return Ok((Some(ast::Cmd::Stmt(stmt)), statement.end)),
            Err(error @ turso_mysql_parser::MysqlParserError::Rejected(_)) => {
                return Err(LimboError::ParseError(error.to_string()));
            }
            Err(_) => {}
        }
        // Anything the MySQL frontend cannot lower yet falls back to SQLite
        // parsing of the same statement, mirroring the PostgreSQL frontend.
        // SQLite must see the same single statement: if it ended earlier
        // (its lexer disagrees about a quote or comment), resuming after its
        // end could run text that MySQL treats as part of a string.
        let (cmd, consumed) = turso_core::dialect::sqlite::parse(text)?;
        let Some(cmd) = cmd else {
            return Err(LimboError::ParseError(format!(
                "MySQL statement is empty when parsed as SQLite: {text}"
            )));
        };
        if let (Some(_), _) = turso_core::dialect::sqlite::parse(&text[consumed..])? {
            return Err(LimboError::ParseError(format!(
                "MySQL and SQLite disagree on where this statement ends: {text}"
            )));
        }
        Ok((Some(cmd), statement.end))
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
        if let Some(function) = functions::resolve(name, arg_count) {
            return Ok(Some(function));
        }
        SqliteDialect.resolve_function(name, arg_count)
    }

    fn exec_scalar_function(
        &self,
        _conn: &Connection,
        name: &str,
        args: &[Value],
    ) -> Result<Value> {
        functions::execute(name, args)
            .unwrap_or_else(|| Err(LimboError::ParseError(format!("no such function: {name}"))))
    }
}

/// Opens the database at `path` on `io` with the MySQL dialect and returns a
/// connection to it.
pub fn open_with_io(io: Arc<dyn IO>, path: &str) -> Result<Arc<Connection>> {
    let database = Database::open(
        io,
        path,
        turso_core::OpenOptions::new(Arc::new(MySqlDialect)),
    )?;
    database.connect(turso_core::ConnectOpts::new())
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
        // The SQLite fallback consumes the same statement.
        assert_eq!(consumed("PRAGMA page_size; SELECT 2"), (true, 17));
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
    fn sqlite_fallback_cannot_end_a_statement_early() {
        // MySQL reads one statement whose string contains `; DROP TABLE t;`.
        // It is not lowered yet, and SQLite would read it as several
        // statements, so the fallback must refuse it rather than run the
        // statements SQLite sees after the first.
        let sql = "SELECT x FROM t WHERE y = 'a\\'; DROP TABLE t; -- '";
        let error = MySqlDialect.parse(sql).expect_err("lexers disagree");
        assert!(error.to_string().contains("disagree"), "{error}");
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

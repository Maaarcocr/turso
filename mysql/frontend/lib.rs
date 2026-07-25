use std::sync::Arc;
use turso_core::schema::{BTreeTable, Schema};
use turso_core::{Dialect, Func, LimboError, Result, SqliteDialect};
use turso_parser::ast;

#[derive(Debug)]
pub struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        let statements = turso_mysql_parser::parse(sql)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        if statements.is_empty() {
            return Ok((None, sql.len()));
        }
        if statements.len() != 1 {
            return Err(LimboError::ParseError(format!(
                "expected one MySQL statement, found {}",
                statements.len()
            )));
        }
        let statement = turso_mysql_parser::translate(&statements[0])
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(ast::Cmd::Stmt(statement)), sql.len()))
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
        SqliteDialect.resolve_function(name, arg_count)
    }
}

pub fn open_memory() -> Result<Arc<turso_core::Connection>> {
    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::MemoryIO::new());
    let database = turso_core::Database::open(
        io,
        ":memory:",
        turso_core::OpenOptions::new(Arc::new(MySqlDialect)),
    )?;
    database.connect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_core::Value;

    #[test]
    fn hello_world_runs_through_mysql_dialect() -> Result<()> {
        let connection = open_memory()?;
        let mut statement = connection.prepare("SELECT 'hello world' AS greeting")?;
        let rows = statement.run_collect_rows()?;
        assert_eq!(rows, vec![vec![Value::build_text("hello world")]]);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn unsupported_syntax_is_not_silently_ignored() -> Result<()> {
        let connection = open_memory()?;
        let error = connection
            .prepare("SELECT greeting FROM messages")
            .expect_err("unsupported FROM must return an error");
        assert!(error
            .to_string()
            .contains("literal and anonymous-parameter SELECT projections"));
        connection.close()?;
        Ok(())
    }
}

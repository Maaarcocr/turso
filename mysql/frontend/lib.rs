use std::sync::Arc;
use turso_core::schema::{BTreeTable, Schema};
use turso_core::{Dialect, Func, Result, SqliteDialect};
use turso_parser::ast;

#[derive(Debug)]
pub struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn parse(&self, sql: &str) -> Result<(Option<ast::Cmd>, usize)> {
        // Engine-generated helper statements and pragmas are canonical SQLite
        // text, so anything the MySQL frontend cannot handle falls back to
        // SQLite parsing, mirroring the PostgreSQL frontend.
        let Ok(statements) = turso_mysql_parser::parse(sql) else {
            return turso_core::dialect::sqlite::parse(sql);
        };
        let [statement] = statements.as_slice() else {
            return turso_core::dialect::sqlite::parse(sql);
        };
        match turso_mysql_parser::translate(statement) {
            Ok(statement) => Ok((Some(ast::Cmd::Stmt(statement)), sql.len())),
            Err(_) => turso_core::dialect::sqlite::parse(sql),
        }
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
    database.connect(turso_core::ConnectOpts::new())
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
}

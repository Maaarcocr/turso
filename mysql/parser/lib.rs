use sqlparser::ast::Statement;
use sqlparser::parser::ParserError;
use sqlparser::tokenizer::TokenizerError;
use thiserror::Error;
use turso_parser::ast;

mod bitwise;
pub mod datetime;
mod ddl;
mod ddl_alter;
mod ddl_column;
mod ddl_default;
mod ddl_expr;
mod ddl_index;
mod ddl_type;
mod ddl_view;
mod dialect;
mod dml;
mod expr;
mod function_names;
mod insert;
mod invalid_json;
mod json;
mod literal;
mod names;
mod query;
mod session;
mod split;

pub use function_names::{BINARY_STRING_FUNCTIONS, DATE_TIME_FUNCTIONS};
pub use session::keeps_target_profile;
pub use split::{first_statement, split_statements, StatementSpan};

#[derive(Debug, Error)]
pub enum MysqlParserError {
    #[error("{0}")]
    Parse(#[from] ParserError),
    #[error("{0}")]
    Tokenize(#[from] TokenizerError),
    #[error("cannot locate MySQL statement boundaries: {0}")]
    Split(String),
    #[error("the SQL input contains no statements")]
    Empty,
    #[error("expected one MySQL statement, found {0}")]
    MultipleStatements(usize),
    #[error("unsupported MySQL syntax: {0}")]
    Unsupported(String),
    #[error("MySQL rejects this statement: {0}")]
    Rejected(String),
}

pub fn parse(sql: &str) -> Result<Vec<Statement>, MysqlParserError> {
    dialect::parse_statements(sql)
}

pub fn parse_one(sql: &str) -> Result<Statement, MysqlParserError> {
    let mut statements = parse(sql)?;
    match statements.len() {
        0 => Err(MysqlParserError::Empty),
        1 => Ok(statements.remove(0)),
        count => Err(MysqlParserError::MultipleStatements(count)),
    }
}

pub fn parse_and_translate(sql: &str) -> Result<ast::Stmt, MysqlParserError> {
    let statement = parse_one(sql)?;
    translate(&statement)
}

pub fn translate(statement: &Statement) -> Result<ast::Stmt, MysqlParserError> {
    match statement {
        Statement::Query(query) => {
            let mut context = query::TranslationContext::default();
            query::translate_query(query, &mut context)
        }
        Statement::CreateTable(create_table) => ddl::translate_create_table(create_table),
        Statement::CreateIndex(create_index) => ddl_index::translate_create_index(create_index),
        Statement::AlterTable(alter_table) => ddl_alter::translate_alter_table(alter_table),
        Statement::RenameTable(renames) => ddl_alter::translate_rename_table(renames),
        Statement::Drop { .. } => ddl_alter::translate_drop(statement),
        Statement::CreateView(create_view) => ddl_view::translate_create_view(create_view),
        Statement::Set(set) => session::translate_set(set),
        Statement::Insert(insert) => insert::translate_insert(insert),
        Statement::Update(update) => dml::translate_update(update),
        Statement::Delete(delete) => dml::translate_delete(delete),
        _ => {
            let text = statement.to_string();
            let keyword = text.split_whitespace().next().unwrap_or_default();
            Err(MysqlParserError::Unsupported(format!(
                "{} statements are not available",
                keyword.to_ascii_uppercase()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::ast::{Expr, SelectItem, SetExpr};

    #[test]
    fn parses_mysql_quoted_identifier() {
        let statement = parse_one("SELECT `greeting`").expect("MySQL query must parse");
        let Statement::Query(query) = statement else {
            panic!("SELECT must produce a query statement");
        };
        let SetExpr::Select(select) = query.body.as_ref() else {
            panic!("SELECT must produce a select query body");
        };
        let SelectItem::UnnamedExpr(Expr::Identifier(identifier)) = &select.projection[0] else {
            panic!("projection must contain the quoted identifier");
        };
        assert_eq!(identifier.value, "greeting");
        assert_eq!(identifier.quote_style, Some('`'));
    }

    #[test]
    fn lowers_hello_world_select() {
        let statement = parse_and_translate("SELECT 'hello world' AS greeting")
            .expect("hello world query must lower");
        assert_eq!(statement.to_string(), "SELECT 'hello world' AS greeting");
    }

    #[test]
    fn lowers_from_clause() {
        let statement =
            parse_and_translate("SELECT greeting FROM messages").expect("FROM must lower");
        assert_eq!(statement.to_string(), "SELECT greeting FROM messages");
    }

    #[test]
    fn rejects_multiple_statements() {
        let error = parse_one("SELECT 1; SELECT 2")
            .expect_err("multiple statements need consumed-offset support");
        assert!(matches!(error, MysqlParserError::MultipleStatements(2)));
    }
}

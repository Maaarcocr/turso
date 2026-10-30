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
mod foreign_key;
mod function_names;
mod having;
mod insert;
mod invalid_json;
mod json;
mod json_table;
mod literal;
mod names;
mod prepared;
mod query;
mod recursive;
mod session;
mod show;
mod split;
mod stored_table;
mod table_statement;
mod transaction;
mod trigger;

pub use ddl_default::{temporal_text, TemporalColumn};
pub use function_names::{BINARY_STRING_FUNCTIONS, DATE_TIME_FUNCTIONS};
pub use prepared::lower_prepared;
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

/// What the lowering of a statement may know about the session that runs it.
pub trait Environment {
    /// Whether a transaction started with `BEGIN` or `START TRANSACTION` is
    /// open.
    fn in_transaction(&self) -> bool;

    /// The table that the index `name` (compared without case) belongs to,
    /// or `None` when no table has it.
    fn index_table(&self, name: &str) -> Result<Option<String>, MysqlParserError>;

    /// Whether the schema has a trigger.
    fn has_triggers(&self) -> bool;

    /// The stored `CREATE TRIGGER` statements of the triggers on `table`.
    fn triggers_on(&self, table: &str) -> Result<Vec<String>, MysqlParserError>;

    /// The foreign keys of other tables (or of `table` itself) that
    /// reference `table`.
    fn foreign_keys_to(&self, table: &str) -> Result<Vec<ForeignKeyTo>, MysqlParserError>;

    /// The columns of `table`, or `None` when there is no such table.
    fn columns_of(&self, table: &str) -> Result<Option<Vec<ColumnFacts>>, MysqlParserError>;

    /// The stored `CREATE TABLE` of `table`, or `None` when there is no such
    /// table.
    fn table_sql(&self, table: &str) -> Result<Option<String>, MysqlParserError>;

    /// The columns of the primary key and of each unique index of `table`.
    fn unique_keys_of(&self, table: &str) -> Result<Vec<Vec<String>>, MysqlParserError>;

    /// The names of the foreign keys of every table.
    fn foreign_key_names(&self) -> Result<Vec<String>, MysqlParserError>;

    /// Whether `FOREIGN_KEY_CHECKS` is 1.
    fn foreign_key_checks(&self) -> bool;

    /// The foreign keys of `table`.
    fn foreign_keys_of(&self, table: &str) -> Result<Vec<ForeignKeyOf>, MysqlParserError>;

    /// The text of the session's prepared statement `name` (lower case), or
    /// `None` when the session has none of that name.
    fn prepared_statement(&self, name: &str) -> Option<String>;
}

/// A foreign key of the table asked about.
pub struct ForeignKeyOf {
    pub child_columns: Vec<String>,
    pub parent_table: String,
    pub parent_columns: Vec<String>,
}

/// A foreign key of `table` that references the table asked about.
pub struct ForeignKeyTo {
    pub table: String,
    pub on_delete: ast::RefAct,
    pub on_update: ast::RefAct,
}

/// What the lowering knows about a column of a table.
pub struct ColumnFacts {
    pub name: String,
    /// The column is the `AUTO_INCREMENT` key.
    pub auto_increment: bool,
    pub generated: bool,
    /// The stored default of the column.
    pub default: Option<ast::Expr>,
    /// The column is `NOT NULL` (or in the primary key).
    pub not_null: bool,
    /// The column holds binary strings (`BINARY`, `VARBINARY` or a `BLOB`
    /// type).
    pub binary: bool,
}

/// A session outside any transaction on an empty database.
pub struct Autocommit;

impl Environment for Autocommit {
    fn in_transaction(&self) -> bool {
        false
    }

    fn index_table(&self, _name: &str) -> Result<Option<String>, MysqlParserError> {
        Ok(None)
    }

    fn has_triggers(&self) -> bool {
        false
    }

    fn triggers_on(&self, _table: &str) -> Result<Vec<String>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_keys_to(&self, _table: &str) -> Result<Vec<ForeignKeyTo>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn columns_of(&self, _table: &str) -> Result<Option<Vec<ColumnFacts>>, MysqlParserError> {
        Ok(None)
    }

    fn table_sql(&self, _table: &str) -> Result<Option<String>, MysqlParserError> {
        Ok(None)
    }

    fn unique_keys_of(&self, _table: &str) -> Result<Vec<Vec<String>>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_key_names(&self) -> Result<Vec<String>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn foreign_key_checks(&self) -> bool {
        true
    }

    fn foreign_keys_of(&self, _table: &str) -> Result<Vec<ForeignKeyOf>, MysqlParserError> {
        Ok(Vec::new())
    }

    fn prepared_statement(&self, _name: &str) -> Option<String> {
        None
    }
}

pub fn parse_and_translate(sql: &str) -> Result<ast::Stmt, MysqlParserError> {
    parse_and_translate_in(sql, &Autocommit)
}

pub fn parse_and_translate_in(
    sql: &str,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let first_word = sql
        .trim_start()
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .next()
        .unwrap_or_default();
    if ["PREPARE", "EXECUTE", "DEALLOCATE", "DROP"]
        .iter()
        .any(|keyword| first_word.eq_ignore_ascii_case(keyword))
    {
        if let Some(command) = prepared::command(sql)? {
            return prepared::translate(command, environment);
        }
    }
    if first_word.eq_ignore_ascii_case("TABLE") {
        return translate_text(&table_statement::as_select(sql)?, environment);
    }
    translate_text(sql, environment)
}

/// Lowers the one statement `sql`, which is not a prepared-statement
/// command.
fn translate_text(sql: &str, environment: &dyn Environment) -> Result<ast::Stmt, MysqlParserError> {
    let statement = parse_one(sql)?;
    if let Statement::Set(sqlparser::ast::Set::SetTransaction {
        modes,
        snapshot: None,
        session: false,
    }) = &statement
    {
        let scope = dialect::word_after_set(sql);
        return transaction::translate_set_transaction(modes, scope.as_deref(), &statement);
    }
    translate_in(&statement, environment)
}

pub fn translate(statement: &Statement) -> Result<ast::Stmt, MysqlParserError> {
    translate_in(statement, &Autocommit)
}

/// Lowers `statement` for a session described by `environment`. Inside a
/// transaction, DDL, which MySQL runs after committing the transaction, is
/// refused.
pub fn translate_in(
    statement: &Statement,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let in_transaction = environment.in_transaction();
    if in_transaction
        && matches!(
            statement,
            Statement::CreateTable(_)
                | Statement::CreateIndex(_)
                | Statement::AlterTable(_)
                | Statement::RenameTable(_)
                | Statement::Drop { .. }
                | Statement::CreateView(_)
                | Statement::Truncate(_)
                | Statement::LockTables { .. }
                | Statement::UnlockTables
                | Statement::CreateTrigger(_)
                | Statement::DropTrigger(_)
        )
    {
        let text = statement.to_string();
        let keyword = text
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        return Err(MysqlParserError::Unsupported(format!(
            "{} inside a transaction, which MySQL commits first, is not lowered yet",
            keyword.to_ascii_uppercase()
        )));
    }
    if environment.has_triggers()
        && matches!(
            statement,
            Statement::AlterTable(_) | Statement::RenameTable(_)
        )
    {
        return Err(MysqlParserError::Unsupported(
            "ALTER TABLE and RENAME TABLE in a database with triggers, whose stored statements Turso would lower again, are not lowered yet"
                .to_string(),
        ));
    }
    let lowered = match statement {
        Statement::Insert(insert) => insert::translate_insert(insert, environment)?,
        Statement::Update(update) => dml::translate_update(update, environment)?,
        Statement::Delete(delete) => dml::translate_delete(delete)?,
        _ => return translate_other(statement, environment),
    };
    trigger::check_fired_triggers(statement, &lowered, environment)?;
    foreign_key::check_writes(&lowered, environment)?;
    Ok(lowered)
}

fn translate_other(
    statement: &Statement,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let in_transaction = environment.in_transaction();
    match statement {
        Statement::CreateTrigger(create) => trigger::translate_create_trigger(create, environment),
        Statement::DropTrigger(drop) => trigger::translate_drop_trigger(drop),
        Statement::StartTransaction { .. }
        | Statement::Commit { .. }
        | Statement::Rollback { .. }
        | Statement::Savepoint { .. }
        | Statement::ReleaseSavepoint { .. } => {
            transaction::translate_transaction(statement, in_transaction)
        }
        Statement::Query(query) => {
            let mut context = query::TranslationContext::for_session(environment);
            query::translate_query(query, &mut context)
        }
        Statement::CreateTable(create_table) => {
            ddl::translate_create_table(create_table, environment)
        }
        Statement::CreateIndex(create_index) => {
            ddl_index::translate_create_index(create_index, environment)
        }
        Statement::AlterTable(alter_table) => {
            ddl_alter::translate_alter_table(alter_table, environment)
        }
        Statement::RenameTable(renames) => ddl_alter::translate_rename_table(renames),
        Statement::Drop { .. } => ddl_alter::translate_drop(statement, environment),
        Statement::CreateView(create_view) => ddl_view::translate_create_view(create_view),
        Statement::Set(set) => session::translate_set(set, in_transaction),
        Statement::ShowTables {
            terse: false,
            history: false,
            extended: false,
            full,
            external: false,
            show_options,
        } => show::translate_show_tables(*full, show_options),
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

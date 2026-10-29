use crate::MysqlParserError;
use sqlparser::ast::{ShowStatementFilter, ShowStatementFilterPosition, ShowStatementOptions};
use turso_parser::ast;

/// `SHOW [FULL] TABLES [LIKE 'pattern']`: the names of the database's
/// tables and views, with the case they were created with, in byte order, as
/// MySQL lists them (its table names compare by their bytes), with
/// `Table_type` (`BASE TABLE` or `VIEW`) for `FULL`. The pattern matches with
/// case, like MySQL's. The column is named `Tables_in_main` after the
/// database, which Turso calls `main`. `__mysql_schema_name(sql)` reads each
/// name from its stored SQL, since `sqlite_schema.name` is in lower case.
pub(crate) fn translate_show_tables(
    full: bool,
    options: &ShowStatementOptions,
) -> Result<ast::Stmt, MysqlParserError> {
    let ShowStatementOptions {
        show_in,
        starts_with,
        limit,
        limit_from,
        filter_position,
    } = options;
    if show_in.is_some() || starts_with.is_some() || limit.is_some() || limit_from.is_some() {
        return Err(unsupported(&format!("SHOW TABLES clause in `{options}`")));
    }
    let filter = match filter_position {
        None => String::new(),
        Some(ShowStatementFilterPosition::Suffix(ShowStatementFilter::Like(pattern))) => {
            format!(" WHERE name GLOB '{}'", glob_pattern(pattern)?.replace('\'', "''"))
        }
        Some(_) => return Err(unsupported(&format!("SHOW TABLES filter in `{options}`"))),
    };
    let table_type = if full {
        ", CASE type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END AS Table_type"
    } else {
        ""
    };
    sqlite_statement(&format!(
        "SELECT name AS Tables_in_main{table_type} FROM (\
         SELECT __mysql_schema_name(sql) AS name, type FROM sqlite_schema \
         WHERE type IN ('table', 'view') AND lower(substr(name, 1, 7)) <> 'sqlite_' \
         AND substr(name, 1, 16) <> '__turso_internal'){filter} ORDER BY name"
    ))
}

/// The GLOB pattern that matches what the LIKE pattern `like` matches with
/// case: `%` is `*`, `_` is `?`, and GLOB's own wildcards match themselves.
/// A pattern with an escape is refused.
fn glob_pattern(like: &str) -> Result<String, MysqlParserError> {
    let mut glob = String::with_capacity(like.len());
    for character in like.chars() {
        match character {
            '%' => glob.push('*'),
            '_' => glob.push('?'),
            '\\' => {
                return Err(unsupported(&format!(
                    "LIKE pattern `{like}` with an escape in SHOW TABLES"
                )))
            }
            '*' | '?' | '[' => {
                glob.push('[');
                glob.push(character);
                glob.push(']');
            }
            other => glob.push(other),
        }
    }
    Ok(glob)
}

fn sqlite_statement(sql: &str) -> Result<ast::Stmt, MysqlParserError> {
    match turso_parser::parser::Parser::new(sql.as_bytes()).next_cmd() {
        Ok(Some(ast::Cmd::Stmt(statement))) => Ok(statement),
        other => panic!("the SHOW query `{sql}` must parse as SQLite: {other:?}"),
    }
}

fn unsupported(what: &str) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, Expr, Ident, ShowStatementFilter, ShowStatementFilterPosition,
    ShowStatementOptions, Value,
};
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
            format!(
                " WHERE name GLOB '{}'",
                glob_pattern(pattern)?.replace('\'', "''")
            )
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

/// `SHOW [SESSION] VARIABLES LIKE 'name'` or `WHERE Variable_name = 'name'`
/// of one system variable that `@@name` reads: its name and its value as
/// MySQL shows it (`ON` or `OFF` for a switch). The name compares without
/// case. A `LIKE` pattern with `%` or an escape, which may match variables
/// the frontend does not know, is refused; `_` matches itself, as no other
/// MySQL variable differs from a known one only where that has `_`.
pub(crate) fn translate_show_variables(
    filter: Option<&ShowStatementFilter>,
) -> Result<ast::Stmt, MysqlParserError> {
    let refused = || match filter {
        Some(filter) => unsupported(&format!("SHOW VARIABLES {filter}")),
        None => unsupported("SHOW VARIABLES without a filter"),
    };
    let name = match filter {
        Some(ShowStatementFilter::Like(pattern)) if !pattern.contains(['%', '\\']) => pattern,
        Some(ShowStatementFilter::Where(Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        })) => match (left.as_ref(), right.as_ref()) {
            (Expr::Identifier(column), Expr::Value(value))
                if column.value.eq_ignore_ascii_case("Variable_name") =>
            {
                match &value.value {
                    Value::SingleQuotedString(name) | Value::DoubleQuotedString(name) => name,
                    _ => return Err(refused()),
                }
            }
            _ => return Err(refused()),
        },
        _ => return Err(refused()),
    };
    let name = name.to_ascii_lowercase();
    let value = match name.as_str() {
        "autocommit" | "unique_checks" | "explicit_defaults_for_timestamp" => {
            crate::expr::string_expr("ON")
        }
        "sql_auto_is_null" | "transaction_read_only" | "sql_safe_updates" => {
            crate::expr::string_expr("OFF")
        }
        _ => crate::session::system_variable("@@", &Ident::new(name.as_str()))
            .map_err(|_| refused())?,
    };
    let column = |expr: ast::Expr, alias: &str| {
        ast::ResultColumn::Expr(
            Box::new(expr),
            Some(ast::As::As(ast::Name::exact(alias.to_string()))),
        )
    };
    Ok(ast::Stmt::Select(ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns: vec![
                    column(crate::expr::string_expr(&name), "Variable_name"),
                    column(value, "Value"),
                ],
                from: None,
                where_clause: None,
                group_by: None,
                window_clause: Vec::new(),
            },
            compounds: Vec::new(),
        },
        order_by: Vec::new(),
        limit: None,
    }))
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

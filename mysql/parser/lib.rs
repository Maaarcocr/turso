use sqlparser::ast::{
    Expr, GroupByExpr, Query, Select, SelectFlavor, SelectItem, SetExpr, Statement, Value,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::{Parser, ParserError};
use thiserror::Error;
use turso_parser::ast;

#[derive(Debug, Error)]
pub enum MysqlParserError {
    #[error("{0}")]
    Parse(#[from] ParserError),
    #[error("the SQL input contains no statements")]
    Empty,
    #[error("expected one MySQL statement, found {0}")]
    MultipleStatements(usize),
    #[error("unsupported MySQL syntax: {0}")]
    Unsupported(String),
}

pub fn parse(sql: &str) -> Result<Vec<Statement>, MysqlParserError> {
    Parser::parse_sql(&MySqlDialect {}, sql).map_err(MysqlParserError::from)
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
    let Statement::Query(query) = statement else {
        return Err(MysqlParserError::Unsupported(
            "only SELECT is available in the bootstrap frontend".to_string(),
        ));
    };
    translate_query(query)
}

fn translate_query(query: &Query) -> Result<ast::Stmt, MysqlParserError> {
    let Query {
        with,
        body,
        order_by,
        limit_clause,
        fetch,
        locks,
        for_clause,
        settings,
        format_clause,
        pipe_operators,
    } = query;
    if with.is_some()
        || order_by.is_some()
        || limit_clause.is_some()
        || fetch.is_some()
        || !locks.is_empty()
        || for_clause.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || !pipe_operators.is_empty()
    {
        return Err(MysqlParserError::Unsupported(
            "only a SELECT projection is available in the bootstrap frontend".to_string(),
        ));
    }

    let SetExpr::Select(select) = body.as_ref() else {
        return Err(MysqlParserError::Unsupported(
            "set operations and nested query bodies are not available in the bootstrap frontend"
                .to_string(),
        ));
    };
    translate_select(select)
}

fn translate_select(select: &Select) -> Result<ast::Stmt, MysqlParserError> {
    let Select {
        select_token: _,
        optimizer_hints,
        distinct,
        select_modifiers,
        top,
        top_before_distinct,
        projection,
        exclude,
        into,
        from,
        lateral_views,
        prewhere,
        selection,
        connect_by,
        group_by,
        cluster_by,
        distribute_by,
        sort_by,
        having,
        named_window,
        qualify,
        window_before_qualify,
        value_table_mode,
        flavor,
    } = select;
    let empty_group_by = matches!(
        group_by,
        GroupByExpr::Expressions(expressions, modifiers)
            if expressions.is_empty() && modifiers.is_empty()
    );
    if !optimizer_hints.is_empty()
        || distinct.is_some()
        || select_modifiers.is_some()
        || top.is_some()
        || *top_before_distinct
        || exclude.is_some()
        || into.is_some()
        || !from.is_empty()
        || !lateral_views.is_empty()
        || prewhere.is_some()
        || selection.is_some()
        || !connect_by.is_empty()
        || !empty_group_by
        || !cluster_by.is_empty()
        || !distribute_by.is_empty()
        || !sort_by.is_empty()
        || having.is_some()
        || !named_window.is_empty()
        || qualify.is_some()
        || *window_before_qualify
        || value_table_mode.is_some()
        || *flavor != SelectFlavor::Standard
    {
        return Err(MysqlParserError::Unsupported(
            "only literal-only SELECT projections are available in the bootstrap frontend"
                .to_string(),
        ));
    }
    if projection.is_empty() {
        return Err(MysqlParserError::Unsupported(
            "SELECT requires at least one projection".to_string(),
        ));
    }

    let columns = projection
        .iter()
        .map(translate_projection)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::Stmt::Select(ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns,
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

fn translate_projection(item: &SelectItem) -> Result<ast::ResultColumn, MysqlParserError> {
    match item {
        SelectItem::UnnamedExpr(expr) => Ok(ast::ResultColumn::Expr(
            Box::new(translate_expr(expr)?),
            None,
        )),
        SelectItem::ExprWithAlias { expr, alias } => Ok(ast::ResultColumn::Expr(
            Box::new(translate_expr(expr)?),
            Some(ast::As::As(ast::Name::exact(alias.value.clone()))),
        )),
        _ => Err(MysqlParserError::Unsupported(
            "wildcards and multiple aliases are not available in the bootstrap frontend"
                .to_string(),
        )),
    }
}

fn translate_expr(expr: &Expr) -> Result<ast::Expr, MysqlParserError> {
    let Expr::Value(value) = expr else {
        return Err(MysqlParserError::Unsupported(format!(
            "expression `{expr}` is not available in the bootstrap frontend"
        )));
    };
    let literal = match &value.value {
        Value::Number(number, _) => translate_numeric_literal(number)?,
        Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
            ast::Literal::String(quote_string(value))
        }
        Value::Boolean(true) => ast::Literal::True,
        Value::Boolean(false) => ast::Literal::False,
        Value::Null => ast::Literal::Null,
        other => {
            return Err(MysqlParserError::Unsupported(format!(
                "literal `{other}` is not available in the bootstrap frontend"
            )))
        }
    };
    Ok(ast::Expr::Literal(literal))
}

fn translate_numeric_literal(number: &str) -> Result<ast::Literal, MysqlParserError> {
    if number.parse::<i64>().is_err() {
        return Err(MysqlParserError::Unsupported(format!(
            "numeric literal `{number}` requires exact MySQL decimal or unsigned semantics"
        )));
    }
    Ok(ast::Literal::Numeric(number.to_string()))
}

fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rejects_unsupported_from_clause() {
        let error = parse_and_translate("SELECT greeting FROM messages")
            .expect_err("FROM must not be silently discarded");
        assert!(error.to_string().contains("literal-only SELECT"));
    }

    #[test]
    fn rejects_multiple_statements() {
        let error = parse_one("SELECT 1; SELECT 2")
            .expect_err("multiple statements need consumed-offset support");
        assert!(matches!(error, MysqlParserError::MultipleStatements(2)));
    }
}

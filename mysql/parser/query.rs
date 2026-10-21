use crate::names::quote_string;
use crate::MysqlParserError;
use sqlparser::ast::{Expr, GroupByExpr, Query, Select, SelectFlavor, SelectItem, SetExpr, Value};
use std::num::NonZeroU32;
use turso_parser::ast;

#[derive(Default)]
pub(crate) struct TranslationContext {
    parameter_count: u32,
}

impl TranslationContext {
    fn next_parameter(&mut self) -> Result<ast::Variable, MysqlParserError> {
        self.parameter_count = self.parameter_count.checked_add(1).ok_or_else(|| {
            MysqlParserError::Unsupported(
                "too many anonymous parameters in SELECT projection".to_string(),
            )
        })?;
        let index = NonZeroU32::new(self.parameter_count).ok_or_else(|| {
            MysqlParserError::Unsupported(
                "anonymous parameter indices must be greater than zero".to_string(),
            )
        })?;
        Ok(ast::Variable::indexed(index))
    }
}

pub(crate) fn translate_query(
    query: &Query,
    context: &mut TranslationContext,
) -> Result<ast::Stmt, MysqlParserError> {
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
    translate_select(select, context)
}

fn translate_select(
    select: &Select,
    context: &mut TranslationContext,
) -> Result<ast::Stmt, MysqlParserError> {
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
            "only literal and anonymous-parameter SELECT projections are available in the bootstrap frontend"
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
        .map(|item| translate_projection(item, context))
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

fn translate_projection(
    item: &SelectItem,
    context: &mut TranslationContext,
) -> Result<ast::ResultColumn, MysqlParserError> {
    match item {
        SelectItem::UnnamedExpr(expr) => {
            let alias = is_anonymous_placeholder(expr)
                .then(|| ast::As::ImplicitColumnName(ast::Name::exact("?".to_string())));
            Ok(ast::ResultColumn::Expr(
                Box::new(translate_expr(expr, context)?),
                alias,
            ))
        }
        SelectItem::ExprWithAlias { expr, alias } => Ok(ast::ResultColumn::Expr(
            Box::new(translate_expr(expr, context)?),
            Some(ast::As::As(ast::Name::exact(alias.value.clone()))),
        )),
        _ => Err(MysqlParserError::Unsupported(
            "wildcards and multiple aliases are not available in the bootstrap frontend"
                .to_string(),
        )),
    }
}

fn translate_expr(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let Expr::Value(value) = expr else {
        return Err(MysqlParserError::Unsupported(format!(
            "expression `{expr}` is not available in the bootstrap frontend"
        )));
    };
    let literal = match &value.value {
        Value::Placeholder(placeholder) if placeholder == "?" => {
            return Ok(ast::Expr::Variable(context.next_parameter()?));
        }
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
            )));
        }
    };
    Ok(ast::Expr::Literal(literal))
}

fn is_anonymous_placeholder(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Value(value) if matches!(&value.value, Value::Placeholder(placeholder) if placeholder == "?")
    )
}

fn translate_numeric_literal(number: &str) -> Result<ast::Literal, MysqlParserError> {
    if number.parse::<i64>().is_err() {
        return Err(MysqlParserError::Unsupported(format!(
            "numeric literal `{number}` requires exact MySQL decimal or unsigned semantics"
        )));
    }
    Ok(ast::Literal::Numeric(number.to_string()))
}

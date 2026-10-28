use crate::expr::{failing_json_cast, translate_condition, translate_expr, translate_window};
use crate::invalid_json::invalid_json_text_evaluated;
use crate::names::translate_identifier;
use crate::MysqlParserError;
use sqlparser::ast::{
    Cte, Distinct, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    Join, JoinConstraint, JoinOperator, LimitClause, NamedWindowDefinition, NamedWindowExpr,
    ObjectName, ObjectNamePart, Offset, OffsetRows, OptimizerHint, OrderBy, OrderByExpr,
    OrderByKind, Query, Select, SelectFlavor, SelectItem, SelectItemQualifiedWildcardKind,
    SelectModifiers, SetExpr, SetOperator, SetQuantifier, TableAlias, TableFactor, TableWithJoins,
    Value, Values, WildcardAdditionalOptions, With,
};
use std::num::NonZeroU32;
use turso_parser::{ast, MAX_EXPR_DEPTH};

#[derive(Default)]
pub(crate) struct TranslationContext {
    parameter_count: u32,
    expression_depth: usize,
    select_list_runs_once: bool,
    tables_read: Vec<ast::Name>,
}

impl TranslationContext {
    pub(crate) fn reads_table(&self, table: &ast::Name) -> bool {
        self.tables_read
            .iter()
            .any(|read| read.as_str().eq_ignore_ascii_case(table.as_str()))
    }

    pub(crate) fn enter_expression(&mut self) -> Result<(), MysqlParserError> {
        if self.expression_depth >= MAX_EXPR_DEPTH {
            return Err(MysqlParserError::Unsupported(format!(
                "expressions nested deeper than {MAX_EXPR_DEPTH} levels"
            )));
        }
        self.expression_depth += 1;
        Ok(())
    }

    pub(crate) fn leave_expression(&mut self) {
        self.expression_depth -= 1;
    }

    pub(crate) fn next_parameter(&mut self) -> Result<ast::Variable, MysqlParserError> {
        self.parameter_count = self.parameter_count.checked_add(1).ok_or_else(|| {
            MysqlParserError::Unsupported("too many anonymous parameters".to_string())
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
    context.select_list_runs_once = runs_select_list_once(query);
    Ok(ast::Stmt::Select(translate_subquery(query, context)?))
}

fn runs_select_list_once(query: &Query) -> bool {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    let no_table = match select.from.as_slice() {
        [] => true,
        [only] => only.joins.is_empty() && is_dual(&only.relation),
        _ => false,
    };
    is_plain_body(query)
        && no_table
        && select.selection.is_none()
        && select.having.is_none()
        && select.named_window.is_empty()
        && matches!(&select.group_by, GroupByExpr::Expressions(terms, modifiers) if terms.is_empty() && modifiers.is_empty())
}

pub(crate) fn translate_subquery(
    query: &Query,
    context: &mut TranslationContext,
) -> Result<ast::Select, MysqlParserError> {
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
    if fetch.is_some()
        || !locks.is_empty()
        || for_clause.is_some()
        || settings.is_some()
        || format_clause.is_some()
        || !pipe_operators.is_empty()
    {
        return Err(unsupported(format!("query clause in `{query}`")));
    }
    let with = with
        .as_ref()
        .map(|with| translate_with(with, context))
        .transpose()?;
    let body = translate_body(body, context)?;
    let order_by = order_by
        .as_ref()
        .map(|order_by| translate_order_by(order_by, context))
        .transpose()?
        .unwrap_or_default();
    let limit = limit_clause
        .as_ref()
        .map(|limit| translate_limit(limit, context))
        .transpose()?;
    Ok(ast::Select {
        with,
        body,
        order_by,
        limit,
    })
}

fn translate_with(
    with: &With,
    context: &mut TranslationContext,
) -> Result<ast::With, MysqlParserError> {
    if with.recursive {
        return Err(unsupported(
            "WITH RECURSIVE, whose cte_max_recursion_depth limit Turso does not enforce,"
                .to_string(),
        ));
    }
    let ctes = with
        .cte_tables
        .iter()
        .map(|cte| translate_cte(cte, context))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::With {
        recursive: false,
        ctes,
    })
}

fn translate_cte(
    cte: &Cte,
    context: &mut TranslationContext,
) -> Result<ast::CommonTableExpr, MysqlParserError> {
    let Cte {
        alias,
        query,
        from: None,
        materialized: None,
        closing_paren_token: _,
    } = cte
    else {
        return Err(unsupported(format!("common table expression `{cte}`")));
    };
    let TableAlias {
        explicit: _,
        name,
        columns,
        at: None,
    } = alias
    else {
        return Err(unsupported(format!(
            "common table expression name `{alias}`"
        )));
    };
    let columns = columns
        .iter()
        .map(|column| {
            if column.data_type.is_some() {
                return Err(unsupported(format!("typed column alias `{}`", column.name)));
            }
            Ok(ast::IndexedColumn {
                col_name: translate_identifier(&column.name)?,
                collation_name: None,
                order: None,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::CommonTableExpr {
        tbl_name: translate_identifier(name)?,
        columns,
        materialized: ast::Materialized::Any,
        select: translate_subquery(query, context)?,
    })
}

fn translate_body(
    body: &SetExpr,
    context: &mut TranslationContext,
) -> Result<ast::SelectBody, MysqlParserError> {
    match body {
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            let mut body = translate_body(left, context)?;
            let operator = compound_operator(op, set_quantifier)?;
            let select = translate_compound_member(right, context)?;
            body.compounds
                .push(ast::CompoundSelect { operator, select });
            Ok(body)
        }
        other => Ok(ast::SelectBody {
            select: translate_compound_member(other, context)?,
            compounds: Vec::new(),
        }),
    }
}

fn translate_compound_member(
    member: &SetExpr,
    context: &mut TranslationContext,
) -> Result<ast::OneSelect, MysqlParserError> {
    match member {
        SetExpr::Select(select) => translate_select(select, context),
        SetExpr::Values(values) => translate_values(values, context),
        SetExpr::Query(query) if is_plain_body(query) => {
            let body = translate_body(&query.body, context)?;
            if body.compounds.is_empty() {
                return Ok(body.select);
            }
            Ok(select_all_from(plain_select(body)))
        }
        SetExpr::Query(query) => Ok(select_all_from(translate_subquery(query, context)?)),
        SetExpr::SetOperation { .. } => Ok(select_all_from(plain_select(translate_body(
            member, context,
        )?))),
        other => Err(unsupported(format!("query body `{other}`"))),
    }
}

fn is_plain_body(query: &Query) -> bool {
    query.with.is_none()
        && query.order_by.is_none()
        && query.limit_clause.is_none()
        && query.fetch.is_none()
        && query.locks.is_empty()
        && query.for_clause.is_none()
        && query.settings.is_none()
        && query.format_clause.is_none()
        && query.pipe_operators.is_empty()
}

fn plain_select(body: ast::SelectBody) -> ast::Select {
    ast::Select {
        with: None,
        body,
        order_by: Vec::new(),
        limit: None,
    }
}

fn select_all_from(select: ast::Select) -> ast::OneSelect {
    ast::OneSelect::Select {
        distinctness: None,
        columns: vec![ast::ResultColumn::Star],
        from: Some(ast::FromClause {
            select: Box::new(ast::SelectTable::Select {
                select,
                alias: None,
                lateral: false,
            }),
            joins: Vec::new(),
        }),
        where_clause: None,
        group_by: None,
        window_clause: Vec::new(),
    }
}

fn compound_operator(
    op: &SetOperator,
    quantifier: &SetQuantifier,
) -> Result<ast::CompoundOperator, MysqlParserError> {
    match (op, quantifier) {
        (SetOperator::Union, SetQuantifier::All) => Ok(ast::CompoundOperator::UnionAll),
        (SetOperator::Union, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(ast::CompoundOperator::Union)
        }
        (SetOperator::Except, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(ast::CompoundOperator::Except)
        }
        (SetOperator::Intersect, SetQuantifier::None | SetQuantifier::Distinct) => {
            Ok(ast::CompoundOperator::Intersect)
        }
        _ => Err(unsupported(format!("set operation `{op} {quantifier}`"))),
    }
}

fn translate_values(
    values: &Values,
    context: &mut TranslationContext,
) -> Result<ast::OneSelect, MysqlParserError> {
    if !values.explicit_row || values.rows.is_empty() {
        return Err(unsupported("VALUES without ROW".to_string()));
    }
    if values
        .rows
        .iter()
        .any(|row| row.len() != values.rows[0].len())
    {
        return Err(unsupported("VALUES rows of different lengths".to_string()));
    }
    let rows = values
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| translate_expr(value, context).map(Box::new))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::OneSelect::Values(rows))
}

fn translate_select(
    select: &Select,
    context: &mut TranslationContext,
) -> Result<ast::OneSelect, MysqlParserError> {
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
    if !optimizer_hints.iter().all(only_changes_the_plan)
        || select_modifiers
            .as_ref()
            .is_some_and(SelectModifiers::is_any_set)
        || top.is_some()
        || *top_before_distinct
        || exclude.is_some()
        || into.is_some()
        || !lateral_views.is_empty()
        || prewhere.is_some()
        || !connect_by.is_empty()
        || !cluster_by.is_empty()
        || !distribute_by.is_empty()
        || !sort_by.is_empty()
        || qualify.is_some()
        || *window_before_qualify
        || value_table_mode.is_some()
        || *flavor != SelectFlavor::Standard
    {
        return Err(unsupported(format!("SELECT clause in `{select}`")));
    }
    if may_break_only_full_group_by(select) {
        return Err(unsupported(format!(
            "a grouped SELECT that may use a column outside GROUP BY, which ONLY_FULL_GROUP_BY rejects in MySQL and Turso allows, `{select}`"
        )));
    }
    let distinctness = match distinct {
        None => None,
        Some(Distinct::All) => Some(ast::Distinctness::All),
        Some(Distinct::Distinct) => Some(ast::Distinctness::Distinct),
        Some(Distinct::On(_)) => return Err(unsupported("DISTINCT ON".to_string())),
    };
    if projection.is_empty() {
        return Err(unsupported("SELECT without projection".to_string()));
    }
    let runs_once = std::mem::take(&mut context.select_list_runs_once);
    let columns = projection
        .iter()
        .map(|item| translate_projection(item, runs_once, context))
        .collect::<Result<Vec<_>, _>>()?;
    let from = translate_from(from, context)?;
    let where_clause = selection
        .as_ref()
        .map(|condition| translate_condition(condition, context).map(Box::new))
        .transpose()?;
    let group_by = translate_group_by(group_by, having.as_ref(), context)?;
    let window_clause = named_window
        .iter()
        .map(|window| translate_named_window(window, context))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::OneSelect::Select {
        distinctness,
        columns,
        from,
        where_clause,
        group_by,
        window_clause,
    })
}

fn may_break_only_full_group_by(select: &Select) -> bool {
    let GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return true;
    };
    let projections = select
        .projection
        .iter()
        .map(projected_expr)
        .collect::<Vec<_>>();
    let grouped = !group_by.is_empty()
        || projections
            .iter()
            .flatten()
            .any(|expr| contains_aggregate(expr))
        || select.having.as_ref().is_some_and(contains_aggregate);
    if !grouped {
        return false;
    }
    let aliases = select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut grouping = group_by.iter().collect::<Vec<_>>();
    for term in group_by {
        grouping.extend(selected_by_group_term(term, select));
    }
    let groups = Grouping {
        exprs: grouping,
        aliases: &[],
    };
    let having_groups = Grouping {
        aliases: &aliases,
        ..groups.clone()
    };
    projections.iter().any(|expr| match expr {
        Some(expr) => uses_ungrouped_column(expr, &groups),
        None => true,
    }) || select
        .having
        .as_ref()
        .is_some_and(|having| uses_ungrouped_column(having, &having_groups))
}

fn projected_expr(item: &SelectItem) -> Option<&Expr> {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => Some(expr),
        _ => None,
    }
}

fn selected_by_group_term<'a>(term: &Expr, select: &'a Select) -> Option<&'a Expr> {
    match term {
        Expr::Value(value) => match &value.value {
            Value::Number(ordinal, false) => {
                let position = ordinal.parse::<usize>().ok()?.checked_sub(1)?;
                projected_expr(select.projection.get(position)?)
            }
            _ => None,
        },
        Expr::Identifier(name) => select.projection.iter().find_map(|item| match item {
            SelectItem::ExprWithAlias { expr, alias }
                if alias.value.eq_ignore_ascii_case(&name.value) =>
            {
                Some(expr)
            }
            _ => None,
        }),
        _ => None,
    }
}

#[derive(Clone)]
struct Grouping<'a> {
    exprs: Vec<&'a Expr>,
    aliases: &'a [&'a str],
}

impl Grouping<'_> {
    fn contains(&self, expr: &Expr) -> bool {
        self.exprs
            .iter()
            .any(|grouped| without_parentheses(grouped) == without_parentheses(expr))
    }

    fn contains_column(&self, name: &str) -> bool {
        self.aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(name))
            || self.exprs.iter().any(|grouped| {
                column_name_of(without_parentheses(grouped))
                    .is_some_and(|column| column.eq_ignore_ascii_case(name))
            })
    }
}

fn without_parentheses(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => without_parentheses(inner),
        other => other,
    }
}

fn column_name_of(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Identifier(identifier) => Some(&identifier.value),
        Expr::CompoundIdentifier(parts) => parts.last().map(|part| part.value.as_str()),
        _ => None,
    }
}

fn uses_ungrouped_column(expr: &Expr, groups: &Grouping) -> bool {
    if groups.contains(expr) {
        return false;
    }
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
            !column_name_of(expr).is_some_and(|name| groups.contains_column(name))
        }
        Expr::Value(_) | Expr::TypedString(_) | Expr::Prefixed { .. } | Expr::Interval(_) => false,
        Expr::Subquery(_) | Expr::Exists { .. } => false,
        Expr::Function(function) if is_aggregate(function) => false,
        Expr::Function(function) => function_arguments(function)
            .is_none_or(|args| args.iter().any(|arg| uses_ungrouped_column(arg, groups))),
        Expr::InSubquery { expr, .. } => uses_ungrouped_column(expr, groups),
        other => match child_expressions(other) {
            Some(children) => children
                .iter()
                .any(|child| uses_ungrouped_column(child, groups)),
            None => true,
        },
    }
}

fn contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) if is_aggregate(function) => true,
        Expr::Function(function) => function_arguments(function)
            .is_some_and(|args| args.iter().any(|arg| contains_aggregate(arg))),
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => false,
        other => child_expressions(other)
            .is_some_and(|children| children.iter().any(|child| contains_aggregate(child))),
    }
}

fn is_aggregate(function: &Function) -> bool {
    function.over.is_none()
        && matches!(
            function.name.to_string().to_ascii_uppercase().as_str(),
            "COUNT"
                | "SUM"
                | "AVG"
                | "MIN"
                | "MAX"
                | "GROUP_CONCAT"
                | "BIT_AND"
                | "BIT_OR"
                | "BIT_XOR"
                | "STD"
                | "STDDEV"
                | "STDDEV_POP"
                | "STDDEV_SAMP"
                | "VARIANCE"
                | "VAR_POP"
                | "VAR_SAMP"
                | "JSON_ARRAYAGG"
                | "JSON_OBJECTAGG"
                | "ANY_VALUE"
        )
}

fn function_arguments(function: &Function) -> Option<Vec<&Expr>> {
    match &function.args {
        FunctionArguments::None => Some(Vec::new()),
        FunctionArguments::List(list) => list
            .args
            .iter()
            .map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
                FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                _ => None,
            })
            .collect(),
        FunctionArguments::Subquery(_) => None,
    }
}

pub(crate) fn child_expressions(expr: &Expr) -> Option<Vec<&Expr>> {
    Some(match expr {
        Expr::Value(_) | Expr::TypedString(_) | Expr::Prefixed { .. } => Vec::new(),
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::Convert { expr: inner, .. }
        | Expr::Collate { expr: inner, .. }
        | Expr::Extract { expr: inner, .. }
        | Expr::Ceil { expr: inner, .. }
        | Expr::Floor { expr: inner, .. } => vec![inner],
        Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. } => vec![left, right],
        Expr::Like { expr, pattern, .. } | Expr::RLike { expr, pattern, .. } => {
            vec![expr, pattern]
        }
        Expr::Between {
            expr, low, high, ..
        } => vec![expr, low, high],
        Expr::InList { expr, list, .. } => std::iter::once(expr.as_ref()).chain(list).collect(),
        Expr::Tuple(items) => items.iter().collect(),
        Expr::Position { expr, r#in } => vec![expr, r#in],
        Expr::MemberOf(member_of) => vec![&member_of.value, &member_of.array],
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => std::iter::once(expr.as_ref())
            .chain(substring_from.as_deref())
            .chain(substring_for.as_deref())
            .collect(),
        Expr::Trim {
            expr, trim_what, ..
        } => std::iter::once(expr.as_ref())
            .chain(trim_what.as_deref())
            .collect(),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => operand
            .as_deref()
            .into_iter()
            .chain(
                conditions
                    .iter()
                    .flat_map(|case| [&case.condition, &case.result]),
            )
            .chain(else_result.as_deref())
            .collect(),
        _ => return None,
    })
}

pub(crate) fn only_changes_the_plan(hint: &OptimizerHint) -> bool {
    let text = hint.text.to_ascii_uppercase();
    !text.contains("SET_VAR") && !text.contains("MAX_EXECUTION_TIME")
}

fn translate_projection(
    item: &SelectItem,
    runs_once: bool,
    context: &mut TranslationContext,
) -> Result<ast::ResultColumn, MysqlParserError> {
    match item {
        SelectItem::UnnamedExpr(expr) => {
            let name = implicit_column_name(expr);
            Ok(ast::ResultColumn::Expr(
                Box::new(translate_selected(expr, runs_once, context)?),
                Some(ast::As::ImplicitColumnName(ast::Name::exact(name))),
            ))
        }
        SelectItem::ExprWithAlias { expr, alias } => {
            let name = match alias.quote_style {
                Some('\'' | '"') => ast::Name::exact(alias.value.clone()),
                _ => translate_identifier(alias)?,
            };
            Ok(ast::ResultColumn::Expr(
                Box::new(translate_selected(expr, runs_once, context)?),
                Some(ast::As::As(name)),
            ))
        }
        SelectItem::Wildcard(options) if is_plain_wildcard(options) => Ok(ast::ResultColumn::Star),
        SelectItem::QualifiedWildcard(
            SelectItemQualifiedWildcardKind::ObjectName(name),
            options,
        ) if is_plain_wildcard(options) => match name.0.as_slice() {
            [ObjectNamePart::Identifier(table)] => {
                Ok(ast::ResultColumn::TableStar(translate_identifier(table)?))
            }
            _ => Err(unsupported(format!("wildcard `{name}.*`"))),
        },
        other => Err(unsupported(format!("projection `{other}`"))),
    }
}

fn translate_selected(
    expr: &Expr,
    runs_once: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let failing_text = runs_once
        .then(|| invalid_json_text_evaluated(expr))
        .flatten();
    match failing_text {
        Some(text) => Ok(failing_json_cast(text)),
        None => translate_expr(expr, context),
    }
}

fn implicit_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text)
            | Value::DoubleQuotedString(text)
            | Value::NationalStringLiteral(text) => text.clone(),
            _ => expr.to_string(),
        },
        _ => expr.to_string(),
    }
}

fn is_plain_wildcard(options: &WildcardAdditionalOptions) -> bool {
    let WildcardAdditionalOptions {
        wildcard_token: _,
        opt_ilike,
        opt_exclude,
        opt_except,
        opt_replace,
        opt_rename,
        opt_alias,
    } = options;
    opt_ilike.is_none()
        && opt_exclude.is_none()
        && opt_except.is_none()
        && opt_replace.is_none()
        && opt_rename.is_none()
        && opt_alias.is_none()
}

fn translate_from(
    from: &[TableWithJoins],
    context: &mut TranslationContext,
) -> Result<Option<ast::FromClause>, MysqlParserError> {
    let Some((first, rest)) = from.split_first() else {
        return Ok(None);
    };
    if rest.is_empty() && first.joins.is_empty() && is_dual(&first.relation) {
        return Ok(None);
    }
    let select = translate_table_factor(&first.relation, context)?;
    let mut joins = translate_joins(&first.joins, context)?;
    for table in rest {
        joins.push(ast::JoinedSelectTable {
            operator: ast::JoinOperator::Comma,
            table: Box::new(translate_table_factor(&table.relation, context)?),
            constraint: None,
        });
        joins.extend(translate_joins(&table.joins, context)?);
    }
    Ok(Some(ast::FromClause {
        select: Box::new(select),
        joins,
    }))
}

fn is_dual(relation: &TableFactor) -> bool {
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
    } = relation
    else {
        return false;
    };
    with_hints.is_empty()
        && partitions.is_empty()
        && index_hints.is_empty()
        && matches!(
            name.0.as_slice(),
            [ObjectNamePart::Identifier(identifier)]
                if identifier.quote_style.is_none()
                    && identifier.value.eq_ignore_ascii_case("dual")
        )
}

fn translate_joins(
    joins: &[Join],
    context: &mut TranslationContext,
) -> Result<Vec<ast::JoinedSelectTable>, MysqlParserError> {
    joins
        .iter()
        .map(|join| translate_join(join, context))
        .collect()
}

fn translate_join(
    join: &Join,
    context: &mut TranslationContext,
) -> Result<ast::JoinedSelectTable, MysqlParserError> {
    if join.global {
        return Err(unsupported(format!("join `{join}`")));
    }
    let (join_type, constraint) = match &join.join_operator {
        JoinOperator::Join(constraint) => (None, constraint),
        JoinOperator::Inner(constraint) => (Some(ast::JoinType::INNER), constraint),
        JoinOperator::Left(constraint) => (Some(ast::JoinType::LEFT), constraint),
        JoinOperator::LeftOuter(constraint) => {
            (Some(ast::JoinType::LEFT | ast::JoinType::OUTER), constraint)
        }
        JoinOperator::Right(constraint) => (Some(ast::JoinType::RIGHT), constraint),
        JoinOperator::RightOuter(constraint) => (
            Some(ast::JoinType::RIGHT | ast::JoinType::OUTER),
            constraint,
        ),
        JoinOperator::CrossJoin(constraint) => (
            Some(ast::JoinType::INNER | ast::JoinType::CROSS),
            constraint,
        ),
        _ => return Err(unsupported(format!("join `{join}`"))),
    };
    let table = translate_table_factor(&join.relation, context)?;
    let (join_type, constraint) = match constraint {
        JoinConstraint::None => (join_type, None),
        JoinConstraint::Natural => (
            Some(join_type.unwrap_or(ast::JoinType::empty()) | ast::JoinType::NATURAL),
            None,
        ),
        JoinConstraint::On(condition) => (
            join_type,
            Some(ast::JoinConstraint::On(Box::new(translate_condition(
                condition, context,
            )?))),
        ),
        JoinConstraint::Using(columns) => (
            join_type,
            Some(ast::JoinConstraint::Using(
                columns
                    .iter()
                    .map(single_identifier)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
        ),
    };
    Ok(ast::JoinedSelectTable {
        operator: ast::JoinOperator::TypedJoin(join_type),
        table: Box::new(table),
        constraint,
    })
}

fn single_identifier(name: &ObjectName) -> Result<ast::Name, MysqlParserError> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(identifier)] => translate_identifier(identifier),
        _ => Err(unsupported(format!("qualified name `{name}`"))),
    }
}

fn translate_table_factor(
    factor: &TableFactor,
    context: &mut TranslationContext,
) -> Result<ast::SelectTable, MysqlParserError> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args: None,
            with_hints,
            version: None,
            with_ordinality: false,
            partitions,
            json_path: None,
            sample: None,
            index_hints,
        } if with_hints.is_empty() && partitions.is_empty() && index_hints.is_empty() => {
            let name = translate_table_name(name)?;
            context.tables_read.push(name.name.clone());
            let alias = alias.as_ref().map(translate_table_alias).transpose()?;
            Ok(ast::SelectTable::Table(name, alias, None))
        }
        TableFactor::Derived {
            lateral,
            subquery,
            alias,
            sample: None,
        } => {
            let select = translate_subquery(subquery, context)?;
            let alias = alias.as_ref().map(translate_table_alias).transpose()?;
            Ok(ast::SelectTable::Select {
                select,
                alias,
                lateral: *lateral,
            })
        }
        TableFactor::NestedJoin {
            table_with_joins,
            alias,
        } => {
            let from = translate_from(std::slice::from_ref(table_with_joins), context)?
                .ok_or_else(|| unsupported(format!("table `{factor}`")))?;
            let alias = alias.as_ref().map(translate_table_alias).transpose()?;
            Ok(ast::SelectTable::Sub(from, alias))
        }
        other => Err(unsupported(format!("table `{other}`"))),
    }
}

pub(crate) fn translate_table_name(
    name: &ObjectName,
) -> Result<ast::QualifiedName, MysqlParserError> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(table)] => {
            Ok(ast::QualifiedName::single(translate_identifier(table)?))
        }
        [ObjectNamePart::Identifier(schema), ObjectNamePart::Identifier(table)] => {
            Ok(ast::QualifiedName::fullname(
                translate_identifier(schema)?,
                translate_identifier(table)?,
            ))
        }
        _ => Err(unsupported(format!("table name `{name}`"))),
    }
}

fn translate_table_alias(alias: &TableAlias) -> Result<ast::As, MysqlParserError> {
    let TableAlias {
        explicit,
        name,
        columns,
        at: None,
    } = alias
    else {
        return Err(unsupported(format!("table alias `{alias}`")));
    };
    if !columns.is_empty() {
        return Err(unsupported(format!("table alias column list `{alias}`")));
    }
    let name = translate_identifier(name)?;
    Ok(if *explicit {
        ast::As::As(name)
    } else {
        ast::As::Elided(name)
    })
}

fn translate_group_by(
    group_by: &GroupByExpr,
    having: Option<&Expr>,
    context: &mut TranslationContext,
) -> Result<Option<ast::GroupBy>, MysqlParserError> {
    let GroupByExpr::Expressions(expressions, modifiers) = group_by else {
        return Err(unsupported(format!("GROUP BY `{group_by}`")));
    };
    if !modifiers.is_empty() {
        return Err(unsupported(format!("GROUP BY `{group_by}`")));
    }
    let exprs = expressions
        .iter()
        .map(|expr| translate_expr(expr, context).map(Box::new))
        .collect::<Result<Vec<_>, _>>()?;
    let having = having
        .map(|condition| translate_condition(condition, context).map(Box::new))
        .transpose()?;
    if exprs.is_empty() && having.is_none() {
        return Ok(None);
    }
    Ok(Some(ast::GroupBy { exprs, having }))
}

fn translate_named_window(
    window: &NamedWindowDefinition,
    context: &mut TranslationContext,
) -> Result<ast::WindowDef, MysqlParserError> {
    let NamedWindowDefinition(name, definition) = window;
    let NamedWindowExpr::WindowSpec(spec) = definition else {
        return Err(unsupported(format!("window `{window}`")));
    };
    Ok(ast::WindowDef {
        name: translate_identifier(name)?,
        window: translate_window(spec, context)?,
    })
}

fn translate_order_by(
    order_by: &OrderBy,
    context: &mut TranslationContext,
) -> Result<Vec<ast::SortedColumn>, MysqlParserError> {
    let OrderBy {
        kind: OrderByKind::Expressions(terms),
        interpolate: None,
    } = order_by
    else {
        return Err(unsupported(format!("ORDER BY `{order_by}`")));
    };
    terms
        .iter()
        .map(|term| translate_order_by_expr(term, context))
        .collect()
}

pub(crate) fn translate_order_by_expr(
    term: &OrderByExpr,
    context: &mut TranslationContext,
) -> Result<ast::SortedColumn, MysqlParserError> {
    if term.with_fill.is_some() || term.options.nulls_first.is_some() {
        return Err(unsupported(format!("ORDER BY term `{term}`")));
    }
    let order = term.options.asc.map(|ascending| {
        if ascending {
            ast::SortOrder::Asc
        } else {
            ast::SortOrder::Desc
        }
    });
    Ok(ast::SortedColumn {
        expr: Box::new(translate_expr(&term.expr, context)?),
        order,
        nulls: None,
    })
}

fn translate_limit(
    limit: &LimitClause,
    context: &mut TranslationContext,
) -> Result<ast::Limit, MysqlParserError> {
    match limit {
        LimitClause::LimitOffset {
            limit: Some(count),
            offset,
            limit_by,
        } if limit_by.is_empty() => {
            let expr = Box::new(translate_expr(count, context)?);
            let offset = match offset {
                None => None,
                Some(Offset {
                    value,
                    rows: OffsetRows::None,
                }) => Some(Box::new(translate_expr(value, context)?)),
                Some(other) => return Err(unsupported(format!("OFFSET `{other}`"))),
            };
            Ok(ast::Limit { expr, offset })
        }
        LimitClause::OffsetCommaLimit { offset, limit } => {
            let offset = Box::new(translate_expr(offset, context)?);
            let expr = Box::new(translate_expr(limit, context)?);
            Ok(ast::Limit {
                expr,
                offset: Some(offset),
            })
        }
        other => Err(unsupported(format!("LIMIT `{other}`"))),
    }
}

fn unsupported(what: String) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

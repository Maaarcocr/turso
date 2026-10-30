//! MySQL's error 1060 for a derived table, a common table expression or a
//! view that names a column twice: in its column list, or, without one, in
//! the select list of its query (the first `SELECT` of a set operation),
//! where `*` names the columns of the tables of the `FROM` in order. Names
//! compare without case. An unnamed expression is named by its text as
//! written, which lowering does not keep, so one whose text may repeat
//! another name is refused. Names that lowering cannot know (a `*` over a
//! view, a common table expression, a `USING` or `NATURAL` join or a table
//! function) are not checked.

use crate::{Environment, MysqlParserError};
use sqlparser::ast::{Expr, TableAliasColumnDef};
use sqlparser::ast::{
    Ident, JoinConstraint, JoinOperator, ObjectNamePart, Query, Select, SelectItem, SetExpr,
    TableFactor, TableWithJoins,
};

/// Fails with 1060 when the column list `columns` repeats a name.
pub(crate) fn check_column_list(columns: &[TableAliasColumnDef]) -> Result<(), MysqlParserError> {
    check_list(columns.iter().map(|column| &column.name))
}

/// Fails with 1060 when the view column list `columns` repeats a name.
pub(crate) fn check_names(columns: &[&Ident]) -> Result<(), MysqlParserError> {
    check_list(columns.iter().copied())
}

fn check_list<'a>(columns: impl Iterator<Item = &'a Ident>) -> Result<(), MysqlParserError> {
    let mut seen: Vec<&str> = Vec::new();
    for column in columns {
        if seen
            .iter()
            .any(|name| name.eq_ignore_ascii_case(&column.value))
        {
            return Err(duplicate(&column.value));
        }
        seen.push(&column.value);
    }
    Ok(())
}

/// Fails with 1060 when the columns of `query`, the query of a derived
/// table, common table expression or view without a column list, repeat a
/// name. `common_tables` are the names of the common table expressions in
/// scope, which may hide a table of the session.
pub(crate) fn check_query(
    query: &Query,
    environment: Option<&dyn Environment>,
    common_tables: &[String],
) -> Result<(), MysqlParserError> {
    let Some(names) = query_names(query, environment, common_tables)? else {
        return Ok(());
    };
    for (index, name) in names.iter().enumerate() {
        for earlier in &names[..index] {
            if !earlier.text().eq_ignore_ascii_case(name.text()) {
                continue;
            }
            return match (earlier, name) {
                (Name::Known(_), Name::Known(text)) => Err(duplicate(text)),
                _ => Err(MysqlParserError::Unsupported(format!(
                    "a derived table, common table expression or view whose unnamed column `{}` may repeat a name, which MySQL rejects (1060) depending on its text, is not lowered yet",
                    name.text()
                ))),
            };
        }
    }
    Ok(())
}

fn duplicate(name: &str) -> MysqlParserError {
    MysqlParserError::Rejected(format!(
        "ERROR 1060 (42S21): Duplicate column name '{name}'"
    ))
}

enum Name {
    /// A column name or alias.
    Known(String),
    /// The text of an unnamed expression, as lowering prints it.
    Text(String),
}

impl Name {
    fn text(&self) -> &str {
        match self {
            Name::Known(text) | Name::Text(text) => text,
        }
    }
}

fn query_names(
    query: &Query,
    environment: Option<&dyn Environment>,
    common_tables: &[String],
) -> Result<Option<Vec<Name>>, MysqlParserError> {
    let mut body = query.body.as_ref();
    loop {
        match body {
            SetExpr::SetOperation { left, .. } => body = left,
            SetExpr::Query(inner) => return query_names(inner, environment, common_tables),
            SetExpr::Select(select) => return select_names(select, environment, common_tables),
            _ => return Ok(None),
        }
    }
}

fn select_names(
    select: &Select,
    environment: Option<&dyn Environment>,
    common_tables: &[String],
) -> Result<Option<Vec<Name>>, MysqlParserError> {
    let mut names = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::ExprWithAlias { alias, .. } => names.push(Name::Known(alias.value.clone())),
            SelectItem::UnnamedExpr(Expr::Identifier(column)) => {
                names.push(Name::Known(column.value.clone()));
            }
            SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => match parts.last() {
                Some(column) => names.push(Name::Known(column.value.clone())),
                None => return Ok(None),
            },
            SelectItem::UnnamedExpr(expr) => names.push(Name::Text(expr.to_string())),
            SelectItem::Wildcard(options) => {
                let plain = options.opt_ilike.is_none()
                    && options.opt_exclude.is_none()
                    && options.opt_except.is_none()
                    && options.opt_replace.is_none()
                    && options.opt_rename.is_none()
                    && options.opt_alias.is_none();
                if !plain {
                    return Ok(None);
                }
                let Some(tables) = from_columns(&select.from, environment, common_tables)? else {
                    return Ok(None);
                };
                for columns in tables {
                    names.extend(columns.into_iter().map(Name::Known));
                }
            }
            SelectItem::QualifiedWildcard(..) | SelectItem::ExprWithAliases { .. } => {
                return Ok(None)
            }
        }
    }
    Ok(Some(names))
}

/// The columns of each table of `from`, in order.
fn from_columns(
    from: &[TableWithJoins],
    environment: Option<&dyn Environment>,
    common_tables: &[String],
) -> Result<Option<Vec<Vec<String>>>, MysqlParserError> {
    let mut tables = Vec::new();
    for table in from {
        for join in &table.joins {
            let coalesces = match &join.join_operator {
                JoinOperator::Join(constraint)
                | JoinOperator::Inner(constraint)
                | JoinOperator::Left(constraint)
                | JoinOperator::LeftOuter(constraint)
                | JoinOperator::Right(constraint)
                | JoinOperator::RightOuter(constraint)
                | JoinOperator::CrossJoin(constraint)
                | JoinOperator::StraightJoin(constraint) => {
                    matches!(
                        constraint,
                        JoinConstraint::Using(_) | JoinConstraint::Natural
                    )
                }
                _ => true,
            };
            if coalesces {
                return Ok(None);
            }
        }
        for relation in
            std::iter::once(&table.relation).chain(table.joins.iter().map(|join| &join.relation))
        {
            let Some(columns) = factor_columns(relation, environment, common_tables)? else {
                return Ok(None);
            };
            tables.push(columns);
        }
    }
    Ok(Some(tables))
}

fn factor_columns(
    factor: &TableFactor,
    environment: Option<&dyn Environment>,
    common_tables: &[String],
) -> Result<Option<Vec<String>>, MysqlParserError> {
    match factor {
        TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } => {
            let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
                return Ok(None);
            };
            if common_tables
                .iter()
                .any(|common| common.eq_ignore_ascii_case(&table.value))
            {
                return Ok(None);
            }
            let Some(environment) = environment else {
                return Ok(None);
            };
            let Some(columns) = environment.columns_of(&table.value)? else {
                return Ok(None);
            };
            let columns = match alias {
                Some(alias) if !alias.columns.is_empty() => alias
                    .columns
                    .iter()
                    .map(|column| column.name.value.clone())
                    .collect(),
                _ => columns.into_iter().map(|facts| facts.name).collect(),
            };
            Ok(Some(columns))
        }
        TableFactor::Derived {
            subquery,
            alias: Some(alias),
            ..
        } => {
            if !alias.columns.is_empty() {
                return Ok(Some(
                    alias
                        .columns
                        .iter()
                        .map(|column| column.name.value.clone())
                        .collect(),
                ));
            }
            let Some(names) = query_names(subquery, environment, common_tables)? else {
                return Ok(None);
            };
            let columns = names
                .into_iter()
                .map(|name| match name {
                    Name::Known(text) => Some(text),
                    Name::Text(_) => None,
                })
                .collect::<Option<Vec<_>>>();
            Ok(columns)
        }
        _ => Ok(None),
    }
}

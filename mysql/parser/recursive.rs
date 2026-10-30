//! A recursive common table expression of `WITH RECURSIVE`: one anchor
//! `SELECT` `UNION ALL` one recursive `SELECT` that reads the expression
//! once. MySQL stops it with an error after `cte_max_recursion_depth` (1000)
//! iterations and gives its columns the types of the anchor's. The lowering
//! keeps the iteration of each row in a hidden column of an inner expression
//! `__mysql_recursive_<name>`, whose recursive rows compute it with
//! `__mysql_recursion_depth(depth)`, which refuses the 1000th iteration; the
//! expression `name` selects the other columns from it. A column whose anchor
//! is an integer literal (an `INT` in MySQL) has its recursive values checked
//! by `__mysql_recursive_integer(value)`, which refuses a value outside `INT`
//! or not an integer. Refused: another shape, a recursive `SELECT` with
//! `DISTINCT`, grouping, a window, a subquery or an outer join, and a column
//! whose anchor is not an integer literal unless its recursive value is the
//! column itself, and an expression in a subquery of an expression or in a
//! `LATERAL` table, which may read an outer row.

use crate::dialect::tokens;
use crate::expr::function_call;
use crate::names::translate_identifier;
use crate::query::{translate_cte, TranslationContext};
use crate::MysqlParserError;
use sqlparser::ast::{
    Cte, Expr, Ident, JoinOperator, ObjectName, ObjectNamePart, Query, Select, SelectItem, SetExpr,
    SetOperator, SetQuantifier, TableAlias, TableAliasColumnDef, TableFactor, UnaryOperator, Value,
    ValueWithSpan,
};
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Span, Token};
use turso_parser::ast;

const DEPTH: &str = "__mysql_depth";

/// Whether the query of `cte` names it, which makes it recursive.
pub(crate) fn reads_itself(cte: &Cte) -> Result<bool, MysqlParserError> {
    mentions(&cte.query.to_string(), &cte.alias.name.value)
}

fn mentions(text: &str, name: &str) -> Result<bool, MysqlParserError> {
    Ok(tokens(text)?.iter().any(
        |token| matches!(&token.token, Token::Word(word) if word.value.eq_ignore_ascii_case(name)),
    ))
}

pub(crate) fn translate_recursive_cte(
    cte: &Cte,
    context: &mut TranslationContext,
) -> Result<[ast::CommonTableExpr; 2], MysqlParserError> {
    let name = &cte.alias.name;
    let refused = |why: &str| {
        MysqlParserError::Unsupported(format!(
            "WITH RECURSIVE `{name}` {why}, where MySQL's recursion limit and anchor column types apply, is not lowered yet"
        ))
    };
    if context.may_read_outer_rows() {
        return Err(refused(
            "in a subquery that may read an outer row, which MySQL may evaluate once",
        ));
    }
    let Query {
        with: None,
        body,
        order_by: None,
        limit_clause: None,
        fetch: None,
        ..
    } = cte.query.as_ref()
    else {
        return Err(refused("with a clause around its UNION"));
    };
    let SetExpr::SetOperation {
        op: SetOperator::Union,
        set_quantifier: SetQuantifier::All,
        left,
        right,
    } = body.as_ref()
    else {
        return Err(refused(
            "that is not an anchor UNION ALL a recursive SELECT",
        ));
    };
    let (SetExpr::Select(anchor), SetExpr::Select(step)) = (left.as_ref(), right.as_ref()) else {
        return Err(refused(
            "that is not an anchor UNION ALL a recursive SELECT",
        ));
    };
    if mentions(&anchor.to_string(), &name.value)? {
        return Err(refused("whose anchor reads it"));
    }
    let step_text = step.to_string();
    let selects = tokens(&step_text)?
        .iter()
        .filter(
            |token| matches!(&token.token, Token::Word(word) if word.keyword == Keyword::SELECT),
        )
        .count();
    if step.distinct.is_some()
        || step.having.is_some()
        || !step.named_window.is_empty()
        || step.qualify.is_some()
        || !matches!(&step.group_by, sqlparser::ast::GroupByExpr::Expressions(terms, modifiers) if terms.is_empty() && modifiers.is_empty())
        || selects != 1
    {
        return Err(refused(
            "whose recursive SELECT groups, is DISTINCT or has a subquery",
        ));
    }
    let names = if cte.alias.columns.is_empty() {
        anchor
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::ExprWithAlias { alias, .. } => Some(alias.clone()),
                SelectItem::UnnamedExpr(Expr::Identifier(column)) => Some(column.clone()),
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => parts.last().cloned(),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| refused("whose anchor has a column without a name"))?
    } else {
        if cte
            .alias
            .columns
            .iter()
            .any(|column| column.data_type.is_some())
        {
            return Err(refused("with typed columns"));
        }
        cte.alias
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect()
    };
    if anchor.projection.len() != names.len() || step.projection.len() != names.len() {
        return Err(refused("whose SELECTs do not have its columns"));
    }
    let inner = Ident::new(format!("__mysql_recursive_{}", name.value));
    let mut step = step.as_ref().clone();
    let reference = rename_reference(&mut step, name, &inner)
        .ok_or_else(|| refused("whose recursive SELECT does not read it once, in an inner join"))?;
    let mut integers = Vec::with_capacity(names.len());
    for (index, column) in names.iter().enumerate() {
        let anchor_value = item_expr(&anchor.projection[index]);
        let step_value = item_expr(&step.projection[index]);
        let unchanged = match step_value {
            Some(Expr::Identifier(read)) => read.value.eq_ignore_ascii_case(&column.value),
            Some(Expr::CompoundIdentifier(parts)) => matches!(
                parts.as_slice(),
                [qualifier, read] if qualifier.value.eq_ignore_ascii_case(&reference.value)
                    && read.value.eq_ignore_ascii_case(&column.value)
            ),
            _ => false,
        };
        if unchanged {
            integers.push(false);
        } else if anchor_value.is_some_and(is_int_literal) {
            integers.push(true);
        } else {
            return Err(refused(&format!(
                "whose column `{column}` takes the type of its anchor"
            )));
        }
    }
    let mut anchor = anchor.as_ref().clone();
    anchor
        .projection
        .push(SelectItem::UnnamedExpr(Expr::Value(ValueWithSpan {
            value: Value::Number("0".to_string(), false),
            span: Span::empty(),
        })));
    step.projection
        .push(SelectItem::UnnamedExpr(Expr::CompoundIdentifier(vec![
            reference,
            Ident::new(DEPTH),
        ])));
    let mut columns = names
        .iter()
        .map(|column| TableAliasColumnDef {
            name: column.clone(),
            data_type: None,
        })
        .collect::<Vec<_>>();
    columns.push(TableAliasColumnDef {
        name: Ident::new(DEPTH),
        data_type: None,
    });
    let mut query = cte.query.as_ref().clone();
    query.body = Box::new(SetExpr::SetOperation {
        op: SetOperator::Union,
        set_quantifier: SetQuantifier::All,
        left: Box::new(SetExpr::Select(Box::new(anchor))),
        right: Box::new(SetExpr::Select(Box::new(step))),
    });
    let inner_cte = Cte {
        alias: TableAlias {
            explicit: cte.alias.explicit,
            name: inner.clone(),
            columns,
            at: None,
        },
        query: Box::new(query),
        from: None,
        materialized: None,
        closing_paren_token: cte.closing_paren_token.clone(),
    };
    let mut inner_cte = translate_cte(&inner_cte, context)?;
    let step_columns = match inner_cte.select.body.compounds.as_mut_slice() {
        [ast::CompoundSelect {
            operator: ast::CompoundOperator::UnionAll,
            select: ast::OneSelect::Select { columns, .. },
        }] if columns.len() == names.len() + 1 => columns,
        _ => return Err(refused("whose lowering changes its UNION ALL")),
    };
    for (column, integer) in step_columns.iter_mut().zip(integers.iter().chain([&false])) {
        if *integer {
            wrap(column, "__mysql_recursive_integer")?;
        }
    }
    wrap(
        step_columns.last_mut().expect("the depth column"),
        "__mysql_recursion_depth",
    )?;
    let outer = ast::CommonTableExpr {
        tbl_name: translate_identifier(name)?,
        columns: names
            .iter()
            .map(|column| {
                Ok(ast::IndexedColumn {
                    col_name: translate_identifier(column)?,
                    collation_name: None,
                    order: None,
                })
            })
            .collect::<Result<Vec<_>, MysqlParserError>>()?,
        materialized: ast::Materialized::Any,
        select: ast::Select {
            with: None,
            body: ast::SelectBody {
                select: ast::OneSelect::Select {
                    distinctness: None,
                    columns: names
                        .iter()
                        .map(|column| {
                            Ok(ast::ResultColumn::Expr(
                                Box::new(ast::Expr::Id(translate_identifier(column)?)),
                                None,
                            ))
                        })
                        .collect::<Result<Vec<_>, MysqlParserError>>()?,
                    from: Some(ast::FromClause {
                        select: Box::new(ast::SelectTable::Table(
                            ast::QualifiedName::single(translate_identifier(&inner)?),
                            None,
                            None,
                        )),
                        joins: Vec::new(),
                    }),
                    where_clause: None,
                    group_by: None,
                    window_clause: Vec::new(),
                },
                compounds: Vec::new(),
            },
            order_by: Vec::new(),
            limit: None,
        },
    };
    Ok([inner_cte, outer])
}

fn wrap(column: &mut ast::ResultColumn, function: &str) -> Result<(), MysqlParserError> {
    let ast::ResultColumn::Expr(expr, _) = column else {
        return Err(MysqlParserError::Unsupported(
            "a recursive SELECT with a star is not lowered yet".to_string(),
        ));
    };
    let value = std::mem::replace(expr.as_mut(), ast::Expr::Literal(ast::Literal::Null));
    **expr = function_call(function, vec![value]);
    Ok(())
}

fn item_expr(item: &SelectItem) -> Option<&Expr> {
    match item {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => Some(expr),
        _ => None,
    }
}

/// An integer literal in the `INT` range, which MySQL types as `INT`.
fn is_int_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_int_literal(inner),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => is_int_literal(expr),
        Expr::Value(value) => matches!(
            &value.value,
            Value::Number(text, false) if text.parse::<i32>().is_ok()
        ),
        _ => false,
    }
}

/// Points the one read of `name` in `step`, a table of its `FROM` that is
/// the first of its join or inner joined, at `inner`, keeping the name it is
/// read by, which this returns.
fn rename_reference(step: &mut Select, name: &Ident, inner: &Ident) -> Option<Ident> {
    let mut found = None;
    for table in &mut step.from {
        if !table.joins.iter().all(|join| {
            matches!(
                join.join_operator,
                JoinOperator::Join(_) | JoinOperator::Inner(_) | JoinOperator::CrossJoin(_)
            )
        }) {
            return None;
        }
        let factors = std::iter::once(&mut table.relation)
            .chain(table.joins.iter_mut().map(|join| &mut join.relation));
        for factor in factors {
            match factor {
                TableFactor::Table {
                    name: table_name,
                    alias,
                    args: None,
                    with_hints,
                    version: None,
                    partitions,
                    json_path: None,
                    sample: None,
                    ..
                } if with_hints.is_empty()
                    && partitions.is_empty()
                    && matches!(
                        table_name.0.as_slice(),
                        [ObjectNamePart::Identifier(read)] if read.value.eq_ignore_ascii_case(&name.value)
                    ) =>
                {
                    if found.is_some() {
                        return None;
                    }
                    let reference = alias
                        .as_ref()
                        .map(|alias| alias.name.clone())
                        .unwrap_or_else(|| name.clone());
                    if alias
                        .as_ref()
                        .is_some_and(|alias| !alias.columns.is_empty())
                    {
                        return None;
                    }
                    *table_name = ObjectName(vec![ObjectNamePart::Identifier(inner.clone())]);
                    *alias = Some(TableAlias {
                        explicit: true,
                        name: reference.clone(),
                        columns: Vec::new(),
                        at: None,
                    });
                    found = Some(reference);
                }
                TableFactor::Table { .. } => {}
                _ => return None,
            }
        }
    }
    found
}

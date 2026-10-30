//! `HAVING` in a `SELECT` without `GROUP BY` or aggregates, which MySQL
//! applies to each row after `WHERE` (and before window functions), with the
//! select aliases in scope: it is the `WHERE` condition `(where) AND
//! (having)` with each alias named in `having` replaced by its expression.
//! A name MySQL resolves to a select alias is the alias. Refused: a name that
//! is neither an alias nor selected (MySQL rejects it, or, in a subquery
//! with a `FROM`, it may be an outer row's), an alias of a window function, a
//! subquery in `HAVING` that names an alias, and expressions this does not
//! rewrite.

use crate::dialect::tokens;
use crate::query::contains_aggregate;
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr, Select,
    SelectItem,
};
use sqlparser::tokenizer::Token;

/// The `WHERE` condition that does what `HAVING` and `WHERE` of `select` do,
/// when `select` has `HAVING` but no grouping. `in_subquery` tells whether
/// `select` may read an outer row.
pub(crate) fn ungrouped_filter(
    select: &Select,
    in_subquery: bool,
) -> Result<Option<Expr>, MysqlParserError> {
    let Some(having) = &select.having else {
        return Ok(None);
    };
    let ungrouped = matches!(&select.group_by, GroupByExpr::Expressions(terms, modifiers) if terms.is_empty() && modifiers.is_empty());
    let aggregated = contains_aggregate(having)
        || select.projection.iter().any(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                contains_aggregate(expr)
            }
            _ => false,
        });
    if !ungrouped || aggregated {
        return Ok(None);
    }
    let refused = || {
        MysqlParserError::Unsupported(format!(
            "HAVING `{having}` of a SELECT without grouping, which names what MySQL may resolve differently, is not lowered yet"
        ))
    };
    let aliases = select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::ExprWithAlias { expr, alias } => Some((alias.value.as_str(), expr)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let wildcard = select.projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
        )
    });
    let selected = |expr: &Expr| {
        wildcard
            || (select.from.is_empty() && in_subquery)
            || select.projection.iter().any(|item| match item {
                SelectItem::UnnamedExpr(item) | SelectItem::ExprWithAlias { expr: item, .. } => {
                    item.to_string().eq_ignore_ascii_case(&expr.to_string())
                }
                _ => false,
            })
    };
    let mut condition = having.clone();
    let rewriter = Rewriter {
        aliases: &aliases,
        selected: &selected,
    };
    rewriter.rewrite(&mut condition).map_err(|()| refused())?;
    let condition = match &select.selection {
        Some(selection) => Expr::BinaryOp {
            left: Box::new(Expr::Nested(Box::new(selection.clone()))),
            op: BinaryOperator::And,
            right: Box::new(Expr::Nested(Box::new(condition))),
        },
        None => condition,
    };
    Ok(Some(condition))
}

struct Rewriter<'a> {
    aliases: &'a [(&'a str, &'a Expr)],
    selected: &'a dyn Fn(&Expr) -> bool,
}

impl Rewriter<'_> {
    fn alias(&self, name: &str) -> Option<&Expr> {
        self.aliases
            .iter()
            .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
            .map(|(_, expr)| *expr)
    }

    fn mentions_alias(&self, expr: &Expr) -> Result<bool, ()> {
        Ok(tokens(&expr.to_string()).map_err(|_| ())?.iter().any(
            |token| matches!(&token.token, Token::Word(word) if self.alias(&word.value).is_some()),
        ))
    }

    fn rewrite(&self, expr: &mut Expr) -> Result<(), ()> {
        match expr {
            Expr::Identifier(name) => {
                if let Some(aliased) = self.alias(&name.value) {
                    if aliased.to_string().to_ascii_uppercase().contains(" OVER") {
                        return Err(());
                    }
                    *expr = Expr::Nested(Box::new(aliased.clone()));
                    Ok(())
                } else if (self.selected)(expr) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            Expr::CompoundIdentifier(_) => {
                if (self.selected)(expr) {
                    Ok(())
                } else {
                    Err(())
                }
            }
            Expr::Value(_) | Expr::TypedString(_) | Expr::Prefixed { .. } => Ok(()),
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
            | Expr::Cast { expr: inner, .. } => self.rewrite(inner),
            Expr::BinaryOp { left, right, .. } => {
                self.rewrite(left)?;
                self.rewrite(right)
            }
            Expr::InList { expr, list, .. } => {
                self.rewrite(expr)?;
                list.iter_mut().try_for_each(|item| self.rewrite(item))
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                self.rewrite(expr)?;
                self.rewrite(low)?;
                self.rewrite(high)
            }
            Expr::Like {
                expr,
                pattern,
                escape_char: None,
                ..
            } => {
                self.rewrite(expr)?;
                self.rewrite(pattern)
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                if let Some(operand) = operand {
                    self.rewrite(operand)?;
                }
                for when in conditions {
                    self.rewrite(&mut when.condition)?;
                    self.rewrite(&mut when.result)?;
                }
                if let Some(else_result) = else_result {
                    self.rewrite(else_result)?;
                }
                Ok(())
            }
            Expr::Function(function) if function.over.is_none() && function.filter.is_none() => {
                match &mut function.args {
                    FunctionArguments::None => Ok(()),
                    FunctionArguments::List(list) if list.clauses.is_empty() => {
                        list.args.iter_mut().try_for_each(|arg| match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) => self.rewrite(arg),
                            _ => Err(()),
                        })
                    }
                    _ => Err(()),
                }
            }
            Expr::Subquery(_) | Expr::Exists { .. } => {
                if self.mentions_alias(expr)? {
                    Err(())
                } else {
                    Ok(())
                }
            }
            Expr::InSubquery {
                expr: left,
                subquery,
                ..
            } => {
                if self.mentions_alias(&Expr::Subquery(subquery.clone()))? {
                    return Err(());
                }
                self.rewrite(left)
            }
            _ => Err(()),
        }
    }
}

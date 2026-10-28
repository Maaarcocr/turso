use crate::expr::{translate_condition, translate_expr};
use crate::names::translate_identifier;
use crate::query::{
    child_expressions, only_changes_the_plan, translate_table_name, TranslationContext,
};
use crate::MysqlParserError;
use sqlparser::ast::{
    Assignment, AssignmentTarget, Delete, Expr, FromTable, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, ObjectName, ObjectNamePart, TableAlias, TableFactor, TableWithJoins,
    Update,
};
use turso_parser::ast;

const UPDATE_MODIFIERS: &[&str] = &["LOW_PRIORITY", "IGNORE"];
const DELETE_MODIFIERS: &[&str] = &["LOW_PRIORITY", "QUICK", "IGNORE"];

pub(crate) fn translate_update(update: &Update) -> Result<ast::Stmt, MysqlParserError> {
    let Update {
        update_token: _,
        optimizer_hints,
        table,
        assignments,
        from,
        selection,
        returning,
        output,
        or,
        order_by,
        limit,
    } = update;
    if !optimizer_hints.iter().all(only_changes_the_plan) {
        return Err(unsupported(format!(
            "an optimizer hint that changes how `{update}` runs"
        )));
    }
    if or.is_some() || returning.is_some() || output.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "`{update}` is not MySQL syntax"
        )));
    }
    if from.is_some() {
        return Err(unsupported(format!("multi-table UPDATE `{update}`")));
    }
    if !order_by.is_empty() || limit.is_some() {
        return Err(unsupported(format!(
            "UPDATE with ORDER BY or LIMIT, which Turso's UPDATE cannot express, `{update}`"
        )));
    }
    let target = Target::from_table(table, "UPDATE", UPDATE_MODIFIERS)?;
    let mut context = TranslationContext::default();
    let sets = translate_assignments(&target, assignments, &mut context)?;
    let where_clause = selection
        .as_ref()
        .map(|condition| translate_condition(condition, &mut context).map(Box::new))
        .transpose()?;
    target.ensure_not_read_by_subquery(&context, "UPDATE")?;
    Ok(ast::Stmt::Update(ast::Update {
        with: None,
        or_conflict: None,
        tbl_name: target.name,
        indexed: None,
        sets,
        from: None,
        where_clause,
        returning: Vec::new(),
    }))
}

pub(crate) fn translate_delete(delete: &Delete) -> Result<ast::Stmt, MysqlParserError> {
    let Delete {
        delete_token: _,
        optimizer_hints,
        tables,
        from,
        using,
        selection,
        returning,
        output,
        order_by,
        limit,
    } = delete;
    if !optimizer_hints.iter().all(only_changes_the_plan) {
        return Err(unsupported(format!(
            "an optimizer hint that changes how `{delete}` runs"
        )));
    }
    if returning.is_some() || output.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "`{delete}` is not MySQL syntax"
        )));
    }
    if !tables.is_empty()
        && tables
            .iter()
            .all(|table| is_modifier(table, DELETE_MODIFIERS))
    {
        return Err(unsupported(format!(
            "DELETE modifiers LOW_PRIORITY, QUICK and IGNORE in `{delete}`"
        )));
    }
    let from = match from {
        FromTable::WithFromKeyword(from) => from,
        FromTable::WithoutKeyword(_) => {
            return Err(MysqlParserError::Unsupported(format!(
                "`{delete}` is not MySQL syntax"
            )));
        }
    };
    let ([table], true, None) = (from.as_slice(), tables.is_empty(), using) else {
        return Err(unsupported(format!("multi-table DELETE `{delete}`")));
    };
    if !order_by.is_empty() || limit.is_some() {
        return Err(unsupported(format!(
            "DELETE with ORDER BY or LIMIT, which Turso's DELETE cannot express, `{delete}`"
        )));
    }
    let target = Target::from_table(table, "DELETE", &[])?;
    let mut context = TranslationContext::default();
    let where_clause = selection
        .as_ref()
        .map(|condition| translate_condition(condition, &mut context).map(Box::new))
        .transpose()?;
    target.ensure_not_read_by_subquery(&context, "DELETE")?;
    Ok(ast::Stmt::Delete {
        with: None,
        tbl_name: target.name,
        indexed: None,
        where_clause,
        returning: Vec::new(),
    })
}

struct Target<'a> {
    name: ast::QualifiedName,
    qualifier: &'a Ident,
}

impl<'a> Target<'a> {
    fn from_table(
        table: &'a TableWithJoins,
        statement: &str,
        modifiers: &[&str],
    ) -> Result<Self, MysqlParserError> {
        let TableWithJoins { relation, joins } = table;
        if !joins.is_empty() {
            return Err(unsupported(format!(
                "multi-table {statement} with `{table}`"
            )));
        }
        let TableFactor::Table {
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
        } = relation
        else {
            return Err(unsupported(format!("{statement} target `{relation}`")));
        };
        if is_modifier(name, modifiers) {
            return Err(unsupported(format!(
                "{statement} modifier in `{statement} {relation}`"
            )));
        }
        if !with_hints.is_empty() || !index_hints.is_empty() {
            return Err(unsupported(format!(
                "index hints in {statement} target `{relation}`"
            )));
        }
        if !partitions.is_empty() {
            return Err(unsupported(format!(
                "partition selection in {statement} target `{name}`"
            )));
        }
        let mut translated = translate_table_name(name)?;
        let qualifier = match alias {
            None => last_identifier(name)?,
            Some(TableAlias {
                explicit: _,
                name: alias,
                columns,
                at: None,
            }) if columns.is_empty() => {
                translated.alias = Some(translate_identifier(alias)?);
                alias
            }
            Some(other) => {
                return Err(unsupported(format!("table alias `{other}`")));
            }
        };
        Ok(Self {
            name: translated,
            qualifier,
        })
    }

    fn ensure_not_read_by_subquery(
        &self,
        context: &TranslationContext,
        statement: &str,
    ) -> Result<(), MysqlParserError> {
        if context.reads_table(&self.name.name) {
            return Err(unsupported(format!(
                "{statement} whose subquery reads the target table `{}`, which MySQL rejects unless it materializes the subquery,",
                self.name.name.as_str()
            )));
        }
        Ok(())
    }

    fn column<'b>(&self, name: &'b ObjectName) -> Result<&'b Ident, MysqlParserError> {
        let parts = identifiers(name)?;
        match parts.as_slice() {
            [column] => Ok(column),
            [qualifier, column] if qualifier.value == self.qualifier.value => Ok(column),
            _ => Err(unsupported(format!("assignment to `{name}`"))),
        }
    }
}

fn translate_assignments(
    target: &Target,
    assignments: &[Assignment],
    context: &mut TranslationContext,
) -> Result<Vec<ast::Set>, MysqlParserError> {
    let mut assigned: Vec<&Ident> = Vec::with_capacity(assignments.len());
    let mut sets = Vec::with_capacity(assignments.len());
    for Assignment {
        target: assignment_target,
        value,
    } in assignments
    {
        let AssignmentTarget::ColumnName(name) = assignment_target else {
            return Err(unsupported(format!("assignment to `{assignment_target}`")));
        };
        let column = target.column(name)?;
        if assigned.iter().any(|earlier| same_column(earlier, column)) {
            return Err(unsupported(format!("UPDATE that assigns `{column}` twice")));
        }
        if is_default_keyword(value) {
            return Err(unsupported(format!(
                "`{column} = DEFAULT`, which Turso's UPDATE cannot express,"
            )));
        }
        if !assigned.is_empty() {
            let mut read = Vec::new();
            columns_read(target, value, &mut read)?;
            if let Some(earlier) = read
                .iter()
                .find(|read| assigned.iter().any(|earlier| same_column(earlier, read)))
            {
                return Err(unsupported(format!(
                    "UPDATE that reads `{earlier}` after assigning it, which MySQL reads as the new value and Turso as the old one,"
                )));
            }
        }
        sets.push(ast::Set {
            col_names: vec![translate_identifier(column)?],
            expr: Box::new(translate_expr(value, context)?),
        });
        assigned.push(column);
    }
    Ok(sets)
}

fn columns_read<'a>(
    target: &Target,
    expr: &'a Expr,
    read: &mut Vec<&'a Ident>,
) -> Result<(), MysqlParserError> {
    match expr {
        Expr::Identifier(column) => {
            read.push(column);
            Ok(())
        }
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [qualifier, column] if qualifier.value == target.qualifier.value => {
                read.push(column);
                Ok(())
            }
            _ => Err(unsupported(format!(
                "column reference `{expr}` after another assignment"
            ))),
        },
        Expr::Function(function) => {
            for argument in function_arguments(function)? {
                columns_read(target, argument, read)?;
            }
            Ok(())
        }
        other => match child_expressions(other) {
            Some(children) => {
                for child in children {
                    columns_read(target, child, read)?;
                }
                Ok(())
            }
            None => Err(unsupported(format!(
                "UPDATE value `{other}` after another assignment"
            ))),
        },
    }
}

fn function_arguments(function: &Function) -> Result<Vec<&Expr>, MysqlParserError> {
    let unreadable = || {
        unsupported(format!(
            "function call `{function}` after another assignment"
        ))
    };
    if function.filter.is_some() || function.over.is_some() || !function.within_group.is_empty() {
        return Err(unreadable());
    }
    match &function.args {
        FunctionArguments::None => Ok(Vec::new()),
        FunctionArguments::List(list) if list.clauses.is_empty() => list
            .args
            .iter()
            .map(|argument| match argument {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr),
                _ => Err(unreadable()),
            })
            .collect(),
        FunctionArguments::List(_) | FunctionArguments::Subquery(_) => Err(unreadable()),
    }
}

fn is_default_keyword(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Identifier(identifier)
            if identifier.quote_style.is_none() && identifier.value.eq_ignore_ascii_case("DEFAULT")
    )
}

fn is_modifier(name: &ObjectName, modifiers: &[&str]) -> bool {
    matches!(
        name.0.as_slice(),
        [ObjectNamePart::Identifier(identifier)]
            if identifier.quote_style.is_none()
                && modifiers
                    .iter()
                    .any(|modifier| identifier.value.eq_ignore_ascii_case(modifier))
    )
}

fn same_column(left: &Ident, right: &Ident) -> bool {
    left.value.eq_ignore_ascii_case(&right.value)
}

fn last_identifier(name: &ObjectName) -> Result<&Ident, MysqlParserError> {
    identifiers(name)?
        .pop()
        .ok_or_else(|| unsupported(format!("table name `{name}`")))
}

fn identifiers(name: &ObjectName) -> Result<Vec<&Ident>, MysqlParserError> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(identifier) => Ok(identifier),
            ObjectNamePart::Function(_) => Err(unsupported(format!("name `{name}`"))),
        })
        .collect()
}

fn unsupported(what: String) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

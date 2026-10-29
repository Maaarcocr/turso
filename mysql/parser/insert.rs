use crate::expr::translate_expr;
use crate::names::translate_identifier;
use crate::query::{only_changes_the_plan, translate_subquery, TranslationContext};
use crate::MysqlParserError;
use sqlparser::ast::{
    Assignment, AssignmentTarget, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
    Insert, ObjectName, ObjectNamePart, OnInsert, Query, SetExpr, TableObject, Values,
};
use turso_parser::ast;

pub(crate) fn translate_insert(insert: &Insert) -> Result<ast::Stmt, MysqlParserError> {
    if insert.ignore && (insert.replace_into || insert.on.is_some()) {
        return Err(unsupported(format!(
            "IGNORE with REPLACE or ON DUPLICATE KEY UPDATE in `{insert}`"
        )));
    }
    if insert.replace_into && insert.on.is_some() {
        return Err(unsupported(format!(
            "REPLACE with a conflict clause `{insert}`"
        )));
    }
    let or_conflict = insert.replace_into.then_some(ast::ResolveType::Replace);
    if !is_plain_insert(insert) {
        return Err(unsupported(format!("INSERT clause in `{insert}`")));
    }
    let TableObject::TableName(table) = &insert.table else {
        return Err(unsupported(format!("INSERT target `{}`", insert.table)));
    };
    let table = single_identifier(table)?;
    let tbl_name = ast::QualifiedName::single(translate_identifier(table)?);
    let mut context = TranslationContext::default();
    let (columns, select) = match (&insert.source, insert.assignments.as_slice()) {
        (Some(source), []) => {
            let columns = insert
                .columns
                .iter()
                .map(|column| translate_identifier(single_identifier(column)?))
                .collect::<Result<Vec<_>, _>>()?;
            if let SetExpr::Values(values) = source.body.as_ref() {
                if values.rows.iter().all(|row| row.is_empty()) {
                    if !columns.is_empty()
                        || values.rows.len() != 1
                        || insert.on.is_some()
                        || insert.ignore
                    {
                        return Err(unsupported(format!("INSERT of empty rows `{insert}`")));
                    }
                    return Ok(insert_statement(
                        or_conflict,
                        tbl_name,
                        Vec::new(),
                        ast::InsertBody::DefaultValues,
                    ));
                }
            }
            (columns, translate_source(source, &mut context)?)
        }
        (None, assignments) if !assignments.is_empty() && insert.columns.is_empty() => {
            translate_set_row(table, assignments, &mut context)?
        }
        _ => return Err(unsupported(format!("INSERT form `{insert}`"))),
    };
    let upsert = match &insert.on {
        None if insert.ignore => Some(Box::new(ast::Upsert {
            index: None,
            do_clause: ast::UpsertDo::Nothing,
            next: None,
        })),
        None => None,
        Some(OnInsert::DuplicateKeyUpdate(assignments)) => {
            if !matches!(
                insert.source.as_ref().map(|source| source.body.as_ref()),
                Some(SetExpr::Values(_))
            ) {
                return Err(unsupported(
                    "ON DUPLICATE KEY UPDATE after an INSERT that is not VALUES".to_string(),
                ));
            }
            let sets = translate_assignments(table, assignments, &mut context)?;
            Some(Box::new(ast::Upsert {
                index: None,
                do_clause: ast::UpsertDo::Set {
                    sets,
                    where_clause: None,
                },
                next: None,
            }))
        }
        Some(other) => return Err(unsupported(format!("INSERT conflict clause `{other}`"))),
    };
    Ok(insert_statement(
        or_conflict,
        tbl_name,
        columns,
        ast::InsertBody::Select(select, upsert),
    ))
}

fn insert_statement(
    or_conflict: Option<ast::ResolveType>,
    tbl_name: ast::QualifiedName,
    columns: Vec<ast::Name>,
    body: ast::InsertBody,
) -> ast::Stmt {
    ast::Stmt::Insert {
        with: None,
        or_conflict,
        tbl_name,
        columns,
        body,
        returning: Vec::new(),
    }
}

fn is_plain_insert(insert: &Insert) -> bool {
    insert.optimizer_hints.iter().all(only_changes_the_plan)
        && insert.or.is_none()
        && insert.table_alias.is_none()
        && !insert.overwrite
        && insert.partitioned.is_none()
        && insert.after_columns.is_empty()
        && !insert.has_table_keyword
        && insert.returning.is_none()
        && insert.output.is_none()
        && insert.insert_alias.is_none()
        && insert.settings.is_none()
        && insert.format_clause.is_none()
        && insert.multi_table_insert_type.is_none()
        && insert.multi_table_into_clauses.is_empty()
        && insert.multi_table_when_clauses.is_empty()
        && insert.multi_table_else_clause.is_none()
}

fn translate_source(
    source: &Query,
    context: &mut TranslationContext,
) -> Result<ast::Select, MysqlParserError> {
    let SetExpr::Values(values) = source.body.as_ref() else {
        return translate_subquery(source, context);
    };
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
    {
        return Err(unsupported(format!("INSERT source `{source}`")));
    }
    Ok(values_select(translate_rows(values, context)?))
}

fn values_select(select: ast::OneSelect) -> ast::Select {
    ast::Select {
        with: None,
        body: ast::SelectBody {
            select,
            compounds: Vec::new(),
        },
        order_by: Vec::new(),
        limit: None,
    }
}

fn translate_set_row(
    table: &Ident,
    assignments: &[Assignment],
    context: &mut TranslationContext,
) -> Result<(Vec<ast::Name>, ast::Select), MysqlParserError> {
    let mut columns: Vec<&Ident> = Vec::with_capacity(assignments.len());
    let mut row = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let column = assigned_column(table, &assignment.target)?;
        if columns.iter().any(|earlier| same_column(earlier, column)) {
            return Err(unsupported(format!(
                "INSERT ... SET that assigns `{column}` twice"
            )));
        }
        let mut read = Vec::new();
        if columns_read(table, &assignment.value, &mut read).is_err() || !read.is_empty() {
            return Err(unsupported(format!(
                "INSERT ... SET value `{}` that may read the inserted row",
                assignment.value
            )));
        }
        row.push(Box::new(translate_value(&assignment.value, context)?));
        columns.push(column);
    }
    let columns = columns
        .into_iter()
        .map(translate_identifier)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((columns, values_select(ast::OneSelect::Values(vec![row]))))
}

fn translate_value(
    value: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Expr::Identifier(name) = value {
        if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("DEFAULT") {
            return Err(unsupported("DEFAULT as an inserted value".to_string()));
        }
    }
    translate_expr(value, context)
}

fn translate_rows(
    values: &Values,
    context: &mut TranslationContext,
) -> Result<ast::OneSelect, MysqlParserError> {
    let Some(first) = values.rows.first() else {
        return Err(unsupported("INSERT without rows".to_string()));
    };
    if values.rows.iter().any(|row| row.len() != first.len()) {
        return Err(unsupported("VALUES rows of different lengths".to_string()));
    }
    let rows = values
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| translate_value(value, context).map(Box::new))
                .collect::<Result<Vec<_>, _>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::OneSelect::Values(rows))
}

fn translate_assignments(
    table: &Ident,
    assignments: &[Assignment],
    context: &mut TranslationContext,
) -> Result<Vec<ast::Set>, MysqlParserError> {
    let mut assigned: Vec<&Ident> = Vec::new();
    let mut sets = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let column = assigned_column(table, &assignment.target)?;
        if assigned.iter().any(|earlier| same_column(earlier, column)) {
            return Err(unsupported(format!(
                "ON DUPLICATE KEY UPDATE that assigns `{column}` twice"
            )));
        }
        if !assigned.is_empty() {
            let mut read = Vec::new();
            columns_read(table, &assignment.value, &mut read)?;
            if let Some(earlier) = read
                .iter()
                .find(|read| assigned.iter().any(|earlier| same_column(earlier, read)))
            {
                return Err(unsupported(format!(
                    "ON DUPLICATE KEY UPDATE that reads `{earlier}` after assigning it, which MySQL reads as the new value and SQLite as the old one,"
                )));
            }
        }
        context.inserted_row_is_readable = true;
        let value = translate_expr(&assignment.value, context);
        context.inserted_row_is_readable = false;
        sets.push(ast::Set {
            col_names: vec![translate_identifier(column)?],
            expr: Box::new(value?),
        });
        assigned.push(column);
    }
    Ok(sets)
}

fn assigned_column<'a>(
    table: &Ident,
    target: &'a AssignmentTarget,
) -> Result<&'a Ident, MysqlParserError> {
    let AssignmentTarget::ColumnName(name) = target else {
        return Err(unsupported(format!("assignment to `{target}`")));
    };
    let parts = identifiers(name)?;
    match parts.as_slice() {
        [column] => Ok(*column),
        [qualifier, column] if qualifier.value == table.value => Ok(*column),
        _ => Err(unsupported(format!("assignment to `{name}`"))),
    }
}

fn columns_read<'a>(
    table: &Ident,
    expr: &'a Expr,
    read: &mut Vec<&'a Ident>,
) -> Result<(), MysqlParserError> {
    match expr {
        Expr::Value(_) => Ok(()),
        Expr::Identifier(column) => {
            read.push(column);
            Ok(())
        }
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [qualifier, column] if qualifier.value == table.value => {
                read.push(column);
                Ok(())
            }
            _ => Err(unsupported(format!("column reference `{expr}`"))),
        },
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Cast { expr: inner, .. } => columns_read(table, inner, read),
        Expr::BinaryOp { left, right, .. } => {
            columns_read(table, left, read)?;
            columns_read(table, right, read)
        }
        Expr::Function(function) if is_values_function(function) => Ok(()),
        Expr::Function(function) => {
            let FunctionArguments::List(list) = &function.args else {
                return Err(unsupported(format!("function call `{function}`")));
            };
            for arg in &list.args {
                let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) = arg else {
                    return Err(unsupported(format!("function argument `{arg}`")));
                };
                columns_read(table, arg, read)?;
            }
            Ok(())
        }
        other => Err(unsupported(format!(
            "ON DUPLICATE KEY UPDATE value `{other}` after another assignment"
        ))),
    }
}

fn is_values_function(function: &sqlparser::ast::Function) -> bool {
    matches!(
        function.name.0.as_slice(),
        [ObjectNamePart::Identifier(name)]
            if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("VALUES")
    )
}

fn same_column(left: &Ident, right: &Ident) -> bool {
    left.value.eq_ignore_ascii_case(&right.value)
}

fn single_identifier(name: &ObjectName) -> Result<&Ident, MysqlParserError> {
    match identifiers(name)?.as_slice() {
        [identifier] => Ok(*identifier),
        _ => Err(unsupported(format!("qualified name `{name}`"))),
    }
}

fn identifiers(name: &ObjectName) -> Result<Vec<&Ident>, MysqlParserError> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(identifier) => Ok(identifier),
            _ => Err(unsupported(format!("name `{name}`"))),
        })
        .collect()
}

fn unsupported(what: String) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

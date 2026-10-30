use crate::expr::{function_call, translate_condition, translate_expr};
use crate::insert::{is_default_keyword, DefaultValue, Defaults};
use crate::names::translate_identifier;
use crate::query::{
    child_expressions, is_aggregate, only_changes_the_plan, translate_from,
    translate_order_by_expr, translate_table_name, TranslationContext,
};
use crate::{Environment, MysqlParserError};
use sqlparser::ast::{
    Assignment, AssignmentTarget, Delete, Expr, FromTable, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, Ident, ObjectName, ObjectNamePart, OrderByExpr, TableAlias, TableFactor,
    TableWithJoins, Update,
};
use turso_parser::ast;

const UPDATE_MODIFIERS: &[&str] = &["LOW_PRIORITY", "IGNORE"];
const DELETE_MODIFIERS: &[&str] = &["LOW_PRIORITY", "QUICK", "IGNORE"];

pub(crate) fn translate_update(
    update: &Update,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
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
    if !table.joins.is_empty() {
        if !order_by.is_empty() || limit.is_some() {
            return Err(MysqlParserError::Rejected(format!(
                "MySQL has no ORDER BY or LIMIT for a multi-table UPDATE like `{update}`"
            )));
        }
        return translate_multi_table_update(table, assignments, selection.as_ref(), environment);
    }
    let target = Target::from_table(table, "UPDATE", UPDATE_MODIFIERS)?;
    let mut context = TranslationContext::for_session(environment);
    let sets = translate_assignments(&target, assignments, environment, &mut context)?;
    let condition = translate_selection(selection.as_ref(), &mut context)?;
    let where_clause = if order_by.is_empty() && limit.is_none() {
        condition
    } else {
        ensure_order_is_not_assigned(&target, assignments, order_by)?;
        Some(Box::new(rows_in_order(
            &target,
            condition,
            order_by,
            limit.as_ref(),
            &mut context,
        )?))
    };
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
    let multi_table = match (tables.as_slice(), using) {
        ([], None) => None,
        ([], Some(joined)) => Some((
            from.iter()
                .map(|table| match table {
                    TableWithJoins {
                        relation:
                            TableFactor::Table {
                                name, alias: None, ..
                            },
                        joins,
                    } if joins.is_empty() => Ok(name),
                    _ => Err(unsupported(format!("DELETE target `{table}`"))),
                })
                .collect::<Result<Vec<_>, _>>()?,
            joined.as_slice(),
        )),
        (targets, None) => Some((targets.iter().collect(), from.as_slice())),
        (_, Some(_)) => {
            return Err(MysqlParserError::Unsupported(format!(
                "`{delete}` is not MySQL syntax"
            )));
        }
    };
    if let Some((targets, joined)) = multi_table {
        if !order_by.is_empty() || limit.is_some() {
            return Err(MysqlParserError::Rejected(format!(
                "MySQL has no ORDER BY or LIMIT for a multi-table DELETE like `{delete}`"
            )));
        }
        let [target] = targets.as_slice() else {
            return Err(unsupported(format!(
                "DELETE from several tables `{delete}`"
            )));
        };
        return translate_multi_table_delete(target, joined, selection.as_ref());
    }
    let [table] = from.as_slice() else {
        return Err(MysqlParserError::Unsupported(format!(
            "`{delete}` is not MySQL syntax"
        )));
    };
    let target = Target::from_table(table, "DELETE", &[])?;
    let mut context = TranslationContext::default();
    let condition = translate_selection(selection.as_ref(), &mut context)?;
    let where_clause = if order_by.is_empty() && limit.is_none() {
        condition
    } else {
        Some(Box::new(rows_in_order(
            &target,
            condition,
            order_by,
            limit.as_ref(),
            &mut context,
        )?))
    };
    target.ensure_not_read_by_subquery(&context, "DELETE")?;
    Ok(ast::Stmt::Delete {
        with: None,
        tbl_name: target.name,
        indexed: None,
        where_clause,
        returning: Vec::new(),
    })
}

fn translate_selection(
    selection: Option<&Expr>,
    context: &mut TranslationContext,
) -> Result<Option<Box<ast::Expr>>, MysqlParserError> {
    selection
        .map(|condition| translate_condition(condition, context).map(Box::new))
        .transpose()
}

/// `rowid IN (SELECT rowid FROM target WHERE condition ORDER BY ... LIMIT
/// n)`: the rows a single-table UPDATE or DELETE with ORDER BY or LIMIT
/// changes. Without ORDER BY, MySQL changes the first rows it reads, in an
/// order Turso does not share, so LIMIT alone is refused.
fn rows_in_order(
    target: &Target,
    condition: Option<Box<ast::Expr>>,
    order_by: &[OrderByExpr],
    limit: Option<&Expr>,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if order_by.is_empty() {
        return Err(unsupported(
            "LIMIT without ORDER BY in UPDATE or DELETE, whose rows MySQL picks in the order it reads them,"
                .to_string(),
        ));
    }
    if let Some(term) = order_by
        .iter()
        .find(|term| matches!(term.expr, Expr::Value(_)))
    {
        return Err(unsupported(format!(
            "ORDER BY `{term}` in UPDATE or DELETE"
        )));
    }
    let order_by = order_by
        .iter()
        .map(|term| translate_order_by_expr(term, context))
        .collect::<Result<Vec<_>, _>>()?;
    let limit = limit
        .map(|count| {
            translate_expr(count, context).map(|count| ast::Limit {
                expr: Box::new(count),
                offset: None,
            })
        })
        .transpose()?;
    let ast::QualifiedName {
        namespace,
        name,
        alias,
    } = target.name.clone();
    let table = ast::SelectTable::Table(
        ast::QualifiedName {
            namespace,
            name,
            alias: None,
        },
        alias.map(ast::As::As),
        None,
    );
    Ok(ast::Expr::InSelect {
        lhs: Box::new(rowid()),
        not: false,
        rhs: Box::new(select(
            vec![ast::ResultColumn::Expr(Box::new(rowid()), None)],
            ast::FromClause {
                select: Box::new(table),
                joins: Vec::new(),
            },
            condition,
            None,
            order_by,
            limit,
        )),
    })
}

/// MySQL changes the rows of an ordered UPDATE one after another, so whether
/// `UPDATE t SET k = k + 1 ORDER BY k DESC` meets a duplicate key depends on
/// that order, which Turso does not follow.
fn ensure_order_is_not_assigned(
    target: &Target,
    assignments: &[Assignment],
    order_by: &[OrderByExpr],
) -> Result<(), MysqlParserError> {
    let mut ordered = Vec::new();
    for term in order_by {
        if !column_names(&term.expr, &mut ordered) {
            return Err(unsupported(format!("UPDATE ordered by `{}`", term.expr)));
        }
    }
    for assignment in assignments {
        let AssignmentTarget::ColumnName(name) = &assignment.target else {
            return Err(unsupported(format!(
                "assignment to `{}`",
                assignment.target
            )));
        };
        let column = target.column(name)?;
        if ordered.iter().any(|ordered| same_column(ordered, column)) {
            return Err(unsupported(format!(
                "UPDATE ordered by `{column}`, which it assigns, and whose duplicate keys MySQL meets in the order it changes rows,"
            )));
        }
    }
    Ok(())
}

fn column_names<'a>(expr: &'a Expr, names: &mut Vec<&'a Ident>) -> bool {
    match expr {
        Expr::Identifier(column) => {
            names.push(column);
            true
        }
        Expr::CompoundIdentifier(parts) => match parts.last() {
            Some(column) => {
                names.push(column);
                true
            }
            None => false,
        },
        Expr::Function(function) => function_arguments(function)
            .is_ok_and(|args| args.into_iter().all(|arg| column_names(arg, names))),
        other => child_expressions(other)
            .is_some_and(|children| children.into_iter().all(|child| column_names(child, names))),
    }
}

/// `DELETE target FROM joined WHERE condition` (or `DELETE FROM target USING
/// joined ...`) as `DELETE FROM table WHERE rowid IN (SELECT target.rowid
/// FROM joined WHERE condition)`. Turso reads the whole subquery before it
/// deletes a row, as MySQL finds the rows before it deletes them.
fn translate_multi_table_delete(
    target: &ObjectName,
    joined: &[TableWithJoins],
    selection: Option<&Expr>,
) -> Result<ast::Stmt, MysqlParserError> {
    let [ObjectNamePart::Identifier(qualifier)] = target.0.as_slice() else {
        return Err(unsupported(format!("DELETE target `{target}`")));
    };
    let target = JoinedTable::find(joined, qualifier, "DELETE")?;
    let mut context = TranslationContext::default();
    let from = translate_from(joined, &mut context)?
        .ok_or_else(|| unsupported(format!("DELETE from `{qualifier}`")))?;
    let read_by_join = context.tables_read_count();
    let condition = translate_selection(selection, &mut context)?;
    target.ensure_not_read_since(&context, read_by_join, "DELETE")?;
    Ok(ast::Stmt::Delete {
        with: None,
        tbl_name: target.table,
        indexed: None,
        where_clause: Some(Box::new(ast::Expr::InSelect {
            lhs: Box::new(rowid()),
            not: false,
            rhs: Box::new(select(
                vec![ast::ResultColumn::Expr(
                    Box::new(ast::Expr::Qualified(target.qualifier, rowid_name())),
                    None,
                )],
                from,
                condition,
                None,
                Vec::new(),
                None,
            )),
        })),
        returning: Vec::new(),
    })
}

/// `UPDATE target JOIN other ... SET target.c = v WHERE condition` as
///
/// ```sql
/// UPDATE table AS __mysql_target SET c = __mysql_rows.__mysql_value_1
/// FROM (SELECT target.rowid AS __mysql_rowid, v AS __mysql_value_1,
///              count(1) AS __mysql_matches
///       FROM target JOIN other ... WHERE condition AND target.rowid IS NOT NULL
///       GROUP BY target.rowid) AS __mysql_rows
/// WHERE __mysql_target.rowid = __mysql_rows.__mysql_rowid
/// ```
///
/// Turso reads the grouped rows before it changes one, as MySQL does when it
/// joins a table with itself. MySQL changes a target row once, with the
/// values of the first joined row that matches it; when the values may read
/// another table, a row that several joined rows match is refused at run
/// time by `__mysql_one_joined_row`.
fn translate_multi_table_update(
    table: &TableWithJoins,
    assignments: &[Assignment],
    selection: Option<&Expr>,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    if let TableFactor::Table { name, .. } = &table.relation {
        if is_modifier(name, UPDATE_MODIFIERS) {
            return Err(unsupported(format!("UPDATE modifier in `UPDATE {table}`")));
        }
    }
    let qualifier = match assignments.first().map(|assignment| &assignment.target) {
        Some(AssignmentTarget::ColumnName(name)) => match identifiers(name)?.as_slice() {
            [qualifier, _] => *qualifier,
            _ => {
                return Err(unsupported(format!(
                    "multi-table UPDATE assignment to `{name}`, whose table MySQL finds by its columns,"
                )));
            }
        },
        _ => return Err(unsupported(format!("UPDATE of `{table}`"))),
    };
    let joined = JoinedTable::find(std::slice::from_ref(table), qualifier, "UPDATE")?;
    let target = Target {
        name: joined.table.clone(),
        qualifier,
    };
    for assignment in assignments {
        if may_aggregate(&assignment.value) {
            return Err(unsupported(format!(
                "UPDATE value `{}`, which may aggregate,",
                assignment.value
            )));
        }
    }
    if let Some(condition) = selection.filter(|condition| may_aggregate(condition)) {
        return Err(unsupported(format!(
            "UPDATE condition `{condition}`, which may aggregate,"
        )));
    }
    let mut context = TranslationContext::default();
    let from = translate_from(std::slice::from_ref(table), &mut context)?
        .ok_or_else(|| unsupported(format!("UPDATE of `{table}`")))?;
    let read_by_join = context.tables_read_count();
    let sets = translate_assignments(&target, assignments, environment, &mut context)?;
    let condition = translate_selection(selection, &mut context)?;
    joined.ensure_not_read_since(&context, read_by_join, "UPDATE")?;

    let target_rowid = || ast::Expr::Qualified(joined.qualifier.clone(), rowid_name());
    let name = |text: &str| ast::Name::exact(text.to_string());
    let rows = || name("__mysql_rows");
    let value_name = |index: usize| name(&format!("__mysql_value_{index}"));
    let mut columns = vec![ast::ResultColumn::Expr(
        Box::new(target_rowid()),
        Some(ast::As::As(name("__mysql_rowid"))),
    )];
    let mut outer_sets = Vec::with_capacity(sets.len());
    for (index, set) in sets.into_iter().enumerate() {
        columns.push(ast::ResultColumn::Expr(
            set.expr,
            Some(ast::As::As(value_name(index))),
        ));
        outer_sets.push(ast::Set {
            col_names: set.col_names,
            expr: Box::new(ast::Expr::Qualified(rows(), value_name(index))),
        });
    }
    columns.push(ast::ResultColumn::Expr(
        Box::new(function_call(
            "count",
            vec![ast::Expr::Literal(ast::Literal::Numeric("1".to_string()))],
        )),
        Some(ast::As::As(name("__mysql_matches"))),
    ));
    let matched = ast::Expr::NotNull(Box::new(target_rowid()));
    let condition = match condition {
        None => matched,
        Some(condition) => ast::Expr::binary(
            ast::Expr::Parenthesized(vec![condition]),
            ast::Operator::And,
            matched,
        ),
    };
    let grouped = select(
        columns,
        from,
        Some(Box::new(condition)),
        Some(ast::GroupBy {
            exprs: vec![Box::new(target_rowid())],
            having: None,
        }),
        Vec::new(),
        None,
    );
    let mut where_clause = ast::Expr::binary(
        ast::Expr::Qualified(name("__mysql_target"), rowid_name()),
        ast::Operator::Equals,
        ast::Expr::Qualified(rows(), name("__mysql_rowid")),
    );
    if !assignments
        .iter()
        .all(|assignment| reads_only(&assignment.value, qualifier))
    {
        where_clause = ast::Expr::binary(
            where_clause,
            ast::Operator::And,
            function_call(
                "__mysql_one_joined_row",
                vec![ast::Expr::Qualified(rows(), name("__mysql_matches"))],
            ),
        );
    }
    Ok(ast::Stmt::Update(ast::Update {
        with: None,
        or_conflict: None,
        tbl_name: ast::QualifiedName {
            alias: Some(name("__mysql_target")),
            ..joined.table
        },
        indexed: None,
        sets: outer_sets,
        from: Some(ast::FromClause {
            select: Box::new(ast::SelectTable::Select {
                select: grouped,
                alias: Some(ast::As::As(rows())),
                lateral: false,
            }),
            joins: Vec::new(),
        }),
        where_clause: Some(Box::new(where_clause)),
        returning: Vec::new(),
    }))
}

/// The table of a join that a multi-table UPDATE or DELETE changes: the one
/// whose alias, or name when it has none, is `qualifier`.
struct JoinedTable {
    table: ast::QualifiedName,
    qualifier: ast::Name,
}

impl JoinedTable {
    fn find(
        joined: &[TableWithJoins],
        qualifier: &Ident,
        statement: &str,
    ) -> Result<Self, MysqlParserError> {
        let mut found = Vec::new();
        for table in joined {
            tables_named(table, qualifier, &mut found);
        }
        let [name] = found.as_slice() else {
            return Err(unsupported(format!(
                "{statement} target `{qualifier}`, which is not one table of the join,"
            )));
        };
        Ok(Self {
            table: translate_table_name(name)?,
            qualifier: translate_identifier(qualifier)?,
        })
    }

    fn ensure_not_read_since(
        &self,
        context: &TranslationContext,
        mark: usize,
        statement: &str,
    ) -> Result<(), MysqlParserError> {
        if context.reads_table_since(mark, &self.table.name) {
            return Err(unsupported(format!(
                "{statement} whose subquery reads the target table `{}`, which MySQL rejects unless it materializes the subquery,",
                self.table.name.as_str()
            )));
        }
        Ok(())
    }
}

fn tables_named<'a>(table: &'a TableWithJoins, qualifier: &Ident, found: &mut Vec<&'a ObjectName>) {
    let factors =
        std::iter::once(&table.relation).chain(table.joins.iter().map(|join| &join.relation));
    for factor in factors {
        match factor {
            TableFactor::Table { name, alias, .. } => {
                let named = match alias {
                    Some(alias) => Some(&alias.name),
                    None => match name.0.last() {
                        Some(ObjectNamePart::Identifier(identifier)) => Some(identifier),
                        _ => None,
                    },
                };
                if named.is_some_and(|named| named.value == qualifier.value) {
                    found.push(name);
                }
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => tables_named(table_with_joins, qualifier, found),
            _ => {}
        }
    }
}

/// Whether `expr` reads no column but `qualifier.column`: then every joined
/// row that matches a target row gives it the same values.
fn reads_only(expr: &Expr, qualifier: &Ident) -> bool {
    match expr {
        Expr::Identifier(_) => false,
        Expr::CompoundIdentifier(parts) => {
            matches!(parts.as_slice(), [table, _] if same_column(table, qualifier))
        }
        Expr::Function(function) => function_arguments(function)
            .is_ok_and(|args| args.into_iter().all(|arg| reads_only(arg, qualifier))),
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => false,
        other => child_expressions(other).is_some_and(|children| {
            children
                .into_iter()
                .all(|child| reads_only(child, qualifier))
        }),
    }
}

/// Whether `expr` may hold an aggregate or a window function outside a
/// subquery, which MySQL rejects in UPDATE and that the grouped rows of a
/// multi-table UPDATE would compute.
fn may_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => {
            function.over.is_some()
                || is_aggregate(function)
                || function_arguments(function)
                    .map_or(true, |args| args.into_iter().any(may_aggregate))
        }
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Subquery(_)
        | Expr::Exists { .. } => false,
        Expr::InSubquery { expr, .. } => may_aggregate(expr),
        other => {
            child_expressions(other).is_none_or(|children| children.into_iter().any(may_aggregate))
        }
    }
}

fn select(
    columns: Vec<ast::ResultColumn>,
    from: ast::FromClause,
    where_clause: Option<Box<ast::Expr>>,
    group_by: Option<ast::GroupBy>,
    order_by: Vec<ast::SortedColumn>,
    limit: Option<ast::Limit>,
) -> ast::Select {
    ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns,
                from: Some(from),
                where_clause,
                group_by,
                window_clause: Vec::new(),
            },
            compounds: Vec::new(),
        },
        order_by,
        limit,
    }
}

fn rowid_name() -> ast::Name {
    ast::Name::exact("rowid".to_string())
}

fn rowid() -> ast::Expr {
    ast::Expr::Id(rowid_name())
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
            index_hints: _,
        } = relation
        else {
            return Err(unsupported(format!("{statement} target `{relation}`")));
        };
        if is_modifier(name, modifiers) {
            return Err(unsupported(format!(
                "{statement} modifier in `{statement} {relation}`"
            )));
        }
        if !with_hints.is_empty() {
            return Err(unsupported(format!(
                "table hints in {statement} target `{relation}`"
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
    environment: &dyn Environment,
    context: &mut TranslationContext,
) -> Result<Vec<ast::Set>, MysqlParserError> {
    let mut defaults = Defaults::new(target.name.name.as_str(), environment, false);
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
            let DefaultValue::Value(default) = defaults.default_of(&column.value)? else {
                return Err(unsupported(format!(
                    "`{column} = DEFAULT` of a generated column"
                )));
            };
            sets.push(ast::Set {
                col_names: vec![translate_identifier(column)?],
                expr: Box::new(default),
            });
            assigned.push(column);
            continue;
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
            expr: Box::new(defaults.column_value(&column.value, value, context)?),
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
            [qualifier, column] if same_column(qualifier, target.qualifier) => {
                read.push(column);
                Ok(())
            }
            [_, _] => Ok(()),
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

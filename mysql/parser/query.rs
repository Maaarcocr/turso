use crate::bitwise::is_bit_operation;
use crate::expr::{
    failing_json_cast, shows_latin1_as_text, translate_bytes, translate_condition, translate_expr,
    translate_window,
};
use crate::invalid_json::invalid_json_text_evaluated;
use crate::names::translate_identifier;
use crate::MysqlParserError;
use crate::{ColumnFacts, Environment};
use sqlparser::ast::{
    BinaryOperator, Cte, Distinct, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    GroupByExpr, Join, JoinConstraint, JoinOperator, LimitClause, NamedWindowDefinition,
    NamedWindowExpr, ObjectName, ObjectNamePart, Offset, OffsetRows, OptimizerHint, OrderBy,
    OrderByExpr, OrderByKind, Query, Select, SelectFlavor, SelectItem,
    SelectItemQualifiedWildcardKind, SelectModifiers, SetExpr, SetOperator, SetQuantifier,
    TableAlias, TableFactor, TableWithJoins, UnaryOperator, Value, Values,
    WildcardAdditionalOptions, With,
};
use std::num::NonZeroU32;
use turso_parser::{ast, MAX_EXPR_DEPTH};

#[derive(Default)]
pub(crate) struct TranslationContext<'a> {
    /// The session, when the statement is lowered for one; lowering that
    /// needs the schema refuses without it.
    pub(crate) environment: Option<&'a dyn Environment>,
    parameter_count: u32,
    expression_depth: usize,
    select_list_runs_once: bool,
    select_list_is_shown: bool,
    pub(crate) latin1_as_text: bool,
    pub(crate) unsigned_as_text: bool,
    /// Inside an operand compared with a column or another non-literal,
    /// where Turso compares a text column with a DOUBLE as text (`v IN
    /// (0e0)` is false for `v = '0'`). String literals in arithmetic keep
    /// Turso's number conversion there instead of becoming MySQL's DOUBLE.
    pub(crate) compared_beside_non_literal: bool,
    pub(crate) inserted_row_is_readable: bool,
    tables_read: Vec<ast::Name>,
}

impl<'a> TranslationContext<'a> {
    pub(crate) fn for_session(environment: &'a dyn Environment) -> Self {
        Self {
            environment: Some(environment),
            ..Self::default()
        }
    }

    pub(crate) fn reads_table(&self, table: &ast::Name) -> bool {
        self.reads_table_since(0, table)
    }

    /// How many table reads the translation has met so far, a mark for
    /// [`Self::reads_table_since`].
    pub(crate) fn tables_read_count(&self) -> usize {
        self.tables_read.len()
    }

    /// Whether a table read met after `mark` reads `table`.
    pub(crate) fn reads_table_since(&self, mark: usize, table: &ast::Name) -> bool {
        self.tables_read[mark..]
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
    context.select_list_is_shown =
        query.with.is_none() && matches!(query.body.as_ref(), SetExpr::Select(_));
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
        || locks.iter().any(|lock| lock.of.is_some())
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
    if let (SetExpr::Select(select), Some(order_by)) = (body.as_ref(), order_by) {
        check_order_by_names(select, order_by)?;
        if let Some(term) = order_by_outside_the_rows(select, order_by) {
            return Err(unsupported(format!(
                "ORDER BY `{term}` of a grouped or DISTINCT SELECT, which may use a column outside the selected rows that MySQL rejects and Turso allows, in `{query}`"
            )));
        }
    }
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
    check_only_full_group_by(select, context.environment)?;
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
    let shown = std::mem::take(&mut context.select_list_is_shown);
    let columns = projection
        .iter()
        .map(|item| translate_projection(item, runs_once, shown, context))
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

/// Checks a grouped `SELECT` as MySQL's `ONLY_FULL_GROUP_BY` does: an
/// expression of the select list or `HAVING` may use a column outside
/// `GROUP BY` only when the column is functionally dependent on the grouped
/// ones: a grouped key (the primary key or a unique key of `NOT NULL`
/// columns) of its table, or the `WHERE` clause equating it with a
/// constant. MySQL rejects other columns (1055, or 1140 without `GROUP BY`),
/// and so does Turso; a statement whose columns lowering cannot resolve
/// against the session's schema is refused, as Turso takes any value of the
/// group.
fn check_only_full_group_by(
    select: &Select,
    environment: Option<&dyn Environment>,
) -> Result<(), MysqlParserError> {
    let refused = || {
        Err(unsupported(format!(
            "a grouped SELECT that may use a column outside GROUP BY, which ONLY_FULL_GROUP_BY rejects in MySQL and Turso allows, `{select}`"
        )))
    };
    let GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return refused();
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
        return Ok(());
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
        table_names: &[],
    };
    let having_groups = Grouping {
        aliases: &aliases,
        ..groups.clone()
    };
    let projection_breaks = projections.iter().any(|expr| match expr {
        Some(expr) => uses_ungrouped_column(expr, &groups),
        None => true,
    });
    let having_breaks = select
        .having
        .as_ref()
        .is_some_and(|having| uses_ungrouped_column(having, &having_groups));
    if !projection_breaks && !having_breaks {
        return Ok(());
    }
    let Some(environment) = environment else {
        return refused();
    };
    let Some(sources) = Sources::of(select, environment)? else {
        return refused();
    };
    let mut fixed: Vec<SourceColumn> = Vec::new();
    for expr in &groups.exprs {
        if let Resolved::Column(column) = sources.resolve(without_parentheses(expr)) {
            fixed.push(column);
        }
    }
    if let Some(selection) = &select.selection {
        let mut constant = Vec::new();
        fixed_by_the_where_clause(selection, &mut constant);
        for expr in constant {
            if let Resolved::Column(column) = sources.resolve(expr) {
                fixed.push(column);
            }
        }
    }
    let mut equalities = Vec::new();
    let mut outer_join = false;
    if let Some(selection) = &select.selection {
        column_equalities(selection, &sources, &mut equalities);
    }
    for table in &select.from {
        for join in &table.joins {
            match &join.join_operator {
                JoinOperator::Join(constraint)
                | JoinOperator::Inner(constraint)
                | JoinOperator::CrossJoin(constraint)
                | JoinOperator::StraightJoin(constraint) => match constraint {
                    JoinConstraint::On(condition) => {
                        column_equalities(condition, &sources, &mut equalities);
                    }
                    JoinConstraint::None => {}
                    JoinConstraint::Using(_) | JoinConstraint::Natural => outer_join = true,
                },
                _ => outer_join = true,
            }
        }
    }
    let determined = loop {
        let determined = sources.determined(&fixed, environment)?;
        let before = fixed.len();
        for &index in &determined {
            for facts in &sources.tables[index].columns {
                let column = (index, facts.name.to_ascii_lowercase());
                if !fixed.contains(&column) {
                    fixed.push(column);
                }
            }
        }
        for (left, right) in &equalities {
            for (known, other) in [(left, right), (right, left)] {
                if fixed.contains(known) && !fixed.contains(other) {
                    fixed.push(other.clone());
                }
            }
        }
        if fixed.len() == before {
            break determined;
        }
    };
    let dependence = Dependence {
        groups: &groups,
        sources: &sources,
        fixed: &fixed,
        determined: &determined,
    };
    let mut verdict = Verdict::Grouped;
    for item in &select.projection {
        let item_verdict = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                dependence.of(expr)
            }
            SelectItem::Wildcard(_) => dependence.of_tables(None),
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(name), _) => {
                match name.0.as_slice() {
                    [ObjectNamePart::Identifier(qualifier)] => {
                        dependence.of_tables(Some(&qualifier.value))
                    }
                    _ => Verdict::Unknown,
                }
            }
            SelectItem::QualifiedWildcard(..) | SelectItem::ExprWithAliases { .. } => {
                Verdict::Unknown
            }
        };
        verdict = verdict.and(item_verdict);
    }
    if let Some(having) = &select.having {
        let having_dependence = Dependence {
            groups: &having_groups,
            ..dependence
        };
        verdict = verdict.and(having_dependence.of(having));
    }
    match verdict {
        Verdict::Grouped => Ok(()),
        Verdict::Ungrouped if outer_join => refused(),
        Verdict::Ungrouped if group_by.is_empty() => Err(MysqlParserError::Rejected(
            "In aggregated query without GROUP BY, the SELECT list or HAVING contains a nonaggregated column; this is incompatible with sql_mode=only_full_group_by".to_string(),
        )),
        Verdict::Ungrouped => Err(MysqlParserError::Rejected(
            "The SELECT list or HAVING is not in GROUP BY clause and contains a nonaggregated column which is not functionally dependent on columns in GROUP BY clause; this is incompatible with sql_mode=only_full_group_by".to_string(),
        )),
        Verdict::Unknown => refused(),
    }
}

/// The equalities between two columns that `condition` requires of every
/// row (the operands of `AND`).
fn column_equalities(
    condition: &Expr,
    sources: &Sources,
    equalities: &mut Vec<(SourceColumn, SourceColumn)>,
) {
    match without_parentheses(condition) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            column_equalities(left, sources, equalities);
            column_equalities(right, sources, equalities);
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            if let (Resolved::Column(left), Resolved::Column(right)) = (
                sources.resolve(without_parentheses(left)),
                sources.resolve(without_parentheses(right)),
            ) {
                equalities.push((left, right));
            }
        }
        _ => {}
    }
}

/// Whether an expression of a grouped `SELECT` uses only grouped columns.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Grouped,
    /// It uses a column MySQL finds outside `GROUP BY`.
    Ungrouped,
    Unknown,
}

impl Verdict {
    fn and(self, other: Verdict) -> Verdict {
        match (self, other) {
            (Verdict::Ungrouped, _) | (_, Verdict::Ungrouped) => Verdict::Ungrouped,
            (Verdict::Unknown, _) | (_, Verdict::Unknown) => Verdict::Unknown,
            (Verdict::Grouped, Verdict::Grouped) => Verdict::Grouped,
        }
    }
}

/// The base tables of a `FROM`, with their columns.
struct Sources {
    tables: Vec<Source>,
}

struct Source {
    table: String,
    /// The alias, or the name of the table.
    qualifier: String,
    columns: Vec<ColumnFacts>,
}

/// A column of a source: its index in `Sources::tables` and its name in
/// lower case.
type SourceColumn = (usize, String);

enum Resolved {
    Column(SourceColumn),
    NotAColumn,
    Unknown,
}

impl Sources {
    /// `None` when a table of the `FROM` is not a base table of the session.
    fn of(
        select: &Select,
        environment: &dyn Environment,
    ) -> Result<Option<Sources>, MysqlParserError> {
        let mut tables = Vec::new();
        for table in &select.from {
            for relation in std::iter::once(&table.relation)
                .chain(table.joins.iter().map(|join| &join.relation))
            {
                let TableFactor::Table {
                    name,
                    alias,
                    args: None,
                    ..
                } = relation
                else {
                    return Ok(None);
                };
                let [ObjectNamePart::Identifier(table_name)] = name.0.as_slice() else {
                    return Ok(None);
                };
                if alias
                    .as_ref()
                    .is_some_and(|alias| !alias.columns.is_empty())
                {
                    return Ok(None);
                }
                let Some(columns) = environment.columns_of(&table_name.value)? else {
                    return Ok(None);
                };
                tables.push(Source {
                    table: table_name.value.clone(),
                    qualifier: alias.as_ref().map_or_else(
                        || table_name.value.clone(),
                        |alias| alias.name.value.clone(),
                    ),
                    columns,
                });
            }
        }
        if tables.is_empty() {
            return Ok(None);
        }
        Ok(Some(Sources { tables }))
    }

    fn resolve(&self, expr: &Expr) -> Resolved {
        let has = |source: &Source, column: &str| {
            source
                .columns
                .iter()
                .any(|facts| facts.name.eq_ignore_ascii_case(column))
        };
        match expr {
            Expr::Identifier(column) => {
                let mut found = self
                    .tables
                    .iter()
                    .enumerate()
                    .filter(|(_, source)| has(source, &column.value));
                match (found.next(), found.next()) {
                    (Some((index, _)), None) => {
                        Resolved::Column((index, column.value.to_ascii_lowercase()))
                    }
                    _ => Resolved::Unknown,
                }
            }
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [qualifier, column] => {
                    let mut found = self
                        .tables
                        .iter()
                        .enumerate()
                        .filter(|(_, source)| source.qualifier == qualifier.value);
                    match (found.next(), found.next()) {
                        (Some((index, source)), None) if has(source, &column.value) => {
                            Resolved::Column((index, column.value.to_ascii_lowercase()))
                        }
                        _ => Resolved::Unknown,
                    }
                }
                _ => Resolved::Unknown,
            },
            _ => Resolved::NotAColumn,
        }
    }

    /// The sources whose rows `fixed` columns determine: a key of `NOT NULL`
    /// columns of theirs is fixed.
    fn determined(
        &self,
        fixed: &[SourceColumn],
        environment: &dyn Environment,
    ) -> Result<Vec<usize>, MysqlParserError> {
        let mut determined = Vec::new();
        for (index, source) in self.tables.iter().enumerate() {
            let not_null = |column: &str| {
                source
                    .columns
                    .iter()
                    .any(|facts| facts.name.eq_ignore_ascii_case(column) && facts.not_null)
            };
            let keys = environment.unique_keys_of(&source.table)?;
            if keys.iter().any(|key| {
                !key.is_empty()
                    && key.iter().all(|column| {
                        not_null(column) && fixed.contains(&(index, column.to_ascii_lowercase()))
                    })
            }) {
                determined.push(index);
            }
        }
        Ok(determined)
    }
}

/// What a grouped `SELECT` makes functionally dependent on its groups.
#[derive(Clone, Copy)]
struct Dependence<'a> {
    groups: &'a Grouping<'a>,
    sources: &'a Sources,
    fixed: &'a [SourceColumn],
    determined: &'a [usize],
}

impl Dependence<'_> {
    fn column(&self, column: &SourceColumn) -> Verdict {
        if self.determined.contains(&column.0) || self.fixed.contains(column) {
            Verdict::Grouped
        } else {
            Verdict::Ungrouped
        }
    }

    fn of(&self, expr: &Expr) -> Verdict {
        if self.groups.contains(expr) {
            return Verdict::Grouped;
        }
        match expr {
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => match self.sources.resolve(expr) {
                Resolved::Column(column) => self.column(&column),
                Resolved::NotAColumn | Resolved::Unknown => Verdict::Unknown,
            },
            Expr::Value(_) | Expr::TypedString(_) | Expr::Prefixed { .. } | Expr::Interval(_) => {
                Verdict::Grouped
            }
            Expr::Subquery(_) | Expr::Exists { .. } => Verdict::Grouped,
            Expr::Function(function) if is_aggregate(function) => Verdict::Grouped,
            Expr::Function(function) => match function_arguments(function) {
                Some(args) => args
                    .iter()
                    .fold(Verdict::Grouped, |verdict, arg| verdict.and(self.of(arg))),
                None => Verdict::Unknown,
            },
            Expr::InSubquery { expr, .. } => self.of(expr),
            other => match child_expressions(other) {
                Some(children) => children.iter().fold(Verdict::Grouped, |verdict, child| {
                    verdict.and(self.of(child))
                }),
                None => Verdict::Unknown,
            },
        }
    }

    /// The columns of every source, or of the one named `qualifier`.
    fn of_tables(&self, qualifier: Option<&str>) -> Verdict {
        let mut verdict = Verdict::Grouped;
        let mut matched = false;
        for (index, source) in self.sources.tables.iter().enumerate() {
            if qualifier.is_some_and(|qualifier| qualifier != source.qualifier) {
                continue;
            }
            matched = true;
            for facts in &source.columns {
                if facts.generated {
                    return Verdict::Unknown;
                }
                verdict = verdict.and(self.column(&(index, facts.name.to_ascii_lowercase())));
            }
        }
        if matched {
            verdict
        } else {
            Verdict::Unknown
        }
    }
}

/// Fails as MySQL does when an `ORDER BY` name is the alias of two different
/// selected expressions (1052). Refused: a name that is also a selected
/// column, or whose expressions may be the same column.
fn check_order_by_names(select: &Select, order_by: &OrderBy) -> Result<(), MysqlParserError> {
    let OrderByKind::Expressions(terms) = &order_by.kind else {
        return Ok(());
    };
    for term in terms {
        let Expr::Identifier(name) = &term.expr else {
            continue;
        };
        let mut aliased = Vec::new();
        let mut columns = 0;
        for item in &select.projection {
            match item {
                SelectItem::ExprWithAlias { expr, alias }
                    if alias.value.eq_ignore_ascii_case(&name.value) =>
                {
                    aliased.push(expr);
                }
                SelectItem::UnnamedExpr(Expr::Identifier(column))
                    if column.value.eq_ignore_ascii_case(&name.value) =>
                {
                    columns += 1;
                }
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts))
                    if parts
                        .last()
                        .is_some_and(|column| column.value.eq_ignore_ascii_case(&name.value)) =>
                {
                    columns += 1;
                }
                _ => {}
            }
        }
        let column_name = |expr: &Expr| match expr {
            Expr::Identifier(column) => Some(column.value.to_lowercase()),
            Expr::CompoundIdentifier(parts) => {
                parts.last().map(|column| column.value.to_lowercase())
            }
            _ => None,
        };
        let texts = aliased
            .iter()
            .map(|expr| expr.to_string())
            .collect::<std::collections::HashSet<_>>();
        if aliased.len() + columns < 2 || (columns == 0 && texts.len() < 2) {
            continue;
        }
        let column_names = aliased
            .iter()
            .filter_map(|expr| column_name(expr))
            .collect::<Vec<_>>();
        let same_column = column_names
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            < column_names.len();
        if columns > 0 || same_column {
            return Err(unsupported(format!(
                "ORDER BY `{name}`, which names more than one selected expression that MySQL may find ambiguous"
            )));
        }
        return Err(MysqlParserError::Rejected(format!(
            "Column '{}' in order clause is ambiguous",
            name.value
        )));
    }
    Ok(())
}

fn order_by_outside_the_rows<'a>(select: &Select, order_by: &'a OrderBy) -> Option<&'a Expr> {
    let OrderByKind::Expressions(terms) = &order_by.kind else {
        return None;
    };
    let GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return None;
    };
    let projections = select
        .projection
        .iter()
        .map(projected_expr)
        .collect::<Vec<_>>();
    let implicitly_grouped = projections
        .iter()
        .flatten()
        .any(|expr| contains_aggregate(expr))
        || select.having.as_ref().is_some_and(contains_aggregate)
        || terms.iter().any(|term| contains_aggregate(&term.expr));
    if group_by.is_empty() && implicitly_grouped {
        return None;
    }
    let table_names = single_table_names(select);
    let table_names = table_names.iter().map(String::as_str).collect::<Vec<_>>();
    let mut exprs = if !group_by.is_empty() {
        let mut grouping = group_by.iter().collect::<Vec<_>>();
        for term in group_by {
            grouping.extend(selected_by_group_term(term, select));
        }
        grouping
    } else if matches!(select.distinct, Some(Distinct::Distinct)) {
        if projections.iter().any(Option::is_none) {
            return None;
        }
        projections.iter().flatten().copied().collect()
    } else {
        return None;
    };
    let aliases = select
        .projection
        .iter()
        .filter_map(|item| match item {
            SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let ordinal = |term: &Expr| matches!(term, Expr::Value(value) if matches!(value.value, Value::Number(_, false)));
    exprs.extend(terms.iter().filter_map(|term| {
        ordinal(&term.expr)
            .then(|| selected_by_group_term(&term.expr, select))
            .flatten()
    }));
    if !table_names.is_empty() {
        if let Some(selection) = &select.selection {
            fixed_by_the_where_clause(selection, &mut exprs);
        }
    }
    let groups = Grouping {
        exprs,
        aliases: &aliases,
        table_names: &table_names,
    };
    terms
        .iter()
        .map(|term| &term.expr)
        .find(|term| !ordinal(term) && uses_ungrouped_column(term, &groups))
}

fn single_table_names(select: &Select) -> Vec<String> {
    let [TableWithJoins { relation, joins }] = select.from.as_slice() else {
        return Vec::new();
    };
    match relation {
        TableFactor::Table { name, alias, .. } if joins.is_empty() => match alias {
            Some(alias) => vec![alias.name.value.clone()],
            None => name
                .0
                .last()
                .and_then(ObjectNamePart::as_ident)
                .map(|name| vec![name.value.clone()])
                .unwrap_or_default(),
        },
        _ => Vec::new(),
    }
}

fn fixed_by_the_where_clause<'a>(condition: &'a Expr, fixed: &mut Vec<&'a Expr>) {
    fn is_constant(expr: &Expr) -> bool {
        match without_parentheses(expr) {
            Expr::Value(_) => true,
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => matches!(without_parentheses(expr), Expr::Value(_)),
            _ => false,
        }
    }
    match without_parentheses(condition) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            fixed_by_the_where_clause(left, fixed);
            fixed_by_the_where_clause(right, fixed);
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            for (column, value) in [(left, right), (right, left)] {
                if column_name_of(without_parentheses(column)).is_some() && is_constant(value) {
                    fixed.push(without_parentheses(column));
                }
            }
        }
        _ => {}
    }
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
        Expr::Nested(inner) if matches!(without_parentheses(inner), Expr::Identifier(_)) => {
            selected_by_group_term(without_parentheses(inner), select)
        }
        _ => None,
    }
}

#[derive(Clone)]
struct Grouping<'a> {
    exprs: Vec<&'a Expr>,
    aliases: &'a [&'a str],
    table_names: &'a [&'a str],
}

impl Grouping<'_> {
    fn contains(&self, expr: &Expr) -> bool {
        let expr = without_parentheses(expr);
        self.exprs.iter().any(|grouped| {
            let grouped = without_parentheses(grouped);
            grouped == expr
                || (!self.table_names.is_empty()
                    && unqualified(grouped, self.table_names)
                        == unqualified(expr, self.table_names))
        })
    }

    fn contains_column(&self, name: &str, qualified: bool) -> bool {
        (!qualified
            && self
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(name)))
            || self.exprs.iter().any(|grouped| {
                column_name_of(without_parentheses(grouped))
                    .is_some_and(|column| column.eq_ignore_ascii_case(name))
            })
    }
}

fn unqualified(expr: &Expr, table_names: &[&str]) -> Expr {
    let strip = |expr: &Expr| Box::new(unqualified(expr, table_names));
    match expr {
        Expr::CompoundIdentifier(parts)
            if parts.len() == 2
                && table_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(&parts[0].value)) =>
        {
            Expr::Identifier(parts[1].clone())
        }
        Expr::Nested(inner) => unqualified(inner, table_names),
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op: *op,
            expr: strip(expr),
        },
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: strip(left),
            op: op.clone(),
            right: strip(right),
        },
        Expr::Function(function) => {
            let mut function = function.clone();
            if let FunctionArguments::List(list) = &mut function.args {
                for arg in &mut list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) = arg {
                        *arg = unqualified(arg, table_names);
                    }
                }
            }
            Expr::Function(function)
        }
        other => other.clone(),
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
            !column_name_of(expr).is_some_and(|name| {
                groups.contains_column(name, matches!(expr, Expr::CompoundIdentifier(_)))
            })
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

pub(crate) fn is_aggregate(function: &Function) -> bool {
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

/// Whether a `/*+ ... */` block leaves the result of a statement that runs
/// to completion unchanged: every hint other than `SET_VAR` only chooses a
/// plan or bounds the run time (`MAX_EXECUTION_TIME`), and `SET_VAR` is
/// accepted for the variables that only size buffers or steer the optimizer.
pub(crate) fn only_changes_the_plan(hint: &OptimizerHint) -> bool {
    const PLAN_VARIABLES: &[&str] = &[
        "bulk_insert_buffer_size",
        "eq_range_index_dive_limit",
        "join_buffer_size",
        "max_heap_table_size",
        "max_seeks_for_key",
        "optimizer_prune_level",
        "optimizer_search_depth",
        "optimizer_switch",
        "range_optimizer_max_mem_size",
        "read_buffer_size",
        "read_rnd_buffer_size",
        "sort_buffer_size",
        "tmp_table_size",
    ];
    let text = hint.text.to_ascii_lowercase();
    let mut rest = text.as_str();
    while let Some(start) = rest.find("set_var") {
        let preceded_by_word = rest[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        rest = &rest[start + "set_var".len()..];
        if preceded_by_word {
            continue;
        }
        let Some(arguments) = rest.trim_start().strip_prefix('(') else {
            return false;
        };
        let Some((variable, _)) = arguments.split_once('=') else {
            return false;
        };
        if !PLAN_VARIABLES.contains(&variable.trim()) {
            return false;
        }
    }
    true
}

fn translate_projection(
    item: &SelectItem,
    runs_once: bool,
    shown: bool,
    context: &mut TranslationContext,
) -> Result<ast::ResultColumn, MysqlParserError> {
    match item {
        SelectItem::UnnamedExpr(expr) => {
            let name = implicit_column_name(expr);
            Ok(ast::ResultColumn::Expr(
                Box::new(translate_selected(expr, runs_once, shown, context)?),
                Some(ast::As::ImplicitColumnName(ast::Name::exact(name))),
            ))
        }
        SelectItem::ExprWithAlias { expr, alias } => {
            let name = match alias.quote_style {
                Some('\'' | '"') => ast::Name::exact(alias.value.clone()),
                _ => translate_identifier(alias)?,
            };
            Ok(ast::ResultColumn::Expr(
                Box::new(translate_selected(expr, runs_once, shown, context)?),
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
    shown: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let failing_text = runs_once
        .then(|| invalid_json_text_evaluated(expr))
        .flatten();
    context.latin1_as_text = runs_once && shown && shows_latin1_as_text(expr);
    context.unsigned_as_text = runs_once && shown && is_bit_operation(expr);
    let translated = match failing_text {
        Some(text) => Ok(failing_json_cast(text)),
        None => match crate::json::temporal_json_text(expr).filter(|_| shown) {
            Some(text) => Ok(text),
            None if shown => crate::expr::shown_negated_binary(expr, context)
                .unwrap_or_else(|| translate_bytes(expr, context)),
            None => translate_expr(expr, context),
        },
    };
    context.latin1_as_text = false;
    context.unsigned_as_text = false;
    translated
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

pub(crate) fn translate_from(
    from: &[TableWithJoins],
    context: &mut TranslationContext,
) -> Result<Option<ast::FromClause>, MysqlParserError> {
    let Some((first, rest)) = from.split_first() else {
        return Ok(None);
    };
    if rest.is_empty() && first.joins.is_empty() && is_dual(&first.relation) {
        return Ok(None);
    }
    check_table_references(from)?;
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

/// Fails as MySQL does when two tables of one `FROM` have the same name or
/// alias (1066) or when it joins more than 61 tables (1116). Refused: more
/// than 61 tables counting those of derived tables, which MySQL may merge
/// into the join.
fn check_table_references(from: &[TableWithJoins]) -> Result<(), MysqlParserError> {
    const MAX_TABLES: usize = 61;
    /// Where MySQL places a table of a `FROM`: two tables with one name (or
    /// alias) conflict only when they are in the same place.
    #[derive(Clone, PartialEq, Eq)]
    enum Place {
        /// A table of the current database, named without a database.
        Current,
        Database(String),
        /// A derived table or a table function, which is in no database.
        Derived,
    }
    #[derive(Default)]
    struct Count {
        names: Vec<(Place, String)>,
        tables: usize,
        merged: usize,
    }
    fn visit(factor: &TableFactor, count: &mut Count) -> Result<(), MysqlParserError> {
        let name = match factor {
            TableFactor::NestedJoin {
                table_with_joins,
                alias: None,
            } => {
                visit(&table_with_joins.relation, count)?;
                for join in &table_with_joins.joins {
                    visit(&join.relation, count)?;
                }
                return Ok(());
            }
            TableFactor::Table { name, alias, .. } => {
                count.merged += 1;
                let parts: Vec<String> = name
                    .0
                    .iter()
                    .map(|part| match part {
                        ObjectNamePart::Identifier(identifier) => identifier.value.clone(),
                        other => other.to_string(),
                    })
                    .collect();
                let place = match parts.as_slice() {
                    [_] => Place::Current,
                    [database, _] => Place::Database(database.clone()),
                    _ => return Err(unsupported(format!("table name `{name}`"))),
                };
                let table = parts.last().cloned().unwrap_or_default();
                Some((
                    place,
                    alias
                        .as_ref()
                        .map_or(table, |alias| alias.name.value.clone()),
                ))
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                count.merged += query_tables(subquery).max(1);
                alias
                    .as_ref()
                    .map(|alias| (Place::Derived, alias.name.value.clone()))
            }
            _ => {
                count.merged += 1;
                None
            }
        };
        count.tables += 1;
        if let Some((place, name)) = name {
            for (other_place, other) in &count.names {
                if *other != name {
                    continue;
                }
                match (&place, other_place) {
                    _ if place == *other_place => {
                        return Err(MysqlParserError::Rejected(format!(
                            "Not unique table/alias: '{name}'"
                        )));
                    }
                    (Place::Current, Place::Database(_)) | (Place::Database(_), Place::Current) => {
                        return Err(unsupported(format!(
                            "two tables named `{name}`, one of them in a named database,"
                        )));
                    }
                    _ => {}
                }
            }
            count.names.push((place, name));
        }
        Ok(())
    }
    fn query_tables(query: &Query) -> usize {
        let SetExpr::Select(select) = query.body.as_ref() else {
            return 1;
        };
        let mut count = Count::default();
        for table in &select.from {
            let _ = visit(&table.relation, &mut count);
            for join in &table.joins {
                let _ = visit(&join.relation, &mut count);
            }
        }
        count.merged
    }
    let mut count = Count::default();
    for table in from {
        visit(&table.relation, &mut count)?;
        for join in &table.joins {
            visit(&join.relation, &mut count)?;
        }
    }
    if count.tables > MAX_TABLES {
        return Err(MysqlParserError::Rejected(format!(
            "Too many tables; MySQL can only use {MAX_TABLES} tables in a join"
        )));
    }
    if count.merged > MAX_TABLES {
        return Err(unsupported(format!(
            "a join of {} tables counting those of its derived tables, which MySQL may merge past its limit of {MAX_TABLES}",
            count.merged
        )));
    }
    Ok(())
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
            index_hints: _,
        } if with_hints.is_empty() && partitions.is_empty() => {
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
            let (select, alias) = match alias {
                Some(alias) if !alias.columns.is_empty() => {
                    if *lateral {
                        return Err(unsupported(format!(
                            "LATERAL derived table with a column list `{alias}`"
                        )));
                    }
                    let unnamed = TableAlias {
                        columns: Vec::new(),
                        ..alias.clone()
                    };
                    (
                        with_column_names(select, &alias.columns)?,
                        Some(translate_table_alias(&unnamed)?),
                    )
                }
                other => (
                    select,
                    other.as_ref().map(translate_table_alias).transpose()?,
                ),
            };
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

/// Names the columns of a derived table, `(select) AS t (a, b)`, as
/// `(WITH d (a, b) AS (select) SELECT * FROM d) AS t`; a different number
/// of columns fails, as in MySQL.
fn with_column_names(
    select: ast::Select,
    columns: &[sqlparser::ast::TableAliasColumnDef],
) -> Result<ast::Select, MysqlParserError> {
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
    let name = ast::Name::exact("__mysql_derived_columns".to_string());
    Ok(ast::Select {
        with: Some(ast::With {
            recursive: false,
            ctes: vec![ast::CommonTableExpr {
                tbl_name: name.clone(),
                columns,
                materialized: ast::Materialized::Any,
                select,
            }],
        }),
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns: vec![ast::ResultColumn::Star],
                from: Some(ast::FromClause {
                    select: Box::new(ast::SelectTable::Table(
                        ast::QualifiedName::single(name),
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
    })
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

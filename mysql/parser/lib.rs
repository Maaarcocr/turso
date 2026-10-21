use sqlparser::ast::{
    CharacterLength, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, CreateTableOptions,
    DataType, Expr, GroupByExpr, HiveDistributionStyle, Ident, ObjectName, ObjectNamePart, Query,
    Select, SelectFlavor, SelectItem, SetExpr, Statement, Value,
};
use sqlparser::dialect::MySqlDialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::TokenizerError;
use std::num::NonZeroU32;
use thiserror::Error;
use turso_parser::ast;

mod split;

pub use split::{first_statement, split_statements, StatementSpan};

const MAX_BOOTSTRAP_VARCHAR_LENGTH: u64 = 16_383;

#[derive(Default)]
struct TranslationContext {
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

#[derive(Debug, Error)]
pub enum MysqlParserError {
    #[error("{0}")]
    Parse(#[from] ParserError),
    #[error("{0}")]
    Tokenize(#[from] TokenizerError),
    #[error("cannot locate MySQL statement boundaries: {0}")]
    Split(String),
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
    match statement {
        Statement::Query(query) => {
            let mut context = TranslationContext::default();
            translate_query(query, &mut context)
        }
        Statement::CreateTable(create_table) => translate_create_table(create_table),
        _ => Err(MysqlParserError::Unsupported(
            "only SELECT projections and basic CREATE TABLE are available in the bootstrap frontend"
                .to_string(),
        )),
    }
}

fn translate_create_table(create_table: &CreateTable) -> Result<ast::Stmt, MysqlParserError> {
    let CreateTable {
        or_replace,
        temporary,
        external,
        dynamic,
        global,
        if_not_exists,
        transient,
        volatile,
        iceberg,
        snapshot,
        name,
        columns,
        constraints,
        hive_distribution,
        hive_formats,
        table_options,
        file_format,
        location,
        query,
        without_rowid,
        like,
        clone,
        version,
        comment,
        on_commit,
        on_cluster,
        primary_key,
        order_by,
        partition_by,
        cluster_by,
        clustered_by,
        inherits,
        partition_of,
        for_values,
        strict,
        copy_grants,
        enable_schema_evolution,
        change_tracking,
        data_retention_time_in_days,
        max_data_extension_time_in_days,
        default_ddl_collation,
        with_aggregation_policy,
        with_row_access_policy,
        with_storage_lifecycle_policy,
        with_tags,
        external_volume,
        base_location,
        catalog,
        catalog_sync,
        storage_serialization_policy,
        target_lag,
        warehouse,
        refresh_mode,
        initialize,
        require_user,
        diststyle,
        distkey,
        sortkey,
        backup,
    } = create_table;

    if *or_replace
        || *temporary
        || *external
        || *dynamic
        || global.is_some()
        || *transient
        || *volatile
        || *iceberg
        || *snapshot
        || !constraints.is_empty()
        || !matches!(hive_distribution, HiveDistributionStyle::NONE)
        || hive_formats.is_some()
        || !matches!(table_options, CreateTableOptions::None)
        || file_format.is_some()
        || location.is_some()
        || query.is_some()
        || *without_rowid
        || like.is_some()
        || clone.is_some()
        || version.is_some()
        || comment.is_some()
        || on_commit.is_some()
        || on_cluster.is_some()
        || primary_key.is_some()
        || order_by.is_some()
        || partition_by.is_some()
        || cluster_by.is_some()
        || clustered_by.is_some()
        || inherits.is_some()
        || partition_of.is_some()
        || for_values.is_some()
        || *strict
        || *copy_grants
        || enable_schema_evolution.is_some()
        || change_tracking.is_some()
        || data_retention_time_in_days.is_some()
        || max_data_extension_time_in_days.is_some()
        || default_ddl_collation.is_some()
        || with_aggregation_policy.is_some()
        || with_row_access_policy.is_some()
        || with_storage_lifecycle_policy.is_some()
        || with_tags.is_some()
        || external_volume.is_some()
        || base_location.is_some()
        || catalog.is_some()
        || catalog_sync.is_some()
        || storage_serialization_policy.is_some()
        || target_lag.is_some()
        || warehouse.is_some()
        || refresh_mode.is_some()
        || initialize.is_some()
        || *require_user
        || diststyle.is_some()
        || distkey.is_some()
        || sortkey.is_some()
        || backup.is_some()
    {
        return Err(MysqlParserError::Unsupported(
            "CREATE TABLE contains a clause or table option outside the basic bootstrap subset"
                .to_string(),
        ));
    }
    if columns.is_empty() {
        return Err(MysqlParserError::Unsupported(
            "CREATE TABLE requires at least one column in the basic bootstrap subset".to_string(),
        ));
    }

    let table_name = translate_unqualified_name(name)?;
    let mut has_primary_key = false;
    let columns = columns
        .iter()
        .map(|column| translate_column(column, &mut has_primary_key))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ast::Stmt::CreateTable {
        temporary: false,
        if_not_exists: *if_not_exists,
        tbl_name: ast::QualifiedName::single(table_name),
        body: ast::CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints: Vec::new(),
            options: ast::TableOptions::empty(),
        },
    })
}

fn translate_unqualified_name(name: &ObjectName) -> Result<ast::Name, MysqlParserError> {
    let [ObjectNamePart::Identifier(identifier)] = name.0.as_slice() else {
        return Err(MysqlParserError::Unsupported(
            "CREATE TABLE requires one unqualified identifier".to_string(),
        ));
    };
    translate_identifier(identifier)
}

fn translate_identifier(identifier: &Ident) -> Result<ast::Name, MysqlParserError> {
    match identifier.quote_style {
        None | Some('`') => Ok(ast::Name::from_string(identifier.to_string())),
        Some(quote) => Err(MysqlParserError::Unsupported(format!(
            "identifier quote style `{quote}` cannot be preserved faithfully"
        ))),
    }
}

fn translate_column(
    column: &ColumnDef,
    has_primary_key: &mut bool,
) -> Result<ast::ColumnDefinition, MysqlParserError> {
    let ColumnDef {
        name,
        data_type,
        options,
    } = column;
    let (col_type, is_integer) = translate_column_type(data_type)?;
    let constraints = match options.as_slice() {
        [] => Vec::new(),
        [ColumnOptionDef {
            name: None,
            option: ColumnOption::PrimaryKey(primary_key),
        }] if is_integer => {
            let sqlparser::ast::PrimaryKeyConstraint {
                name,
                index_name,
                index_type,
                columns,
                index_options,
                characteristics,
            } = primary_key;
            if name.is_some()
                || index_name.is_some()
                || index_type.is_some()
                || !columns.is_empty()
                || !index_options.is_empty()
                || characteristics.is_some()
                || *has_primary_key
            {
                return Err(MysqlParserError::Unsupported(
                    "only one unnamed inline INTEGER PRIMARY KEY is available in CREATE TABLE"
                        .to_string(),
                ));
            }
            *has_primary_key = true;
            vec![ast::NamedColumnConstraint {
                name: None,
                constraint: ast::ColumnConstraint::PrimaryKey {
                    order: None,
                    conflict_clause: None,
                    auto_increment: false,
                },
            }]
        }
        _ => {
            return Err(MysqlParserError::Unsupported(
                "column options outside one unnamed inline INTEGER PRIMARY KEY are not available in CREATE TABLE"
                    .to_string(),
            ));
        }
    };

    Ok(ast::ColumnDefinition {
        col_name: translate_identifier(name)?,
        col_type: Some(col_type),
        constraints,
    })
}

fn translate_column_type(data_type: &DataType) -> Result<(ast::Type, bool), MysqlParserError> {
    match data_type {
        DataType::Integer(None) => Ok((
            ast::Type {
                name: "INTEGER".to_string(),
                size: None,
                array_dimensions: 0,
            },
            true,
        )),
        DataType::Varchar(Some(CharacterLength::IntegerLength { length, unit: None }))
            if (1..=MAX_BOOTSTRAP_VARCHAR_LENGTH).contains(length) =>
        {
            Ok((
                ast::Type {
                    name: "VARCHAR".to_string(),
                    size: Some(ast::TypeSize::MaxSize(Box::new(ast::Expr::Literal(
                        ast::Literal::Numeric(length.to_string()),
                    )))),
                    array_dimensions: 0,
                },
                false,
            ))
        }
        DataType::Varchar(Some(CharacterLength::IntegerLength { length, unit: None })) => {
            Err(MysqlParserError::Unsupported(format!(
                "VARCHAR length `{length}` is outside the safe bootstrap range 1..={MAX_BOOTSTRAP_VARCHAR_LENGTH}"
            )))
        }
        _ => Err(MysqlParserError::Unsupported(format!(
            "column type `{data_type}` is not available; expected exact INTEGER or VARCHAR(n)"
        ))),
    }
}

fn translate_query(
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
        assert!(error
            .to_string()
            .contains("literal and anonymous-parameter SELECT projections"));
    }

    #[test]
    fn rejects_multiple_statements() {
        let error = parse_one("SELECT 1; SELECT 2")
            .expect_err("multiple statements need consumed-offset support");
        assert!(matches!(error, MysqlParserError::MultipleStatements(2)));
    }
}

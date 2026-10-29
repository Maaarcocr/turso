use crate::names::{translate_ddl_identifier, translate_ddl_name};
use crate::stored_table::{Family, TableDefinition};
use crate::{Environment, MysqlParserError};
use sqlparser::ast::{
    AlterTableOperation, BinaryOperator, CreateIndex, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, IndexColumn, IndexOption, IndexType, ObjectName, ObjectNamePart,
    OrderByExpr, OrderByOptions, UnaryOperator, Value,
};
use turso_parser::ast;

pub(crate) struct KeyPart {
    pub(crate) name: ast::Name,
    pub(crate) prefix: Option<u64>,
    order: Option<ast::SortOrder>,
}

impl KeyPart {
    pub(crate) fn sorted_column(&self) -> ast::SortedColumn {
        ast::SortedColumn {
            expr: Box::new(ast::Expr::Id(self.name.clone())),
            order: self.order,
            nulls: None,
        }
    }

    pub(crate) fn is_descending(&self) -> bool {
        self.order == Some(ast::SortOrder::Desc)
    }
}

pub(crate) fn translate_create_index(
    create_index: &CreateIndex,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let CreateIndex {
        name,
        table_name,
        using,
        columns,
        unique,
        concurrently,
        if_not_exists,
        include,
        nulls_distinct,
        with,
        predicate,
        index_options,
        alter_options,
    } = create_index;
    let Some(name) = name else {
        return Err(MysqlParserError::Unsupported(
            "CREATE INDEX requires an index name".to_string(),
        ));
    };
    if *concurrently
        || *if_not_exists
        || !include.is_empty()
        || nulls_distinct.is_some()
        || !with.is_empty()
        || predicate.is_some()
        || !using.as_ref().is_none_or(is_btree_or_hash)
        || !index_options_have_no_effect(index_options)
        || !alter_options.iter().all(is_online_ddl_hint)
    {
        return Err(MysqlParserError::Unsupported(format!(
            "CREATE INDEX `{create_index}` contains a clause that is not available"
        )));
    }
    let table_name = translate_ddl_name(table_name)?;
    if let Some(table) = environment
        .table_sql(table_name.as_str())?
        .and_then(|sql| TableDefinition::parse(&sql).ok())
    {
        for column in columns {
            let Expr::Identifier(identifier) = &column.column.expr else {
                continue;
            };
            let Some(definition) = table.column(&identifier.value) else {
                continue;
            };
            match definition.family() {
                Family::Json => {
                    return Err(MysqlParserError::Rejected(format!(
                        "JSON column `{}` cannot be indexed directly",
                        identifier.value
                    )))
                }
                Family::Blob => {
                    return Err(MysqlParserError::Rejected(format!(
                        "BLOB/TEXT column `{}` used in a key needs a key length",
                        identifier.value
                    )))
                }
                Family::String | Family::Integer | Family::Other => {}
            }
        }
    }
    index_statement(*unique, translate_ddl_name(name)?, table_name, columns)
}

fn index_statement(
    unique: bool,
    index_name: ast::Name,
    table_name: ast::Name,
    columns: &[IndexColumn],
) -> Result<ast::Stmt, MysqlParserError> {
    if index_name.as_str().eq_ignore_ascii_case("PRIMARY") {
        return Err(MysqlParserError::Rejected(
            "an index cannot be named PRIMARY".to_string(),
        ));
    }
    let columns = columns
        .iter()
        .map(index_column)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::Stmt::CreateIndex {
        unique,
        if_not_exists: false,
        idx_name: ast::QualifiedName::single(index_name),
        tbl_name: table_name,
        using: None,
        columns,
        with_clause: Vec::new(),
        where_clause: None,
    })
}

fn index_column(column: &IndexColumn) -> Result<ast::SortedColumn, MysqlParserError> {
    let Expr::Nested(expr) = &column.column.expr else {
        let [part] = translate_key_parts(std::slice::from_ref(column))?
            .try_into()
            .unwrap_or_else(|_| unreachable!("one key part per index column"));
        if part.prefix.is_some() {
            return Err(MysqlParserError::Unsupported(
                "an index with a prefix length needs the column types to validate it".to_string(),
            ));
        }
        return Ok(part.sorted_column());
    };
    let IndexColumn {
        column:
            OrderByExpr {
                expr: _,
                options: OrderByOptions { asc, nulls_first },
                with_fill,
            },
        operator_class,
    } = column;
    if operator_class.is_some() || nulls_first.is_some() || with_fill.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "key part `{}` is not MySQL syntax",
            column.column
        )));
    }
    if matches!(unnested(expr), Expr::Identifier(_)) {
        return Err(MysqlParserError::Unsupported(format!(
            "functional key part `{}` of a bare column, which MySQL rejects, is not lowered",
            column.column.expr
        )));
    }
    Ok(ast::SortedColumn {
        expr: Box::new(functional_expr(expr).ok_or_else(|| {
            MysqlParserError::Unsupported(format!(
                "functional key part `{}` is not available",
                column.column.expr
            ))
        })?),
        order: asc.map(|asc| {
            if asc {
                ast::SortOrder::Asc
            } else {
                ast::SortOrder::Desc
            }
        }),
        nulls: None,
    })
}

fn unnested(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unnested(inner),
        other => other,
    }
}

/// A functional key part of columns and integer literals under `+`, `-`
/// and `*`, whose values Turso computes as MySQL does (other than BIGINT
/// overflow, which becomes a REAL).
fn functional_expr(expr: &Expr) -> Option<ast::Expr> {
    Some(match expr {
        Expr::Identifier(identifier) => ast::Expr::Id(translate_ddl_identifier(identifier).ok()?),
        Expr::Value(value) => match &value.value {
            Value::Number(digits, false)
                if digits.bytes().all(|byte| byte.is_ascii_digit())
                    && digits.parse::<i64>().is_ok() =>
            {
                ast::Expr::Literal(ast::Literal::Numeric(digits.clone()))
            }
            _ => return None,
        },
        Expr::Nested(inner) => ast::Expr::Parenthesized(vec![Box::new(functional_expr(inner)?)]),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => ast::Expr::Unary(
            ast::UnaryOperator::Negative,
            Box::new(functional_expr(expr)?),
        ),
        Expr::BinaryOp { left, op, right } => ast::Expr::Binary(
            Box::new(functional_expr(left)?),
            match op {
                BinaryOperator::Plus => ast::Operator::Add,
                BinaryOperator::Minus => ast::Operator::Subtract,
                BinaryOperator::Multiply => ast::Operator::Multiply,
                _ => return None,
            },
            Box::new(functional_expr(right)?),
        ),
        _ => return None,
    })
}

pub(crate) fn translate_key_parts(
    columns: &[IndexColumn],
) -> Result<Vec<KeyPart>, MysqlParserError> {
    columns
        .iter()
        .map(|column| {
            let IndexColumn {
                column:
                    OrderByExpr {
                        expr,
                        options: OrderByOptions { asc, nulls_first },
                        with_fill,
                    },
                operator_class,
            } = column;
            if operator_class.is_some() || nulls_first.is_some() || with_fill.is_some() {
                return Err(MysqlParserError::Unsupported(format!(
                    "key part `{}` is not MySQL syntax",
                    column.column
                )));
            }
            let (name, prefix) = match expr {
                Expr::Identifier(identifier) => (translate_ddl_identifier(identifier)?, None),
                Expr::Function(function) => prefix_key_part(function)?,
                _ => {
                    return Err(MysqlParserError::Unsupported(format!(
                        "functional key part `{expr}` is not available"
                    )));
                }
            };
            let order = asc.map(|asc| {
                if asc {
                    ast::SortOrder::Asc
                } else {
                    ast::SortOrder::Desc
                }
            });
            Ok(KeyPart {
                name,
                prefix,
                order,
            })
        })
        .collect()
}

pub(crate) fn translate_drop_index(
    names: &[ObjectName],
    table: &ast::Name,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let [name] = names else {
        return Err(MysqlParserError::Unsupported(
            "DROP INDEX is available for one index at a time".to_string(),
        ));
    };
    drop_index_statement(translate_ddl_name(name)?, table, environment)
}

/// `DROP INDEX name ON table`. Turso's index names belong to the database,
/// MySQL's to a table, so an index of another table is not dropped: MySQL
/// fails (1091) as it has no such key.
pub(crate) fn drop_index_statement(
    name: ast::Name,
    table: &ast::Name,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    if name.as_str().eq_ignore_ascii_case("PRIMARY") {
        return Err(MysqlParserError::Unsupported(
            "dropping the PRIMARY KEY is not available".to_string(),
        ));
    }
    let owner = environment.index_table(name.as_str())?;
    if !owner.is_some_and(|owner| owner.eq_ignore_ascii_case(table.as_str())) {
        return Err(MysqlParserError::Rejected(format!(
            "Can't DROP '{}'; check that column/key exists",
            name.as_str()
        )));
    }
    Ok(ast::Stmt::DropIndex {
        if_exists: false,
        idx_name: ast::QualifiedName::single(name),
    })
}

pub(crate) fn is_btree_or_hash(index_type: &IndexType) -> bool {
    matches!(index_type, IndexType::BTree | IndexType::Hash)
}

pub(crate) fn index_options_have_no_effect(options: &[IndexOption]) -> bool {
    options.iter().all(|option| match option {
        IndexOption::Using(index_type) => is_btree_or_hash(index_type),
        IndexOption::Comment(_) => true,
    })
}

fn prefix_key_part(function: &Function) -> Result<(ast::Name, Option<u64>), MysqlParserError> {
    let unsupported = || {
        MysqlParserError::Unsupported(format!("functional key part `{function}` is not available"))
    };
    let [ObjectNamePart::Identifier(identifier)] = function.name.0.as_slice() else {
        return Err(unsupported());
    };
    let FunctionArguments::List(list) = &function.args else {
        return Err(unsupported());
    };
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value)))] = list.args.as_slice()
    else {
        return Err(unsupported());
    };
    let Value::Number(length, _) = &value.value else {
        return Err(unsupported());
    };
    let length = length.parse::<u64>().map_err(|_| unsupported())?;
    if length == 0 || list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(unsupported());
    }
    Ok((translate_ddl_identifier(identifier)?, Some(length)))
}

fn is_online_ddl_hint(operation: &AlterTableOperation) -> bool {
    matches!(
        operation,
        AlterTableOperation::Algorithm { .. } | AlterTableOperation::Lock { .. }
    )
}

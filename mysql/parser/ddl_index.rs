use crate::names::{translate_ddl_identifier, translate_ddl_name};
use crate::MysqlParserError;
use sqlparser::ast::{
    AlterTableOperation, CreateIndex, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArguments, IndexColumn, IndexOption, IndexType, ObjectName, ObjectNamePart,
    OrderByExpr, OrderByOptions, Value,
};
use turso_parser::ast;

/// One column of a MySQL key, with the prefix length MySQL indexes when the
/// key covers only the start of a string.
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
    let index_name = translate_ddl_name(name)?;
    if index_name.as_str().eq_ignore_ascii_case("PRIMARY") {
        return Err(MysqlParserError::Rejected(
            "an index cannot be named PRIMARY".to_string(),
        ));
    }
    let key_parts = translate_key_parts(columns)?;
    if key_parts.iter().any(|part| part.prefix.is_some()) {
        return Err(MysqlParserError::Unsupported(
            "CREATE INDEX with a prefix length needs the column types to validate it".to_string(),
        ));
    }
    Ok(ast::Stmt::CreateIndex {
        unique: *unique,
        if_not_exists: false,
        idx_name: ast::QualifiedName::single(index_name),
        tbl_name: translate_ddl_name(table_name)?,
        using: None,
        columns: key_parts.iter().map(KeyPart::sorted_column).collect(),
        with_clause: Vec::new(),
        where_clause: None,
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

pub(crate) fn translate_drop_index(names: &[ObjectName]) -> Result<ast::Stmt, MysqlParserError> {
    let [name] = names else {
        return Err(MysqlParserError::Unsupported(
            "DROP INDEX is available for one index at a time".to_string(),
        ));
    };
    drop_index_statement(translate_ddl_name(name)?)
}

pub(crate) fn drop_index_statement(name: ast::Name) -> Result<ast::Stmt, MysqlParserError> {
    if name.as_str().eq_ignore_ascii_case("PRIMARY") {
        return Err(MysqlParserError::Unsupported(
            "dropping the PRIMARY KEY is not available".to_string(),
        ));
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

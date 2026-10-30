use crate::ddl_column::Column;
use crate::ddl_index::{drop_index_statement, translate_create_index, translate_drop_index};
use crate::expr::DEFAULT_COLLATION;
use crate::names::{translate_column_name, translate_ddl_identifier, translate_ddl_name};
use crate::{Environment, MysqlParserError};
use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnDef, ColumnOption, ColumnOptionDef, CreateIndex,
    DataType, Expr, Ident, IndexColumn, IndexConstraint, IndexOption, IndexType,
    NullsDistinctOption, ObjectName, ObjectNamePart, ObjectType, RenameTable, RenameTableNameKind,
    Statement, TableConstraint, UniqueConstraint,
};
use turso_parser::ast;

pub(crate) fn translate_alter_table(
    alter_table: &AlterTable,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let AlterTable {
        name,
        if_exists,
        only,
        operations,
        location,
        on_cluster,
        table_type,
        end_token: _,
    } = alter_table;
    let [operation] = operations.as_slice() else {
        return Err(MysqlParserError::Unsupported(
            "ALTER TABLE is available with exactly one operation".to_string(),
        ));
    };
    if *if_exists || *only || location.is_some() || on_cluster.is_some() || table_type.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "ALTER TABLE `{alter_table}` is not MySQL syntax"
        )));
    }
    let table_name = translate_ddl_name(name)?;
    let body = match operation {
        AlterTableOperation::AddColumn {
            column_keyword: _,
            if_not_exists: false,
            column_def,
            column_position: None,
        } => ast::AlterTableBody::AddColumn(
            Column::from_mysql(column_def)?.into_definition(DEFAULT_COLLATION)?,
        ),
        AlterTableOperation::DropColumn {
            has_column_keyword: _,
            column_names,
            if_exists: false,
            drop_behavior: None,
        } => {
            let [column_name] = column_names.as_slice() else {
                return Err(unsupported_operation(operation));
            };
            ast::AlterTableBody::DropColumn(translate_ddl_identifier(column_name)?)
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => ast::AlterTableBody::RenameColumn {
            old: translate_ddl_identifier(old_column_name)?,
            new: translate_column_name(new_column_name)?,
        },
        AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::To(new_name) | RenameTableNameKind::As(new_name),
        } => ast::AlterTableBody::RenameTo(translate_ddl_name(new_name)?),
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            options,
            column_position: None,
        } => altered_column(
            operation,
            &table_name,
            col_name,
            col_name,
            data_type,
            options,
            environment,
        )?,
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            options,
            column_position: None,
        } => altered_column(
            operation,
            &table_name,
            old_name,
            new_name,
            data_type,
            options,
            environment,
        )?,
        AlterTableOperation::DropIndex { name } => {
            return drop_index_statement(translate_ddl_identifier(name)?, &table_name, environment);
        }
        AlterTableOperation::AddConstraint {
            constraint:
                TableConstraint::Index(IndexConstraint {
                    display_as_key: _,
                    name: index_name,
                    index_type,
                    columns,
                    index_options,
                }),
            not_valid: false,
        } => {
            let index_name = match index_name {
                Some(index_name) => index_name.clone(),
                None => generated_index_name(operation, &table_name, columns, environment)?,
            };
            return add_index(
                name,
                false,
                index_name,
                index_type,
                columns,
                index_options,
                environment,
            );
        }
        AlterTableOperation::AddConstraint {
            constraint:
                TableConstraint::Unique(UniqueConstraint {
                    name: constraint_name,
                    index_name,
                    index_type_display: _,
                    index_type,
                    columns,
                    index_options,
                    characteristics: None,
                    nulls_distinct: NullsDistinctOption::None,
                }),
            not_valid: false,
        } => {
            let index_name = match index_name.as_ref().or(constraint_name.as_ref()) {
                Some(index_name) => index_name.clone(),
                None => generated_index_name(operation, &table_name, columns, environment)?,
            };
            return add_index(
                name,
                true,
                index_name,
                index_type,
                columns,
                index_options,
                environment,
            );
        }
        _ => return Err(unsupported_operation(operation)),
    };
    Ok(ast::Stmt::AlterTable(ast::AlterTable {
        name: ast::QualifiedName::single(table_name),
        body,
    }))
}

pub(crate) fn translate_rename_table(
    renames: &[RenameTable],
) -> Result<ast::Stmt, MysqlParserError> {
    let [RenameTable { old_name, new_name }] = renames else {
        return Err(MysqlParserError::Unsupported(
            "RENAME TABLE is available for one table at a time".to_string(),
        ));
    };
    Ok(ast::Stmt::AlterTable(ast::AlterTable {
        name: ast::QualifiedName::single(translate_ddl_name(old_name)?),
        body: ast::AlterTableBody::RenameTo(translate_ddl_name(new_name)?),
    }))
}

pub(crate) fn translate_drop(
    statement: &Statement,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let Statement::Drop {
        object_type,
        if_exists,
        names,
        cascade: _,
        restrict: _,
        purge,
        temporary,
        table,
    } = statement
    else {
        return Err(MysqlParserError::Unsupported(format!(
            "`{statement}` is not a DROP statement"
        )));
    };
    if *purge || *temporary {
        return Err(MysqlParserError::Unsupported(format!(
            "`{statement}` is not available"
        )));
    }
    match (object_type, table) {
        (ObjectType::Table, None) => {
            let [name] = names.as_slice() else {
                return Err(MysqlParserError::Unsupported(
                    "DROP TABLE is available for one table at a time".to_string(),
                ));
            };
            Ok(ast::Stmt::DropTable {
                if_exists: *if_exists,
                tbl_name: ast::QualifiedName::single(translate_ddl_name(name)?),
            })
        }
        (ObjectType::Index, Some(table)) if !*if_exists => {
            translate_drop_index(names, &translate_ddl_name(table)?, environment)
        }
        (ObjectType::View, None) => crate::ddl_view::translate_drop_view(names, *if_exists),
        _ => Err(MysqlParserError::Unsupported(format!(
            "`{statement}` is not available"
        ))),
    }
}

/// `CHANGE old new ...`, as core's `ALTER COLUMN`, which converts the stored
/// values through the new type as MySQL does. Core's `ALTER COLUMN` renames
/// the column, so `MODIFY` and a `CHANGE` that keeps the name are refused. A
/// column of a key, or with a check, a reference or a generated value, which
/// the new definition would drop in core but MySQL keeps, is refused.
fn altered_column(
    operation: &AlterTableOperation,
    table_name: &ast::Name,
    old_name: &Ident,
    new_name: &Ident,
    data_type: &DataType,
    options: &[ColumnOption],
    environment: &dyn Environment,
) -> Result<ast::AlterTableBody, MysqlParserError> {
    let unavailable = |reason: &str| {
        MysqlParserError::Unsupported(format!(
            "ALTER TABLE operation `{operation}` {reason} is not available"
        ))
    };
    let old = translate_ddl_identifier(old_name)?;
    if old
        .as_str()
        .eq_ignore_ascii_case(translate_ddl_identifier(new_name)?.as_str())
    {
        return Err(unavailable(
            "that keeps the column's name, which core's ALTER COLUMN does not take,",
        ));
    }
    let Some(sql) = environment.table_sql(table_name.as_str())? else {
        return Err(unavailable("on a table that is not in the schema"));
    };
    let mut parser = turso_parser::parser::Parser::new(sql.as_bytes()).with_type_parameter_lists();
    let Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateTable {
        body:
            ast::CreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                ..
            },
        ..
    }))) = parser.next_cmd()
    else {
        return Err(unavailable("on a table without a column list"));
    };
    let Some(column) = columns
        .iter()
        .find(|column| column.col_name.as_str().eq_ignore_ascii_case(old.as_str()))
    else {
        return Err(MysqlParserError::Rejected(format!(
            "Unknown column '{}' in '{}'",
            old.as_str(),
            table_name.as_str()
        )));
    };
    let keeps_more_than_its_type = |constraints: &[ast::NamedColumnConstraint]| {
        constraints.iter().any(|constraint| {
            !matches!(
                constraint.constraint,
                ast::ColumnConstraint::NotNull { .. }
                    | ast::ColumnConstraint::Default(_)
                    | ast::ColumnConstraint::Collate { .. }
            )
        })
    };
    let in_key = crate::stored_table::TableDefinition::from_body(&columns, &constraints)
        .unique_keys
        .iter()
        .any(|key| {
            key.iter()
                .any(|name| name.eq_ignore_ascii_case(old.as_str()))
        });
    let in_constraint = constraints.iter().any(|constraint| {
        !matches!(
            constraint.constraint,
            ast::TableConstraint::PrimaryKey { .. } | ast::TableConstraint::Unique { .. }
        )
    });
    if in_key || in_constraint || keeps_more_than_its_type(&column.constraints) {
        return Err(unavailable(
            "of a column in a key or of a table with a check, reference or generated value",
        ));
    }
    let definition = Column::from_mysql(&ColumnDef {
        name: new_name.clone(),
        data_type: data_type.clone(),
        options: options
            .iter()
            .map(|option| ColumnOptionDef {
                name: None,
                option: option.clone(),
            })
            .collect(),
    })?
    .into_definition(DEFAULT_COLLATION)?;
    if keeps_more_than_its_type(&definition.constraints) {
        return Err(unavailable(
            "that makes the column a key or gives it a check, reference or generated value",
        ));
    }
    Ok(ast::AlterTableBody::AlterColumn {
        old,
        new: definition,
    })
}

/// `ADD [UNIQUE] INDEX name (...)`, as `CREATE [UNIQUE] INDEX name ON table (...)`.
fn add_index(
    table_name: &ObjectName,
    unique: bool,
    index_name: Ident,
    index_type: &Option<IndexType>,
    columns: &[IndexColumn],
    index_options: &[IndexOption],
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let create_index = CreateIndex {
        name: Some(ObjectName(vec![ObjectNamePart::Identifier(index_name)])),
        table_name: table_name.clone(),
        using: index_type.clone(),
        columns: columns.to_vec(),
        unique,
        concurrently: false,
        if_not_exists: false,
        include: Vec::new(),
        nulls_distinct: None,
        with: Vec::new(),
        predicate: None,
        index_options: index_options.to_vec(),
        alter_options: Vec::new(),
    };
    translate_create_index(&create_index, environment)
}

/// The name MySQL gives an index added without one: its first column's,
/// when no index has it yet (MySQL adds a suffix otherwise).
fn generated_index_name(
    operation: &AlterTableOperation,
    table_name: &ast::Name,
    columns: &[IndexColumn],
    environment: &dyn Environment,
) -> Result<Ident, MysqlParserError> {
    let unavailable = || {
        MysqlParserError::Unsupported(format!(
            "ALTER TABLE operation `{operation}`, an index without a name, whose name MySQL generates from the keys of the table, is not available"
        ))
    };
    let Some(Expr::Identifier(column)) = columns.first().map(|column| &column.column.expr) else {
        return Err(unavailable());
    };
    let name = translate_ddl_identifier(column)?;
    if environment.index_table(name.as_str())?.is_some()
        || environment
            .table_sql(table_name.as_str())?
            .is_some_and(|sql| crate::ddl_index::may_have_unique_key_named(&sql, name.as_str()))
    {
        return Err(unavailable());
    }
    Ok(Ident::new(name.as_str()))
}

fn unsupported_operation(operation: &AlterTableOperation) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "ALTER TABLE operation `{operation}` is not available"
    ))
}

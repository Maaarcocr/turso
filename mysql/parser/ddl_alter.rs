use crate::ddl_column::Column;
use crate::ddl_index::{drop_index_statement, translate_create_index, translate_drop_index};
use crate::expr::DEFAULT_COLLATION;
use crate::names::{translate_column_name, translate_ddl_identifier, translate_ddl_name};
use crate::{Environment, MysqlParserError};
use sqlparser::ast::{
    AlterTable, AlterTableOperation, CreateIndex, Expr, Ident, IndexColumn, IndexConstraint,
    IndexOption, IndexType, NullsDistinctOption, ObjectName, ObjectNamePart, ObjectType,
    RenameTable, RenameTableNameKind, Statement, TableConstraint, UniqueConstraint,
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
                None => generated_index_name(operation, columns, environment)?,
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
            let Some(index_name) = index_name.as_ref().or(constraint_name.as_ref()) else {
                return Err(MysqlParserError::Unsupported(format!(
                    "ALTER TABLE operation `{operation}`, a unique key without a name, whose name MySQL generates from the keys of the table, is not available"
                )));
            };
            return add_index(
                name,
                true,
                index_name.clone(),
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
    if environment.index_table(name.as_str())?.is_some() {
        return Err(unavailable());
    }
    Ok(Ident::new(name.as_str()))
}

fn unsupported_operation(operation: &AlterTableOperation) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "ALTER TABLE operation `{operation}` is not available"
    ))
}

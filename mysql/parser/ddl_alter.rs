use crate::ddl_column::Column;
use crate::ddl_index::{drop_index_statement, translate_drop_index};
use crate::names::{translate_identifier, translate_unqualified_name};
use crate::MysqlParserError;
use sqlparser::ast::{
    AlterTable, AlterTableOperation, ObjectType, RenameTable, RenameTableNameKind, Statement,
};
use turso_parser::ast;

pub(crate) fn translate_alter_table(
    alter_table: &AlterTable,
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
    let table_name = translate_unqualified_name(name)?;
    let body = match operation {
        AlterTableOperation::AddColumn {
            column_keyword: _,
            if_not_exists: false,
            column_def,
            column_position: None,
        } => ast::AlterTableBody::AddColumn(Column::from_mysql(column_def)?.into_definition()?),
        AlterTableOperation::DropColumn {
            has_column_keyword: _,
            column_names,
            if_exists: false,
            drop_behavior: None,
        } => {
            let [column_name] = column_names.as_slice() else {
                return Err(unsupported_operation(operation));
            };
            ast::AlterTableBody::DropColumn(translate_identifier(column_name)?)
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => ast::AlterTableBody::RenameColumn {
            old: translate_identifier(old_column_name)?,
            new: translate_identifier(new_column_name)?,
        },
        AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::To(new_name) | RenameTableNameKind::As(new_name),
        } => ast::AlterTableBody::RenameTo(translate_unqualified_name(new_name)?),
        AlterTableOperation::DropIndex { name } => {
            return drop_index_statement(translate_identifier(name)?);
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
        name: ast::QualifiedName::single(translate_unqualified_name(old_name)?),
        body: ast::AlterTableBody::RenameTo(translate_unqualified_name(new_name)?),
    }))
}

pub(crate) fn translate_drop(statement: &Statement) -> Result<ast::Stmt, MysqlParserError> {
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
                tbl_name: ast::QualifiedName::single(translate_unqualified_name(name)?),
            })
        }
        (ObjectType::Index, Some(_)) if !*if_exists => translate_drop_index(names),
        _ => Err(MysqlParserError::Unsupported(format!(
            "`{statement}` is not available"
        ))),
    }
}

fn unsupported_operation(operation: &AlterTableOperation) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "ALTER TABLE operation `{operation}` is not available"
    ))
}

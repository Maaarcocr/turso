use crate::names::{translate_ddl_identifier, translate_ddl_name};
use crate::query::{translate_subquery, TranslationContext};
use crate::MysqlParserError;
use sqlparser::ast::{CreateTableOptions, CreateView, ObjectName, ViewColumnDef};
use turso_parser::ast;

pub(crate) fn translate_create_view(
    create_view: &CreateView,
) -> Result<ast::Stmt, MysqlParserError> {
    let CreateView {
        or_alter,
        or_replace,
        materialized,
        secure,
        name,
        name_before_not_exists: _,
        columns,
        query,
        options,
        cluster_by,
        comment,
        with_no_schema_binding,
        if_not_exists,
        temporary,
        copy_grants,
        to,
        params: _,
    } = create_view;
    if *or_replace {
        return Err(unsupported("CREATE OR REPLACE VIEW"));
    }
    if *or_alter
        || *materialized
        || *secure
        || *if_not_exists
        || *temporary
        || *copy_grants
        || *with_no_schema_binding
        || !matches!(options, CreateTableOptions::None)
        || !cluster_by.is_empty()
        || comment.is_some()
        || to.is_some()
    {
        return Err(unsupported(&format!("view clause in `{create_view}`")));
    }
    let columns = columns
        .iter()
        .map(view_column)
        .collect::<Result<Vec<_>, _>>()?;
    let mut context = TranslationContext::default();
    Ok(ast::Stmt::CreateView {
        temporary: false,
        if_not_exists: false,
        view_name: ast::QualifiedName::single(translate_ddl_name(name)?),
        columns,
        select: translate_subquery(query, &mut context)?,
    })
}

pub(crate) fn translate_drop_view(
    names: &[ObjectName],
    if_exists: bool,
) -> Result<ast::Stmt, MysqlParserError> {
    let [name] = names else {
        return Err(unsupported("DROP VIEW of several views"));
    };
    Ok(ast::Stmt::DropView {
        if_exists,
        view_name: ast::QualifiedName::single(translate_ddl_name(name)?),
    })
}

fn view_column(column: &ViewColumnDef) -> Result<ast::IndexedColumn, MysqlParserError> {
    let ViewColumnDef {
        name,
        data_type,
        options,
    } = column;
    if data_type.is_some() || options.is_some() {
        return Err(unsupported(&format!("view column `{column}`")));
    }
    Ok(ast::IndexedColumn {
        col_name: translate_ddl_identifier(name)?,
        collation_name: None,
        order: None,
    })
}

fn unsupported(what: &str) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

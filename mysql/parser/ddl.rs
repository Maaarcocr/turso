use crate::names::{translate_identifier, translate_unqualified_name};
use crate::MysqlParserError;
use sqlparser::ast::{
    CharacterLength, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable, CreateTableOptions,
    DataType, HiveDistributionStyle,
};
use turso_parser::ast;

const MAX_BOOTSTRAP_VARCHAR_LENGTH: u64 = 16_383;

pub(crate) fn translate_create_table(
    create_table: &CreateTable,
) -> Result<ast::Stmt, MysqlParserError> {
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

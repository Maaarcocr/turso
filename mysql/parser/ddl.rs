use crate::ddl_column::{
    ensure_supported_charset, ensure_supported_collation, translate_check, Column,
};
use crate::ddl_index::{
    index_options_have_no_effect, is_btree_or_hash, translate_key_parts, KeyPart,
};
use crate::ddl_type::ensure_key_length;
use crate::names::{translate_ddl_identifier, translate_ddl_name};
use crate::MysqlParserError;
use sqlparser::ast::{
    CreateTable, CreateTableOptions, Expr, ForeignKeyConstraint, HiveDistributionStyle, Ident,
    IndexColumn, IndexConstraint, IndexOption, IndexType, NamedParenthesizedList, ObjectName,
    ObjectNamePart, PrimaryKeyConstraint, ReferentialAction, SqlOption, TableConstraint,
    UniqueConstraint, Value,
};
use turso_parser::ast;

const STORAGE_ROW_FORMATS: [&str; 4] = ["DEFAULT", "DYNAMIC", "COMPACT", "REDUNDANT"];

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
        comment: _,
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
        || !matches!(hive_distribution, HiveDistributionStyle::NONE)
        || hive_formats.is_some()
        || file_format.is_some()
        || location.is_some()
        || query.is_some()
        || *without_rowid
        || like.is_some()
        || clone.is_some()
        || version.is_some()
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
            "CREATE TABLE contains a clause that is not available in the MySQL frontend"
                .to_string(),
        ));
    }
    if columns.is_empty() {
        return Err(MysqlParserError::Unsupported(
            "CREATE TABLE requires at least one column".to_string(),
        ));
    }
    translate_table_options(table_options)?;

    let table_name = translate_ddl_name(name)?;
    let columns = columns
        .iter()
        .map(Column::from_mysql)
        .collect::<Result<Vec<_>, _>>()?;
    let mut table = Table {
        columns,
        constraints: Vec::new(),
        key_names: Vec::new(),
        has_primary_key: false,
    };
    table.validate_inline_keys()?;
    for constraint in constraints {
        table.add_constraint(constraint)?;
    }
    let Table {
        columns,
        constraints: table_constraints,
        ..
    } = table;
    let columns = columns
        .into_iter()
        .map(Column::into_definition)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ast::Stmt::CreateTable {
        temporary: false,
        if_not_exists: *if_not_exists,
        tbl_name: ast::QualifiedName::single(table_name),
        body: ast::CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints: table_constraints,
            options: ast::TableOptions::empty(),
        },
    })
}

fn translate_table_options(options: &CreateTableOptions) -> Result<(), MysqlParserError> {
    let options = match options {
        CreateTableOptions::None => return Ok(()),
        CreateTableOptions::Plain(options) => options,
        other => {
            return Err(MysqlParserError::Unsupported(format!(
                "table options `{other}` are not available"
            )));
        }
    };
    for option in options {
        if !table_option_has_no_effect(option)? {
            return Err(MysqlParserError::Unsupported(format!(
                "table option `{option}` is not available"
            )));
        }
    }
    Ok(())
}

fn table_option_has_no_effect(option: &SqlOption) -> Result<bool, MysqlParserError> {
    match option {
        SqlOption::NamedParenthesizedList(NamedParenthesizedList {
            key,
            name: Some(engine),
            values,
        }) if key.value == "ENGINE" => {
            Ok(values.is_empty() && engine.value.eq_ignore_ascii_case("InnoDB"))
        }
        SqlOption::Comment(_) => Ok(true),
        SqlOption::KeyValue { key, value } => match key.value.to_ascii_uppercase().as_str() {
            "DEFAULT CHARSET" | "CHARSET" | "DEFAULT CHARACTER SET" | "CHARACTER SET" => {
                ensure_supported_charset(&option_word(value)?)?;
                Ok(true)
            }
            "DEFAULT COLLATE" | "COLLATE" => {
                ensure_supported_collation(&option_word(value)?)?;
                Ok(true)
            }
            "AUTO_INCREMENT" => Ok(matches!(
                value,
                Expr::Value(literal) if matches!(&literal.value, Value::Number(number, _) if number == "1")
            )),
            "ROW_FORMAT" => {
                let format = option_word(value)?.to_string().to_ascii_uppercase();
                Ok(STORAGE_ROW_FORMATS.contains(&format.as_str()))
            }
            "STATS_PERSISTENT" | "STATS_AUTO_RECALC" | "STATS_SAMPLE_PAGES" => Ok(true),
            _ => Ok(false),
        },
        _ => Ok(false),
    }
}

fn option_word(value: &Expr) -> Result<ObjectName, MysqlParserError> {
    let word = match value {
        Expr::Identifier(identifier) => identifier.clone(),
        Expr::Value(literal) => match &literal.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Ident::new(text),
            _ => return Err(unsupported_option_value(value)),
        },
        _ => return Err(unsupported_option_value(value)),
    };
    Ok(ObjectName(vec![ObjectNamePart::Identifier(word)]))
}

/// The columns and constraints of a CREATE TABLE while table constraints are
/// lowered, with the key names MySQL requires to be distinct.
struct Table {
    columns: Vec<Column>,
    constraints: Vec<ast::NamedTableConstraint>,
    key_names: Vec<String>,
    has_primary_key: bool,
}

impl Table {
    fn validate_inline_keys(&mut self) -> Result<(), MysqlParserError> {
        for column in &self.columns {
            if column.primary_key || column.unique {
                let bytes = column
                    .column_type
                    .key_part_bytes(column.name.as_str(), None)?;
                ensure_key_length(bytes)?;
            }
        }
        let inline_primary_keys = self
            .columns
            .iter()
            .filter(|column| column.primary_key)
            .count();
        if inline_primary_keys > 1 {
            return Err(MysqlParserError::Rejected(
                "a table can have only one PRIMARY KEY".to_string(),
            ));
        }
        let auto_increment_columns = self
            .columns
            .iter()
            .filter(|column| column.auto_increment)
            .count();
        if auto_increment_columns > 1 {
            return Err(MysqlParserError::Rejected(
                "a table can have only one AUTO_INCREMENT column".to_string(),
            ));
        }
        self.has_primary_key = inline_primary_keys == 1;
        Ok(())
    }

    fn add_constraint(&mut self, constraint: &TableConstraint) -> Result<(), MysqlParserError> {
        match constraint {
            TableConstraint::PrimaryKey(primary_key) => self.add_primary_key(primary_key),
            TableConstraint::Unique(unique) => self.add_unique(unique),
            TableConstraint::ForeignKey(foreign_key) => self.add_foreign_key(foreign_key),
            TableConstraint::Check(check) => {
                if let Some(expr) = translate_check(check)? {
                    self.constraints.push(ast::NamedTableConstraint {
                        name: check
                            .name
                            .as_ref()
                            .map(translate_ddl_identifier)
                            .transpose()?,
                        constraint: ast::TableConstraint::Check {
                            expr: Box::new(expr),
                            source: None,
                        },
                    });
                }
                Ok(())
            }
            TableConstraint::Index(index) => self.add_secondary_index(index),
            _ => Err(MysqlParserError::Unsupported(format!(
                "table constraint `{constraint}` is not available"
            ))),
        }
    }

    fn add_primary_key(
        &mut self,
        primary_key: &PrimaryKeyConstraint,
    ) -> Result<(), MysqlParserError> {
        let PrimaryKeyConstraint {
            name: _,
            index_name: _,
            index_type,
            columns: key_columns,
            index_options,
            characteristics,
        } = primary_key;
        if self.has_primary_key {
            return Err(MysqlParserError::Rejected(
                "a table can have only one PRIMARY KEY".to_string(),
            ));
        }
        if characteristics.is_some() {
            return Err(MysqlParserError::Unsupported(
                "PRIMARY KEY constraint characteristics are not MySQL syntax".to_string(),
            ));
        }
        self.has_primary_key = true;
        ensure_key_has_no_storage_options(index_type.as_ref(), index_options)?;
        let key_parts = self.whole_column_key(key_columns)?;
        for part in &key_parts {
            self.column_named(&part.name)?.mark_primary_key_member()?;
        }
        if let [part] = key_parts.as_slice() {
            let column = self.column_named(&part.name)?;
            if column.auto_increment && !part.is_descending() {
                column.primary_key = true;
                return Ok(());
            }
        }
        self.constraints.push(ast::NamedTableConstraint {
            name: None,
            constraint: ast::TableConstraint::PrimaryKey {
                columns: key_parts.iter().map(KeyPart::sorted_column).collect(),
                auto_increment: false,
                conflict_clause: None,
            },
        });
        Ok(())
    }

    fn add_unique(&mut self, unique: &UniqueConstraint) -> Result<(), MysqlParserError> {
        let UniqueConstraint {
            name,
            index_name,
            index_type_display: _,
            index_type,
            columns: key_columns,
            index_options,
            characteristics,
            nulls_distinct: _,
        } = unique;
        if characteristics.is_some() {
            return Err(MysqlParserError::Unsupported(
                "UNIQUE constraint characteristics are not MySQL syntax".to_string(),
            ));
        }
        ensure_key_has_no_storage_options(index_type.as_ref(), index_options)?;
        let name = index_name.as_ref().or(name.as_ref());
        self.claim_key_name(name)?;
        let key_parts = self.whole_column_key(key_columns)?;
        self.constraints.push(ast::NamedTableConstraint {
            name: name.map(translate_ddl_identifier).transpose()?,
            constraint: ast::TableConstraint::Unique {
                columns: key_parts.iter().map(KeyPart::sorted_column).collect(),
                conflict_clause: None,
            },
        });
        Ok(())
    }

    fn add_secondary_index(&mut self, index: &IndexConstraint) -> Result<(), MysqlParserError> {
        let IndexConstraint {
            display_as_key: _,
            name,
            index_type,
            columns: key_columns,
            index_options,
        } = index;
        ensure_key_has_no_storage_options(index_type.as_ref(), index_options)?;
        self.claim_key_name(name.as_ref())?;
        self.key_parts(key_columns)?;
        Ok(())
    }

    fn add_foreign_key(
        &mut self,
        foreign_key: &ForeignKeyConstraint,
    ) -> Result<(), MysqlParserError> {
        let ForeignKeyConstraint {
            name,
            index_name: _,
            columns: child_columns,
            foreign_table,
            referred_columns,
            on_delete,
            on_update,
            match_kind,
            characteristics,
        } = foreign_key;
        if match_kind.is_some()
            || characteristics.is_some()
            || referred_columns.len() != child_columns.len()
        {
            return Err(MysqlParserError::Unsupported(format!(
                "FOREIGN KEY `{foreign_key}` is not available"
            )));
        }
        let mut columns = Vec::new();
        for column in child_columns {
            let name = translate_ddl_identifier(column)?;
            self.column_named(&name)?;
            columns.push(indexed_column(name));
        }
        let referred_columns = referred_columns
            .iter()
            .map(|column| translate_ddl_identifier(column).map(indexed_column))
            .collect::<Result<Vec<_>, _>>()?;
        let mut args = Vec::new();
        if let Some(action) = on_delete {
            args.push(ast::RefArg::OnDelete(translate_referential_action(action)?));
        }
        if let Some(action) = on_update {
            args.push(ast::RefArg::OnUpdate(translate_referential_action(action)?));
        }
        self.constraints.push(ast::NamedTableConstraint {
            name: name.as_ref().map(translate_ddl_identifier).transpose()?,
            constraint: ast::TableConstraint::ForeignKey {
                columns,
                clause: ast::ForeignKeyClause {
                    tbl_name: translate_ddl_name(foreign_table)?,
                    columns: referred_columns,
                    args,
                },
                defer_clause: None,
            },
        });
        Ok(())
    }

    fn whole_column_key(
        &mut self,
        key_columns: &[IndexColumn],
    ) -> Result<Vec<KeyPart>, MysqlParserError> {
        let key_parts = self.key_parts(key_columns)?;
        if key_parts.iter().any(|part| part.prefix.is_some()) {
            return Err(MysqlParserError::Unsupported(
                "a unique key on a column prefix is not available".to_string(),
            ));
        }
        Ok(key_parts)
    }

    fn key_parts(&mut self, key_columns: &[IndexColumn]) -> Result<Vec<KeyPart>, MysqlParserError> {
        let key_parts = translate_key_parts(key_columns)?;
        let mut total_bytes = 0;
        for part in &key_parts {
            let column = self.column_named(&part.name)?;
            column.in_key = true;
            total_bytes += column
                .column_type
                .key_part_bytes(part.name.as_str(), part.prefix)?;
        }
        ensure_key_length(total_bytes)?;
        Ok(key_parts)
    }

    fn claim_key_name(&mut self, name: Option<&Ident>) -> Result<(), MysqlParserError> {
        let Some(name) = name else {
            return Ok(());
        };
        let name = name.value.to_lowercase();
        if self.key_names.contains(&name) {
            return Err(MysqlParserError::Rejected(format!(
                "duplicate key name `{name}`"
            )));
        }
        self.key_names.push(name);
        Ok(())
    }

    fn column_named(&mut self, name: &ast::Name) -> Result<&mut Column, MysqlParserError> {
        self.columns
            .iter_mut()
            .find(|column| column.has_name(name))
            .ok_or_else(|| {
                MysqlParserError::Rejected(format!(
                    "key column `{}` does not exist in the table",
                    name.as_str()
                ))
            })
    }
}

fn translate_referential_action(
    action: &ReferentialAction,
) -> Result<ast::RefAct, MysqlParserError> {
    match action {
        ReferentialAction::Restrict => Ok(ast::RefAct::Restrict),
        ReferentialAction::Cascade => Ok(ast::RefAct::Cascade),
        ReferentialAction::SetNull => Ok(ast::RefAct::SetNull),
        ReferentialAction::NoAction => Ok(ast::RefAct::NoAction),
        ReferentialAction::SetDefault => Err(MysqlParserError::Unsupported(
            "InnoDB rejects SET DEFAULT referential actions".to_string(),
        )),
    }
}

fn ensure_key_has_no_storage_options(
    index_type: Option<&IndexType>,
    options: &[IndexOption],
) -> Result<(), MysqlParserError> {
    if index_type.is_none_or(is_btree_or_hash) && index_options_have_no_effect(options) {
        return Ok(());
    }
    Err(MysqlParserError::Unsupported(
        "index options other than USING BTREE/HASH and COMMENT are not available".to_string(),
    ))
}

fn indexed_column(name: ast::Name) -> ast::IndexedColumn {
    ast::IndexedColumn {
        col_name: name,
        collation_name: None,
        order: None,
    }
}

fn unsupported_option_value(value: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("table option value `{value}` is not available"))
}

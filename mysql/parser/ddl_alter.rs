use crate::ddl_column::Column;
use crate::ddl_index::{drop_index_statement, translate_create_index, translate_drop_index};
use crate::ddl_rebuild::Rebuild;
use crate::expr::DEFAULT_COLLATION;
use crate::names::{translate_column_name, translate_ddl_identifier, translate_ddl_name};
use crate::{Environment, MysqlParserError};
use sqlparser::ast::{
    AlterTable, AlterTableOperation, ColumnDef, ColumnOption, ColumnOptionDef, CreateIndex,
    DataType, Ident, IndexColumn, IndexConstraint, IndexOption, IndexType, MySQLColumnPosition,
    NullsDistinctOption, ObjectName, ObjectNamePart, ObjectType, RenameTable, RenameTableNameKind,
    Statement, TableConstraint, UniqueConstraint,
};
use turso_parser::ast;

pub(crate) fn lower_alter_table(
    alter_table: &AlterTable,
    environment: &dyn Environment,
) -> Result<Vec<ast::Stmt>, MysqlParserError> {
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
    let rebuild = |change: &dyn Fn(&mut Rebuild) -> Result<(), MysqlParserError>| {
        let mut rebuild = Rebuild::of(&table_name, environment)?;
        change(&mut rebuild)?;
        rebuild.statements(environment)
    };
    match operation {
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::Check(check),
            not_valid: false,
        } => return rebuild(&|rebuild| rebuild.add_check(check)),
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::ForeignKey(foreign_key),
            not_valid: false,
        } => return rebuild(&|rebuild| rebuild.add_foreign_key(foreign_key)),
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::PrimaryKey(primary_key),
            not_valid: false,
        } => return rebuild(&|rebuild| rebuild.add_primary_key(primary_key)),
        AlterTableOperation::DropPrimaryKey {
            drop_behavior: None,
        } => return rebuild(&|rebuild| rebuild.drop_primary_key()),
        AlterTableOperation::AddColumn {
            column_keyword: _,
            if_not_exists: false,
            column_def,
            column_position: Some(position),
        } => return rebuild(&|rebuild| rebuild.add_column(column_def, Some(position))),
        _ => {}
    }
    let body = match operation {
        AlterTableOperation::AddColumn {
            column_keyword: _,
            if_not_exists: false,
            column_def,
            column_position: None,
        } => {
            let column = Column::from_mysql(column_def)?;
            if let Some(stored) = environment
                .table_sql(table_name.as_str())?
                .and_then(|sql| crate::stored_table::TableDefinition::parse(&sql).ok())
            {
                let types = stored
                    .columns
                    .iter()
                    .map(|column| column.column_type())
                    .collect::<Vec<_>>();
                crate::ddl_type::ensure_row_fits(
                    types
                        .iter()
                        .map(Option::as_ref)
                        .chain([Some(&column.column_type)]),
                )?;
            }
            let definition = column.into_definition(DEFAULT_COLLATION)?;
            if has_default_core_cannot_add(&definition) {
                return rebuild(&|rebuild| rebuild.add_column(column_def, None));
            }
            ast::AlterTableBody::AddColumn(definition)
        }
        AlterTableOperation::DropColumn {
            has_column_keyword: _,
            column_names,
            if_exists: false,
            drop_behavior: None,
        } => {
            let [column_name] = column_names.as_slice() else {
                return Err(unsupported_operation(operation));
            };
            let column_name = translate_ddl_identifier(column_name)?;
            ensure_no_functional_index_reads(&table_name, &column_name, environment)?;
            ast::AlterTableBody::DropColumn(column_name)
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => {
            let old = translate_ddl_identifier(old_column_name)?;
            let new = translate_column_name(new_column_name)?;
            if !old.as_str().eq_ignore_ascii_case(new.as_str()) {
                ensure_no_functional_index_reads(&table_name, &old, environment)?;
            }
            ast::AlterTableBody::RenameColumn { old, new }
        }
        AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::To(new_name) | RenameTableNameKind::As(new_name),
        } => {
            crate::ddl_index::ensure_indexes_keep_their_names(&table_name, environment)?;
            ast::AlterTableBody::RenameTo(translate_ddl_name(new_name)?)
        }
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            options,
            column_position,
        } => {
            return altered_column(
                operation,
                &table_name,
                col_name,
                None,
                column_position.as_ref(),
                data_type,
                options,
                environment,
            )
        }
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            options,
            column_position,
        } => {
            return altered_column(
                operation,
                &table_name,
                old_name,
                Some(new_name),
                column_position.as_ref(),
                data_type,
                options,
                environment,
            )
        }
        AlterTableOperation::DropIndex { name } => {
            return Ok(vec![drop_index_statement(
                translate_ddl_identifier(name)?,
                &table_name,
                environment,
            )?]);
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
            return Ok(vec![add_index(
                name,
                false,
                index_name,
                index_type,
                columns,
                index_options,
                environment,
            )?]);
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
            return Ok(vec![add_index(
                name,
                true,
                index_name,
                index_type,
                columns,
                index_options,
                environment,
            )?]);
        }
        _ => return Err(unsupported_operation(operation)),
    };
    Ok(vec![alter_table_statement(table_name, body)])
}

fn alter_table_statement(table_name: ast::Name, body: ast::AlterTableBody) -> ast::Stmt {
    ast::Stmt::AlterTable(ast::AlterTable {
        name: ast::QualifiedName::single(table_name),
        body,
    })
}

pub(crate) fn translate_rename_table(
    renames: &[RenameTable],
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let [RenameTable { old_name, new_name }] = renames else {
        return Err(MysqlParserError::Unsupported(
            "RENAME TABLE is available for one table at a time".to_string(),
        ));
    };
    let old_name = translate_ddl_name(old_name)?;
    crate::ddl_index::ensure_indexes_keep_their_names(&old_name, environment)?;
    Ok(ast::Stmt::AlterTable(ast::AlterTable {
        name: ast::QualifiedName::single(old_name),
        body: ast::AlterTableBody::RenameTo(translate_ddl_name(new_name)?),
    }))
}

/// `TRUNCATE [TABLE] t` as `DELETE FROM t`, which leaves the table as
/// MySQL's does when it has no `AUTO_INCREMENT` counter to reset, no
/// triggers (which `TRUNCATE` does not fire) and no foreign key that
/// references it (MySQL refuses those, or ignores them for one of the table
/// itself, where `DELETE` checks them). Inside a transaction, which `TRUNCATE`
/// commits first, it is refused before this.
pub(crate) fn translate_truncate(
    truncate: &sqlparser::ast::Truncate,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    let sqlparser::ast::Truncate {
        table_names,
        partitions,
        table: _,
        if_exists,
        identity,
        cascade,
        on_cluster,
    } = truncate;
    let refused = |reason: &str| {
        MysqlParserError::Unsupported(format!("`{truncate}`, {reason}, is not lowered yet"))
    };
    let [target] = table_names.as_slice() else {
        return Err(refused("which names several tables"));
    };
    if partitions.is_some()
        || *if_exists
        || identity.is_some()
        || cascade.is_some()
        || on_cluster.is_some()
        || target.only
        || target.has_asterisk
    {
        return Err(MysqlParserError::Unsupported(format!(
            "`{truncate}` is not MySQL syntax"
        )));
    }
    let name = translate_ddl_name(&target.name)?;
    let table = name.as_str();
    if let Some(columns) = environment.columns_of(table)? {
        if columns.iter().any(|column| column.auto_increment) {
            return Err(refused("which resets the AUTO_INCREMENT counter"));
        }
        if !environment.triggers_on(table)?.is_empty() {
            return Err(refused("of a table with triggers"));
        }
        if !environment.foreign_keys_to(table)?.is_empty() {
            return Err(refused("of a table a foreign key references"));
        }
    }
    Ok(ast::Stmt::Delete {
        with: None,
        tbl_name: ast::QualifiedName::single(name),
        indexed: None,
        where_clause: None,
        returning: Vec::new(),
    })
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

/// `CHANGE old new ...` (`new_name`) or `MODIFY old ...` (`None`), as
/// core's `ALTER COLUMN`, which converts the stored values through the new
/// type as MySQL does. Core's `ALTER COLUMN` renames the column, so one that
/// keeps the name alters it to a name the table does not have and renames
/// it back, in the transaction of the statement. A column of a key, or with
/// a check, a reference or a generated value, which the new definition would
/// drop in core but MySQL keeps, and a column moved to a `position`, are
/// altered by a rebuild of the table.
#[allow(clippy::too_many_arguments)]
fn altered_column(
    operation: &AlterTableOperation,
    table_name: &ast::Name,
    old_name: &Ident,
    new_name: Option<&Ident>,
    position: Option<&MySQLColumnPosition>,
    data_type: &DataType,
    options: &[ColumnOption],
    environment: &dyn Environment,
) -> Result<Vec<ast::Stmt>, MysqlParserError> {
    let unavailable = |reason: &str| {
        MysqlParserError::Unsupported(format!(
            "ALTER TABLE operation `{operation}` {reason} is not available"
        ))
    };
    let old = translate_ddl_identifier(old_name)?;
    if let Some(new_name) = new_name {
        if !old
            .as_str()
            .eq_ignore_ascii_case(translate_ddl_identifier(new_name)?.as_str())
        {
            ensure_no_functional_index_reads(table_name, &old, environment)?;
        }
    }
    let column_def = ColumnDef {
        name: new_name.unwrap_or(old_name).clone(),
        data_type: data_type.clone(),
        options: options
            .iter()
            .map(|option| ColumnOptionDef {
                name: None,
                option: option.clone(),
            })
            .collect(),
    };
    let rebuilt = || {
        let mut rebuild = Rebuild::of(table_name, environment)?;
        rebuild.modify_column(&old, &column_def, position, environment)?;
        rebuild.statements(environment)
    };
    if position.is_some() {
        return rebuilt();
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
        return rebuilt();
    }
    let final_name = match new_name {
        Some(new_name) => translate_ddl_identifier(new_name)?,
        None => column.col_name.clone(),
    };
    let temporary = old
        .as_str()
        .eq_ignore_ascii_case(final_name.as_str())
        .then(|| {
            (0..)
                .map(|n| format!("{}_turso_altered_{n}", final_name.as_str()))
                .find(|candidate| {
                    !columns
                        .iter()
                        .any(|column| column.col_name.as_str().eq_ignore_ascii_case(candidate))
                })
                .unwrap_or_else(|| unreachable!("a table has finitely many columns"))
        });
    let altered = Column::from_mysql(&column_def)?;
    if altered.auto_increment {
        return Err(unavailable("that makes the column AUTO_INCREMENT"));
    }
    let stored = crate::stored_table::TableDefinition::from_body(&columns, &constraints);
    if let Some(reason) =
        indexes_refusing_the_type(table_name, &stored, &old, &altered.column_type, environment)?
    {
        return Err(unavailable(reason));
    }
    let other_types = stored
        .columns
        .iter()
        .filter(|column| !column.name.eq_ignore_ascii_case(old.as_str()))
        .map(|column| column.column_type())
        .collect::<Vec<_>>();
    crate::ddl_type::ensure_row_fits(
        other_types
            .iter()
            .map(Option::as_ref)
            .chain([Some(&altered.column_type)]),
    )?;
    let mut definition = altered.into_definition(DEFAULT_COLLATION)?;
    if keeps_more_than_its_type(&definition.constraints) {
        return rebuilt();
    }
    let Some(temporary) = temporary else {
        return Ok(vec![alter_table_statement(
            table_name.clone(),
            ast::AlterTableBody::AlterColumn {
                old,
                new: definition,
            },
        )]);
    };
    definition.col_name = ast::Name::exact(temporary.clone());
    Ok(vec![
        alter_table_statement(
            table_name.clone(),
            ast::AlterTableBody::AlterColumn {
                old,
                new: definition,
            },
        ),
        alter_table_statement(
            table_name.clone(),
            ast::AlterTableBody::RenameColumn {
                old: ast::Name::exact(temporary),
                new: final_name,
            },
        ),
    ])
}

/// Checks the new type `column_type` of `column` against the indexes of
/// `table` that have it as a key part, as MySQL does: the type must be one
/// a key takes whole, within the key length. An index Turso keeps of a
/// string column may index a prefix of it, which Turso does not record, so
/// such an index, or one on an expression, gives the reason the operation
/// is not available.
pub(crate) fn indexes_refusing_the_type(
    table: &ast::Name,
    stored: &crate::stored_table::TableDefinition,
    column: &ast::Name,
    column_type: &crate::ddl_type::ColumnType,
    environment: &dyn Environment,
) -> Result<Option<&'static str>, MysqlParserError> {
    let is_column = |name: &str| name.eq_ignore_ascii_case(column.as_str());
    if environment
        .expression_index_columns(table.as_str())?
        .iter()
        .any(|read| is_column(read))
    {
        return Ok(Some("of a column an index expression reads"));
    }
    for parts in environment.index_key_columns(table.as_str())? {
        if !parts.iter().flatten().any(|part| is_column(part)) {
            continue;
        }
        let mut total_bytes = 0;
        for part in &parts {
            let Some(name) = part else {
                return Ok(Some("of a column of an index on an expression"));
            };
            let Some(part_type) = stored.column(name).and_then(|part| part.column_type()) else {
                return Ok(Some(
                    "of a column of an index whose key parts Turso cannot read",
                ));
            };
            if matches!(
                part_type.family,
                crate::ddl_type::Family::Char { .. }
                    | crate::ddl_type::Family::Varchar { .. }
                    | crate::ddl_type::Family::Text
                    | crate::ddl_type::Family::Binary { .. }
                    | crate::ddl_type::Family::Varbinary { .. }
                    | crate::ddl_type::Family::Blob
            ) {
                return Ok(Some(
                    "of a column of an index on a string column, which may index a prefix that Turso does not record,",
                ));
            }
            let part_type = if is_column(name) {
                column_type
            } else {
                &part_type
            };
            total_bytes += part_type.key_part_bytes(name, None)?;
        }
        crate::ddl_type::ensure_key_length(total_bytes)?;
    }
    Ok(None)
}

/// Whether core's `ADD COLUMN`, which fills the rows of the table with the
/// default of the column as SQLite does, refuses the default of
/// `definition`, which is not a constant.
fn has_default_core_cannot_add(definition: &ast::ColumnDefinition) -> bool {
    fn is_constant(expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::Literal(
                ast::Literal::CurrentDate
                | ast::Literal::CurrentTime
                | ast::Literal::CurrentTimestamp,
            ) => false,
            ast::Expr::Literal(_) => true,
            ast::Expr::Unary(_, operand) => is_constant(operand),
            ast::Expr::Parenthesized(items) => items.len() == 1 && is_constant(&items[0]),
            _ => false,
        }
    }
    definition
        .constraints
        .iter()
        .any(|constraint| match &constraint.constraint {
            ast::ColumnConstraint::Default(expr) => !is_constant(expr),
            _ => false,
        })
}

/// Fails as MySQL does (3837) to drop or rename `column`, which an index of
/// `table` on an expression reads.
fn ensure_no_functional_index_reads(
    table: &ast::Name,
    column: &ast::Name,
    environment: &dyn Environment,
) -> Result<(), MysqlParserError> {
    if environment
        .expression_index_columns(table.as_str())?
        .iter()
        .any(|read| read.eq_ignore_ascii_case(column.as_str()))
    {
        return Err(MysqlParserError::Rejected(format!(
            "Column '{}' has a functional index dependency and cannot be dropped or renamed.",
            column.as_str()
        )));
    }
    Ok(())
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
/// followed by `_2`, `_3`, ... when the table has a key of that name.
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
    let first_column = match columns.first().map(std::slice::from_ref) {
        Some(first) => match crate::ddl_index::translate_key_parts(first) {
            Ok(parts) => parts
                .into_iter()
                .next()
                .map(|part| part.name)
                .ok_or_else(unavailable)?,
            Err(_) => return Err(unavailable()),
        },
        None => return Err(unavailable()),
    };
    let table_sql = environment.table_sql(table_name.as_str())?;
    let base = first_column.as_str();
    let mut candidate = base.to_string();
    let mut suffix = 2;
    loop {
        if table_sql
            .as_deref()
            .is_some_and(|sql| crate::ddl_index::may_have_unique_key_named(sql, &candidate))
        {
            return Err(unavailable());
        }
        if !candidate.eq_ignore_ascii_case("PRIMARY")
            && crate::ddl_index::turso_index_name(table_name.as_str(), &candidate, environment)?
                .is_none()
        {
            return Ok(Ident::with_quote('`', candidate));
        }
        candidate = format!("{base}_{suffix}");
        suffix += 1;
    }
}

fn unsupported_operation(operation: &AlterTableOperation) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "ALTER TABLE operation `{operation}` is not available"
    ))
}

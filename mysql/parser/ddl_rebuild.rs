//! `ALTER TABLE` operations that core's `ALTER TABLE` cannot run, as a
//! rebuild of the table: a new table with the changed definition, the rows
//! of the table copied into it in their order, the table dropped and the new
//! one renamed after it, and the indexes of the table created again, all in
//! the transaction of the statement.

use crate::ddl::{ensure_key_has_no_storage_options, translate_foreign_key};
use crate::ddl_column::{translate_check, Column};
use crate::ddl_index::translate_key_parts;
use crate::expr::DEFAULT_COLLATION;
use crate::names::translate_ddl_identifier;
use crate::stored_table::TableDefinition;
use crate::{foreign_key, Environment, MysqlParserError};
use sqlparser::ast::{
    CheckConstraint, ColumnDef, ForeignKeyConstraint, MySQLColumnPosition, PrimaryKeyConstraint,
};
use turso_parser::ast;

/// The stored definition of a table, as the operation changes it.
pub(crate) struct Rebuild {
    table: ast::Name,
    columns: Vec<RebuiltColumn>,
    constraints: Vec<ast::NamedTableConstraint>,
    options: ast::TableOptions,
    indexes: Vec<ast::Stmt>,
}

struct RebuiltColumn {
    definition: ast::ColumnDefinition,
    /// The column of the table whose values the column takes, or `None` for
    /// a new column, which takes its default.
    source: Option<ast::Name>,
}

impl Rebuild {
    /// The definition of `table`, refused for a table whose rebuild would
    /// change more than its definition: one with an `AUTO_INCREMENT`
    /// counter, which the new table would not keep, with triggers, which
    /// the drop would drop, or that a foreign key references, whose rows
    /// the drop would delete.
    pub(crate) fn of(
        table: &ast::Name,
        environment: &dyn Environment,
    ) -> Result<Self, MysqlParserError> {
        let refused = |why: &str| {
            MysqlParserError::Unsupported(format!(
                "ALTER TABLE `{}`, which Turso runs as a rebuild of the table, {why} is not available",
                table.as_str()
            ))
        };
        let Some(sql) = environment.table_sql(table.as_str())? else {
            return Err(MysqlParserError::Rejected(format!(
                "Table '{}' doesn't exist",
                table.as_str()
            )));
        };
        let mut parser =
            turso_parser::parser::Parser::new(sql.as_bytes()).with_type_parameter_lists();
        let Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateTable {
            body:
                ast::CreateTableBody::ColumnsAndConstraints {
                    columns,
                    constraints,
                    options,
                },
            ..
        }))) = parser.next_cmd()
        else {
            return Err(refused("of a table without a column list"));
        };
        let facts = environment
            .columns_of(table.as_str())?
            .ok_or_else(|| refused("of a table Turso cannot read"))?;
        if facts.iter().any(|column| column.auto_increment) {
            return Err(refused(
                "of a table with an AUTO_INCREMENT counter, which the new table would not keep,",
            ));
        }
        if !environment.triggers_on(table.as_str())?.is_empty() {
            return Err(refused("of a table with triggers"));
        }
        if !environment.foreign_keys_to(table.as_str())?.is_empty() {
            return Err(refused("of a table a foreign key references"));
        }
        if columns.iter().any(|column| {
            ["rowid", "_rowid_", "oid"]
                .iter()
                .any(|name| column.col_name.as_str().eq_ignore_ascii_case(name))
        }) {
            return Err(refused("of a table with a column named like the rowid"));
        }
        Ok(Self {
            table: table.clone(),
            columns: columns
                .into_iter()
                .map(|definition| RebuiltColumn {
                    source: Some(definition.col_name.clone()),
                    definition,
                })
                .collect(),
            constraints,
            options,
            indexes: environment.index_definitions(table.as_str())?,
        })
    }

    fn definition(&self) -> TableDefinition {
        TableDefinition::from_body(
            &self
                .columns
                .iter()
                .map(|column| column.definition.clone())
                .collect::<Vec<_>>(),
            &self.constraints,
        )
    }

    fn position(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| {
            column
                .definition
                .col_name
                .as_str()
                .eq_ignore_ascii_case(name)
        })
    }

    fn ensure_column(&self, name: &ast::Name) -> Result<usize, MysqlParserError> {
        self.position(name.as_str()).ok_or_else(|| {
            MysqlParserError::Rejected(format!(
                "Key column '{}' doesn't exist in table",
                name.as_str()
            ))
        })
    }

    fn has_primary_key(&self) -> bool {
        self.columns.iter().any(|column| {
            column.definition.constraints.iter().any(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::PrimaryKey { .. }
                )
            })
        }) || self.constraints.iter().any(|constraint| {
            matches!(
                constraint.constraint,
                ast::TableConstraint::PrimaryKey { .. }
            )
        })
    }

    /// `ADD [CONSTRAINT name] CHECK (...)`, which MySQL checks against the
    /// rows of the table.
    pub(crate) fn add_check(&mut self, check: &CheckConstraint) -> Result<(), MysqlParserError> {
        let Some(expr) = translate_check(check)? else {
            return Err(MysqlParserError::Unsupported(
                "a NOT ENFORCED check added by ALTER TABLE is not available".to_string(),
            ));
        };
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
        Ok(())
    }

    /// `ADD [CONSTRAINT name] FOREIGN KEY (...) REFERENCES ...`, which MySQL
    /// checks against the rows of the table. A foreign key to the table
    /// itself is refused, as the drop of the table would delete the rows of
    /// the new one.
    pub(crate) fn add_foreign_key(
        &mut self,
        foreign_key: &ForeignKeyConstraint,
    ) -> Result<(), MysqlParserError> {
        let constraint = translate_foreign_key(foreign_key, &mut |name| {
            self.ensure_column(name).map(|_| ())
        })?;
        self.constraints.push(constraint);
        Ok(())
    }

    /// `ADD PRIMARY KEY (...)`, whose columns become `NOT NULL`.
    pub(crate) fn add_primary_key(
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
        if characteristics.is_some() {
            return Err(MysqlParserError::Unsupported(
                "PRIMARY KEY constraint characteristics are not MySQL syntax".to_string(),
            ));
        }
        ensure_key_has_no_storage_options(index_type.as_ref(), index_options)?;
        if self.has_primary_key() {
            return Err(MysqlParserError::Rejected(
                "Multiple primary key defined".to_string(),
            ));
        }
        let key_parts = translate_key_parts(key_columns)?;
        let definition = self.definition();
        let mut total_bytes = 0;
        for part in &key_parts {
            if part.prefix.is_some() {
                return Err(MysqlParserError::Unsupported(
                    "a primary key on a column prefix is not available".to_string(),
                ));
            }
            self.ensure_column(&part.name)?;
            let column_type = definition
                .column(part.name.as_str())
                .and_then(|column| column.column_type())
                .ok_or_else(|| {
                    MysqlParserError::Unsupported(format!(
                        "a primary key on column `{}`, whose type Turso cannot measure, is not available",
                        part.name.as_str()
                    ))
                })?;
            total_bytes += column_type.key_part_bytes(part.name.as_str(), None)?;
        }
        crate::ddl_type::ensure_key_length(total_bytes)?;
        for part in &key_parts {
            let position = self.ensure_column(&part.name)?;
            let constraints = &mut self.columns[position].definition.constraints;
            if constraints.iter().any(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::NotNull { nullable: true, .. }
                )
            }) {
                return Err(MysqlParserError::Rejected(
                    "All parts of a PRIMARY KEY must be NOT NULL; if you need NULL in a key, use UNIQUE instead".to_string(),
                ));
            }
            make_not_null(constraints);
        }
        self.constraints.push(ast::NamedTableConstraint {
            name: None,
            constraint: ast::TableConstraint::PrimaryKey {
                columns: key_parts.iter().map(|part| part.sorted_column()).collect(),
                auto_increment: false,
                conflict_clause: None,
            },
        });
        Ok(())
    }

    /// `DROP PRIMARY KEY`; its columns stay `NOT NULL`. Refused for a table
    /// with foreign keys, one of which may need the key as its index.
    pub(crate) fn drop_primary_key(&mut self) -> Result<(), MysqlParserError> {
        if !self.has_primary_key() {
            return Err(MysqlParserError::Rejected(
                "Can't DROP 'PRIMARY'; check that column/key exists".to_string(),
            ));
        }
        if self.constraints.iter().any(|constraint| {
            matches!(
                constraint.constraint,
                ast::TableConstraint::ForeignKey { .. }
            )
        }) {
            return Err(MysqlParserError::Unsupported(
                "DROP PRIMARY KEY of a table with foreign keys, which may need the key as their index, is not available".to_string(),
            ));
        }
        let mut key_columns = Vec::new();
        self.constraints.retain(|constraint| {
            let ast::TableConstraint::PrimaryKey { columns, .. } = &constraint.constraint else {
                return true;
            };
            key_columns.extend(
                columns
                    .iter()
                    .filter_map(|column| match column.expr.as_ref() {
                        ast::Expr::Id(name) | ast::Expr::Name(name) => {
                            Some(name.as_str().to_string())
                        }
                        _ => None,
                    }),
            );
            false
        });
        for column in &mut self.columns {
            let constraints = &mut column.definition.constraints;
            let was_key = constraints.iter().any(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::PrimaryKey { .. }
                )
            }) || key_columns
                .iter()
                .any(|key| key.eq_ignore_ascii_case(column.definition.col_name.as_str()));
            constraints.retain(|constraint| {
                !matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::PrimaryKey { .. }
                )
            });
            if was_key {
                make_not_null(constraints);
            }
        }
        Ok(())
    }

    /// `ADD [COLUMN] definition [FIRST | AFTER column]`.
    pub(crate) fn add_column(
        &mut self,
        column_def: &ColumnDef,
        position: Option<&MySQLColumnPosition>,
    ) -> Result<(), MysqlParserError> {
        let column = Column::from_mysql(column_def)?;
        if column.auto_increment {
            return Err(MysqlParserError::Unsupported(
                "adding an AUTO_INCREMENT column is not available".to_string(),
            ));
        }
        let definition = self.definition();
        let types = definition
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
        let definition = column.into_definition(DEFAULT_COLLATION)?;
        if definition.constraints.iter().any(|constraint| {
            matches!(&constraint.constraint, ast::ColumnConstraint::Default(expr) if reads_columns(expr))
        }) {
            return Err(MysqlParserError::Unsupported(format!(
                "column `{}` with a default that reads other columns, which core does not take, is not available",
                definition.col_name.as_str()
            )));
        }
        if self.position(definition.col_name.as_str()).is_some() {
            return Err(MysqlParserError::Rejected(format!(
                "Duplicate column name '{}'",
                definition.col_name.as_str()
            )));
        }
        if definition.constraints.iter().any(|constraint| {
            matches!(
                constraint.constraint,
                ast::ColumnConstraint::PrimaryKey { .. }
            )
        }) && self.has_primary_key()
        {
            return Err(MysqlParserError::Rejected(
                "Multiple primary key defined".to_string(),
            ));
        }
        let at = match position {
            None => self.columns.len(),
            Some(MySQLColumnPosition::First) => 0,
            Some(MySQLColumnPosition::After(after)) => {
                let after = translate_ddl_identifier(after)?;
                self.position(after.as_str()).ok_or_else(|| {
                    MysqlParserError::Rejected(format!(
                        "Unknown column '{}' in '{}'",
                        after.as_str(),
                        self.table.as_str()
                    ))
                })? + 1
            }
        };
        self.columns.insert(
            at,
            RebuiltColumn {
                definition,
                source: None,
            },
        );
        Ok(())
    }

    /// `MODIFY old definition` or `CHANGE old definition` (`renames`), whose
    /// column MySQL replaces by `column_def` and moves to `position`; a
    /// `MODIFY` keeps the name of the column as the table spells it. The keys and
    /// checks of the column stay, as in MySQL, so a column of the primary
    /// key stays `NOT NULL`. Renaming a column of a table with checks,
    /// generated columns or defaults that are expressions, which may read
    /// it, is refused, as is altering a generated column.
    pub(crate) fn modify_column(
        &mut self,
        old: &ast::Name,
        column_def: &ColumnDef,
        renames: bool,
        position: Option<&MySQLColumnPosition>,
        environment: &dyn Environment,
    ) -> Result<(), MysqlParserError> {
        let unavailable = |why: &str| {
            MysqlParserError::Unsupported(format!(
                "ALTER TABLE `{}` altering the column `{}` {why} is not available",
                self.table.as_str(),
                old.as_str()
            ))
        };
        let Some(at) = self.position(old.as_str()) else {
            return Err(MysqlParserError::Rejected(format!(
                "Unknown column '{}' in '{}'",
                old.as_str(),
                self.table.as_str()
            )));
        };
        let column = Column::from_mysql(column_def)?;
        if column.auto_increment {
            return Err(unavailable("to AUTO_INCREMENT"));
        }
        let is_generated = |definition: &ast::ColumnDefinition| {
            definition.constraints.iter().any(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::Generated { .. }
                )
            })
        };
        if is_generated(&self.columns[at].definition) {
            return Err(unavailable("that is generated"));
        }
        let stored = self.definition();
        if let Some(reason) = crate::ddl_alter::indexes_refusing_the_type(
            &self.table,
            &stored,
            old,
            &column.column_type,
            environment,
        )? {
            return Err(unavailable(reason));
        }
        for key in &stored.unique_keys {
            if !key
                .iter()
                .any(|name| name.eq_ignore_ascii_case(old.as_str()))
            {
                continue;
            }
            let mut total_bytes = 0;
            for name in key {
                total_bytes += if name.eq_ignore_ascii_case(old.as_str()) {
                    column.column_type.key_part_bytes(name, None)?
                } else {
                    stored
                        .column(name)
                        .and_then(|part| part.column_type())
                        .ok_or_else(|| unavailable("of a key whose types Turso cannot measure"))?
                        .key_part_bytes(name, None)?
                };
            }
            crate::ddl_type::ensure_key_length(total_bytes)?;
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
                .chain([Some(&column.column_type)]),
        )?;
        let explicit_null = column.explicit_null;
        let mut definition = column.into_definition(DEFAULT_COLLATION)?;
        if !renames {
            definition.col_name = self.columns[at].definition.col_name.clone();
        }
        if is_generated(&definition) {
            return Err(unavailable("to a generated column"));
        }
        if definition.constraints.iter().any(|constraint| {
            matches!(&constraint.constraint, ast::ColumnConstraint::Default(expr) if reads_columns(expr))
        }) {
            return Err(unavailable(
                "to a default that reads other columns, which core does not take,",
            ));
        }
        let new = definition.col_name.clone();
        let renamed = !new.as_str().eq_ignore_ascii_case(old.as_str());
        if renamed && self.position(new.as_str()).is_some() {
            return Err(MysqlParserError::Rejected(format!(
                "Duplicate column name '{}'",
                new.as_str()
            )));
        }
        if renamed
            && (self.constraints.iter().any(|constraint| {
                matches!(constraint.constraint, ast::TableConstraint::Check { .. })
            }) || self.columns.iter().any(|column| {
                column.definition.constraints.iter().any(|constraint| {
                    matches!(
                        &constraint.constraint,
                        ast::ColumnConstraint::Check { .. }
                            | ast::ColumnConstraint::Generated { .. }
                    ) || matches!(
                        &constraint.constraint,
                        ast::ColumnConstraint::Default(expr) if reads_columns(expr)
                    )
                })
            }))
        {
            return Err(unavailable(
                "to a new name, in a table whose checks, generated columns or defaults may read it,",
            ));
        }
        if definition.constraints.iter().any(|constraint| {
            matches!(
                constraint.constraint,
                ast::ColumnConstraint::PrimaryKey { .. }
            )
        }) && self.has_primary_key()
        {
            return Err(MysqlParserError::Rejected(
                "Multiple primary key defined".to_string(),
            ));
        }
        let old_constraints = std::mem::take(&mut self.columns[at].definition.constraints);
        let in_primary_key = old_constraints.iter().any(|constraint| {
            matches!(
                constraint.constraint,
                ast::ColumnConstraint::PrimaryKey { .. }
            )
        }) || self.constraints.iter().any(|constraint| {
            matches!(
                &constraint.constraint,
                ast::TableConstraint::PrimaryKey { columns, .. }
                    if columns.iter().any(|column| names_column(&column.expr, old))
            )
        });
        definition
            .constraints
            .extend(old_constraints.into_iter().filter(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::PrimaryKey { .. }
                        | ast::ColumnConstraint::Unique(_)
                        | ast::ColumnConstraint::Check { .. }
                        | ast::ColumnConstraint::ForeignKey { .. }
                )
            }));
        if in_primary_key {
            if explicit_null {
                return Err(MysqlParserError::Rejected(
                    "All parts of a PRIMARY KEY must be NOT NULL; if you need NULL in a key, use UNIQUE instead".to_string(),
                ));
            }
            make_not_null(&mut definition.constraints);
        }
        if renamed {
            for constraint in &mut self.constraints {
                match &mut constraint.constraint {
                    ast::TableConstraint::PrimaryKey { columns, .. }
                    | ast::TableConstraint::Unique { columns, .. } => {
                        for column in columns {
                            rename_column(&mut column.expr, old, &new);
                        }
                    }
                    ast::TableConstraint::ForeignKey { columns, .. } => {
                        for column in columns {
                            if column.col_name.as_str().eq_ignore_ascii_case(old.as_str()) {
                                column.col_name = new.clone();
                            }
                        }
                    }
                    ast::TableConstraint::Check { .. } => {}
                }
            }
            for index in &mut self.indexes {
                let ast::Stmt::CreateIndex { columns, .. } = index else {
                    unreachable!("the indexes of a rebuild are CREATE INDEX statements");
                };
                for column in columns {
                    rename_column(&mut column.expr, old, &new);
                }
            }
        }
        let source = self.columns.remove(at).source;
        let at = match position {
            None => at,
            Some(MySQLColumnPosition::First) => 0,
            Some(MySQLColumnPosition::After(after)) => {
                let after = translate_ddl_identifier(after)?;
                self.position(after.as_str()).ok_or_else(|| {
                    MysqlParserError::Rejected(format!(
                        "Unknown column '{}' in '{}'",
                        after.as_str(),
                        self.table.as_str()
                    ))
                })? + 1
            }
        };
        self.columns
            .insert(at, RebuiltColumn { definition, source });
        Ok(())
    }

    /// Replaces the definition of the column of the name of `definition`,
    /// which the table has.
    pub(crate) fn replace_column(&mut self, definition: ast::ColumnDefinition) {
        let at = self
            .position(definition.col_name.as_str())
            .expect("the column replaced is a column of the table");
        self.columns[at].definition = definition;
    }

    /// The statements of the rebuild.
    pub(crate) fn statements(
        self,
        environment: &dyn Environment,
    ) -> Result<Vec<ast::Stmt>, MysqlParserError> {
        let Self {
            table,
            columns,
            constraints,
            options,
            indexes,
        } = self;
        let new_table = (0..)
            .map(|n| format!("{}_turso_rebuilt_{n}", table.as_str()))
            .find_map(|candidate| {
                match (
                    environment.table_sql(&candidate),
                    environment.index_table(&candidate),
                ) {
                    (Ok(None), Ok(None)) => Some(Ok(candidate)),
                    (Err(error), _) | (_, Err(error)) => Some(Err(error)),
                    _ => None,
                }
            })
            .unwrap_or_else(|| unreachable!("a database has finitely many tables"))?;
        let new_table = ast::Name::exact(new_table);
        let copied = columns
            .iter()
            .filter(|column| {
                !column.definition.constraints.iter().any(|constraint| {
                    matches!(
                        constraint.constraint,
                        ast::ColumnConstraint::Generated { .. }
                    )
                })
            })
            .filter_map(|column| {
                column
                    .source
                    .as_ref()
                    .map(|source| (&column.definition.col_name, source))
            })
            .collect::<Vec<_>>();
        let copy = format!(
            "INSERT INTO {} ({}) SELECT {} FROM {} ORDER BY rowid",
            quoted(&new_table),
            copied
                .iter()
                .map(|(name, _)| quoted(name))
                .collect::<Vec<_>>()
                .join(", "),
            copied
                .iter()
                .map(|(_, source)| quoted(source))
                .collect::<Vec<_>>()
                .join(", "),
            quoted(&table)
        );
        let Ok(Some(ast::Cmd::Stmt(copy))) =
            turso_parser::parser::Parser::new(copy.as_bytes()).next_cmd()
        else {
            return Err(MysqlParserError::Unsupported(format!(
                "the copy `{copy}` of a rebuild does not parse"
            )));
        };
        let body = ast::CreateTableBody::ColumnsAndConstraints {
            columns: columns
                .into_iter()
                .map(|column| column.definition)
                .collect(),
            constraints,
            options,
        };
        foreign_key::check_foreign_keys(&table, &body, environment)?;
        if let ast::CreateTableBody::ColumnsAndConstraints { constraints, .. } = &body {
            if constraints.iter().any(|constraint| {
                matches!(
                    &constraint.constraint,
                    ast::TableConstraint::ForeignKey { clause, .. }
                        if clause.tbl_name.as_str().eq_ignore_ascii_case(table.as_str())
                )
            }) {
                return Err(MysqlParserError::Unsupported(format!(
                    "ALTER TABLE `{}`, which Turso runs as a rebuild of the table, of a table with a foreign key to itself, whose rows the drop of the table would delete, is not available",
                    table.as_str()
                )));
            }
        }
        let mut statements = vec![
            ast::Stmt::CreateTable {
                temporary: false,
                if_not_exists: false,
                tbl_name: ast::QualifiedName::single(new_table.clone()),
                body,
            },
            copy,
            ast::Stmt::DropTable {
                if_exists: false,
                tbl_name: ast::QualifiedName::single(table.clone()),
            },
            ast::Stmt::AlterTable(ast::AlterTable {
                name: ast::QualifiedName::single(new_table),
                body: ast::AlterTableBody::RenameTo(table),
            }),
        ];
        statements.extend(indexes);
        Ok(statements)
    }
}

/// Whether `expr`, a lowered column default, may read a column: the
/// expressions a default lowers to that do not are literals and calls and
/// operators of them.
pub(crate) fn reads_columns(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Literal(_) => false,
        ast::Expr::Parenthesized(items) => items.iter().any(|item| reads_columns(item)),
        ast::Expr::Unary(_, operand)
        | ast::Expr::IsNull(operand)
        | ast::Expr::NotNull(operand)
        | ast::Expr::Collate(operand, _)
        | ast::Expr::Cast { expr: operand, .. } => reads_columns(operand),
        ast::Expr::Binary(left, _, right) => reads_columns(left) || reads_columns(right),
        ast::Expr::FunctionCall { args, .. } => args.iter().any(|arg| reads_columns(arg)),
        _ => true,
    }
}

/// Whether `expr`, a key part, is the column `name`.
fn names_column(expr: &ast::Expr, name: &ast::Name) -> bool {
    match expr {
        ast::Expr::Id(column) | ast::Expr::Name(column) => {
            column.as_str().eq_ignore_ascii_case(name.as_str())
        }
        ast::Expr::Collate(operand, _) => names_column(operand, name),
        _ => false,
    }
}

/// Renames the column `old` of `expr`, a key part, to `new`.
fn rename_column(expr: &mut ast::Expr, old: &ast::Name, new: &ast::Name) {
    match expr {
        ast::Expr::Id(column) | ast::Expr::Name(column)
            if column.as_str().eq_ignore_ascii_case(old.as_str()) =>
        {
            *column = new.clone();
        }
        ast::Expr::Collate(operand, _) => rename_column(operand, old, new),
        _ => {}
    }
}

fn make_not_null(constraints: &mut Vec<ast::NamedColumnConstraint>) {
    if !constraints.iter().any(|constraint| {
        matches!(
            constraint.constraint,
            ast::ColumnConstraint::NotNull {
                nullable: false,
                ..
            }
        )
    }) {
        constraints.push(ast::NamedColumnConstraint {
            name: None,
            constraint: ast::ColumnConstraint::NotNull {
                nullable: false,
                conflict_clause: None,
            },
        });
    }
}

fn quoted(name: &ast::Name) -> String {
    format!("\"{}\"", name.as_str().replace('"', "\"\""))
}

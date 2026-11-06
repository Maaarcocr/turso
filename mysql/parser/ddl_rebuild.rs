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
fn reads_columns(expr: &ast::Expr) -> bool {
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

use crate::ddl_default::resolve_default;
use crate::ddl_expr::{translate_default, translate_expr, DefaultValue};
use crate::ddl_type::{translate_column_type, ColumnType, Family};
use crate::expr::{collation_sequence, DEFAULT_COLLATION};
use crate::names::translate_ddl_identifier;
use crate::MysqlParserError;
use sqlparser::ast::{
    CheckConstraint, ColumnDef, ColumnOption, ColumnOptionDef, DataType, GeneratedAs,
    GeneratedExpressionMode, ObjectName, ObjectNamePart, PrimaryKeyConstraint, UniqueConstraint,
};
use sqlparser::tokenizer::Token;
use turso_parser::ast;

pub(crate) struct Column {
    pub(crate) name: ast::Name,
    pub(crate) column_type: ColumnType,
    not_null: bool,
    explicit_null: bool,
    pub(crate) auto_increment: bool,
    pub(crate) primary_key: bool,
    pub(crate) unique: bool,
    pub(crate) in_key: bool,
    default: Option<DefaultValue>,
    checks: Vec<(Option<ast::Name>, ast::Expr)>,
    generated: Option<(ast::Expr, ast::GeneratedColumnType)>,
    collation: Option<String>,
    charset: bool,
}

impl Column {
    pub(crate) fn from_mysql(column: &ColumnDef) -> Result<Self, MysqlParserError> {
        let ColumnDef {
            name,
            data_type,
            options,
        } = column;
        let mut lowered = Column {
            name: translate_ddl_identifier(name)?,
            column_type: translate_column_type(data_type)?,
            not_null: false,
            explicit_null: false,
            auto_increment: false,
            primary_key: false,
            unique: false,
            in_key: false,
            default: None,
            checks: Vec::new(),
            generated: None,
            collation: None,
            charset: false,
        };
        for option in options {
            lowered.apply_option(option, data_type)?;
        }
        let rejected = (lowered.explicit_null && lowered.primary_key)
            || (lowered.auto_increment
                && (lowered.default.is_some() || lowered.generated.is_some()))
            || (lowered.generated.is_some() && lowered.default.is_some());
        if rejected {
            return Err(MysqlParserError::Rejected(format!(
                "column `{}` combines options that MySQL rejects",
                lowered.name.as_str()
            )));
        }
        if lowered.explicit_null && lowered.not_null {
            return Err(MysqlParserError::Unsupported(format!(
                "column `{}` is declared both NULL and NOT NULL",
                lowered.name.as_str()
            )));
        }
        Ok(lowered)
    }

    pub(crate) fn has_name(&self, name: &ast::Name) -> bool {
        self.name.as_str().eq_ignore_ascii_case(name.as_str())
    }

    pub(crate) fn mark_primary_key_member(&mut self) -> Result<(), MysqlParserError> {
        if self.explicit_null {
            return Err(MysqlParserError::Rejected(format!(
                "primary key column `{}` cannot be declared NULL",
                self.name.as_str()
            )));
        }
        self.not_null = true;
        Ok(())
    }

    pub(crate) fn into_definition(
        self,
        table_collation: &str,
    ) -> Result<ast::ColumnDefinition, MysqlParserError> {
        let Column {
            name,
            column_type,
            not_null,
            explicit_null: _,
            auto_increment,
            primary_key,
            unique,
            in_key,
            default,
            checks,
            generated,
            collation,
            charset,
        } = self;
        if auto_increment && !primary_key && !unique && !in_key {
            return Err(MysqlParserError::Rejected(format!(
                "AUTO_INCREMENT column `{}` must be part of a key",
                name.as_str()
            )));
        }
        if auto_increment && !primary_key {
            return Err(MysqlParserError::Unsupported(format!(
                "AUTO_INCREMENT column `{}` must be the whole primary key",
                name.as_str()
            )));
        }
        let not_null = not_null || primary_key;
        let mut constraints = Vec::new();
        if primary_key {
            constraints.push(constraint(ast::ColumnConstraint::PrimaryKey {
                order: None,
                conflict_clause: None,
                auto_increment,
            }));
        }
        if not_null {
            constraints.push(constraint(ast::ColumnConstraint::NotNull {
                nullable: false,
                conflict_clause: None,
            }));
        }
        if unique {
            constraints.push(constraint(ast::ColumnConstraint::Unique(None)));
        }
        let default = default.or_else(|| match &column_type.family {
            Family::Enum(members) if not_null => members.first().cloned().map(DefaultValue::Text),
            _ => None,
        });
        if let Some(default) = default {
            let default = resolve_default(&column_type, default, !not_null)?;
            constraints.push(constraint(ast::ColumnConstraint::Default(Box::new(
                default,
            ))));
        }
        if matches!(
            column_type.family,
            Family::Char { .. } | Family::Varchar { .. } | Family::Text
        ) {
            let collation = match (&collation, charset) {
                (Some(collation), _) => collation.as_str(),
                (None, true) => DEFAULT_COLLATION,
                (None, false) => table_collation,
            };
            let sequence = collation_sequence(collation).ok_or_else(|| {
                MysqlParserError::Unsupported(format!("collation `{collation}` is not available"))
            })?;
            constraints.push(constraint(ast::ColumnConstraint::Collate {
                collation_name: ast::Name::exact(sequence.to_string()),
            }));
        }
        if let Some(value_check) = column_type.value_check(&name) {
            constraints.push(check(value_check));
        }
        for (check_name, expr) in checks {
            constraints.push(ast::NamedColumnConstraint {
                name: check_name,
                constraint: ast::ColumnConstraint::Check {
                    expr: Box::new(expr),
                    source: None,
                },
            });
        }
        if let Some((expr, typ)) = generated {
            constraints.push(constraint(ast::ColumnConstraint::Generated {
                generated_always: true,
                expr: Box::new(expr),
                typ: Some(typ),
            }));
        }
        Ok(ast::ColumnDefinition {
            col_name: name,
            col_type: Some(column_type.declared),
            constraints,
        })
    }

    fn apply_option(
        &mut self,
        option: &ColumnOptionDef,
        data_type: &DataType,
    ) -> Result<(), MysqlParserError> {
        let ColumnOptionDef { name, option } = option;
        if name.is_some() && !matches!(option, ColumnOption::Check(_)) {
            return Err(MysqlParserError::Unsupported(format!(
                "named column option `{option}` is not available"
            )));
        }
        match option {
            ColumnOption::Null => self.explicit_null = true,
            ColumnOption::NotNull => self.not_null = true,
            ColumnOption::Default(expr) if self.default.is_none() => {
                self.default = Some(translate_default(expr)?);
            }
            ColumnOption::PrimaryKey(primary_key) if !self.primary_key => {
                ensure_plain_primary_key(primary_key)?;
                self.primary_key = true;
            }
            ColumnOption::Unique(unique) => {
                ensure_plain_unique(unique)?;
                self.unique = true;
            }
            ColumnOption::ForeignKey(_) => {}
            ColumnOption::Check(check) => {
                if let Some(check) = translate_check(check)? {
                    let name = name.as_ref().map(translate_ddl_identifier).transpose()?;
                    self.checks.push((name, check));
                }
            }
            ColumnOption::DialectSpecific(tokens) if is_auto_increment(tokens) => {
                if !self.column_type.is_integer() {
                    return Err(MysqlParserError::Unsupported(format!(
                        "AUTO_INCREMENT requires an integer column, found `{data_type}`"
                    )));
                }
                self.auto_increment = true;
            }
            ColumnOption::CharacterSet(charset) if self.column_type.is_character_string() => {
                ensure_supported_charset(charset)?;
                self.charset = true;
            }
            ColumnOption::Collation(collation) if !self.column_type.is_binary() => {
                self.collation = Some(supported_collation(collation)?);
            }
            ColumnOption::Collation(collation) => {
                return Err(MysqlParserError::Rejected(format!(
                    "collation `{collation}` is not valid for a binary string column"
                )));
            }
            ColumnOption::Comment(_) => {}
            ColumnOption::Generated {
                generated_as,
                sequence_options: None,
                generation_expr: Some(expr),
                generation_expr_mode,
                generated_keyword: _,
            } if self.generated.is_none() => {
                let typ = match (generated_as, generation_expr_mode) {
                    (GeneratedAs::ExpStored, Some(GeneratedExpressionMode::Stored)) => {
                        ast::GeneratedColumnType::Stored
                    }
                    (GeneratedAs::Always, Some(GeneratedExpressionMode::Virtual) | None) => {
                        ast::GeneratedColumnType::Virtual
                    }
                    _ => return Err(unsupported_option(option)),
                };
                self.generated = Some((translate_expr(expr)?, typ));
            }
            _ => return Err(unsupported_option(option)),
        }
        Ok(())
    }
}

pub(crate) fn translate_check(
    check: &CheckConstraint,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    let CheckConstraint {
        name: _,
        expr,
        enforced,
    } = check;
    match enforced {
        Some(false) => Ok(None),
        Some(true) | None => translate_expr(expr).map(Some),
    }
}

pub(crate) fn ensure_supported_charset(charset: &ObjectName) -> Result<(), MysqlParserError> {
    match single_word(charset).as_deref() {
        Some("utf8mb4") => Ok(()),
        _ => Err(MysqlParserError::Unsupported(format!(
            "character set `{charset}` differs from the utf8mb4 server default"
        ))),
    }
}

pub(crate) fn supported_collation(collation: &ObjectName) -> Result<String, MysqlParserError> {
    match single_word(collation) {
        Some(name) if name != "binary" && collation_sequence(&name).is_some() => Ok(name),
        _ => Err(MysqlParserError::Unsupported(format!(
            "collation `{collation}` is not available"
        ))),
    }
}

fn single_word(name: &ObjectName) -> Option<String> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(identifier)] => Some(identifier.value.to_ascii_lowercase()),
        _ => None,
    }
}

fn ensure_plain_primary_key(primary_key: &PrimaryKeyConstraint) -> Result<(), MysqlParserError> {
    let PrimaryKeyConstraint {
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
    {
        return Err(MysqlParserError::Unsupported(
            "inline PRIMARY KEY with names or options is not available".to_string(),
        ));
    }
    Ok(())
}

fn ensure_plain_unique(unique: &UniqueConstraint) -> Result<(), MysqlParserError> {
    let UniqueConstraint {
        name,
        index_name,
        index_type_display: _,
        index_type,
        columns,
        index_options,
        characteristics,
        nulls_distinct: _,
    } = unique;
    if name.is_some()
        || index_name.is_some()
        || index_type.is_some()
        || !columns.is_empty()
        || !index_options.is_empty()
        || characteristics.is_some()
    {
        return Err(MysqlParserError::Unsupported(
            "inline UNIQUE with names or options is not available".to_string(),
        ));
    }
    Ok(())
}

fn is_auto_increment(tokens: &[Token]) -> bool {
    matches!(tokens, [Token::Word(word)] if word.value.eq_ignore_ascii_case("AUTO_INCREMENT"))
}

fn constraint(constraint: ast::ColumnConstraint) -> ast::NamedColumnConstraint {
    ast::NamedColumnConstraint {
        name: None,
        constraint,
    }
}

fn check(expr: ast::Expr) -> ast::NamedColumnConstraint {
    constraint(ast::ColumnConstraint::Check {
        expr: Box::new(expr),
        source: None,
    })
}

fn unsupported_option(option: &ColumnOption) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("column option `{option}` is not available"))
}

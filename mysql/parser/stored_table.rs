//! The columns and keys of a table as its stored `CREATE TABLE` (or a
//! lowered one) declares them.

use crate::MysqlParserError;
use turso_parser::ast;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    /// `CHAR` and `VARCHAR`, whose lengths may differ.
    String,
    /// Integer types, whose size and sign must be the same.
    Integer,
    Blob,
    Json,
    Other,
}

#[derive(Clone)]
pub(crate) struct ColumnDefinition {
    pub(crate) name: String,
    /// The type as written, in upper case without spaces (`VARCHAR(20)`,
    /// `INTUNSIGNED`), with no display width for integer types and `INT` for
    /// `INTEGER`.
    pub(crate) type_text: String,
    pub(crate) collation: Option<String>,
    pub(crate) not_null: bool,
    pub(crate) default: Option<ast::Expr>,
    pub(crate) generated: bool,
    /// The members of an `ENUM` or `SET` column, as declared.
    pub(crate) members: Option<Vec<String>>,
}

impl ColumnDefinition {
    fn new(definition: &ast::ColumnDefinition) -> Self {
        let declared = definition
            .col_type
            .as_ref()
            .map(|col_type| col_type.to_string())
            .unwrap_or_default();
        let written = declared.to_ascii_uppercase();
        let base = written
            .split(|c: char| c == '(' || c.is_whitespace())
            .next()
            .unwrap_or_default();
        let type_text = if is_integer(base) {
            let mut depth = 0;
            written
                .chars()
                .filter(|c| {
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            return false;
                        }
                        _ => {}
                    }
                    depth == 0 && !c.is_whitespace()
                })
                .collect::<String>()
                .replacen("INTEGER", "INT", 1)
        } else {
            written.split_whitespace().collect()
        };
        let mut collation = None;
        let mut not_null = false;
        let mut default = None;
        let mut generated = false;
        for constraint in &definition.constraints {
            match &constraint.constraint {
                ast::ColumnConstraint::Default(expr) => default = Some(expr.as_ref().clone()),
                ast::ColumnConstraint::Generated { .. } => generated = true,
                ast::ColumnConstraint::Collate { collation_name } => {
                    collation = Some(collation_name.as_str().to_string());
                }
                ast::ColumnConstraint::NotNull { nullable, .. } => not_null = !nullable,
                ast::ColumnConstraint::PrimaryKey { .. } => not_null = true,
                _ => {}
            }
        }
        Self {
            name: definition.col_name.as_str().to_string(),
            type_text,
            collation,
            not_null,
            default,
            generated,
            members: matches!(
                base,
                "ENUM" | "ENUM_EXACT" | "SET" | "\"SET\"" | "SET_EXACT"
            )
            .then(|| members(definition.col_type.as_ref()?))
            .flatten(),
        }
    }

    pub(crate) fn base_type(&self) -> &str {
        self.type_text
            .split_once('(')
            .map_or(self.type_text.as_str(), |(base, _)| base)
    }

    pub(crate) fn family(&self) -> Family {
        match self.base_type() {
            "CHAR" | "VARCHAR" => Family::String,
            "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "TINYBLOB" | "BLOB"
            | "MEDIUMBLOB" | "LONGBLOB" => Family::Blob,
            "JSON" => Family::Json,
            base if base.starts_with("TINYINT")
                || base.starts_with("SMALLINT")
                || base.starts_with("MEDIUMINT")
                || base.starts_with("INT")
                || base.starts_with("BIGINT") =>
            {
                Family::Integer
            }
            _ => Family::Other,
        }
    }
}

/// The members of a stored `ENUM('a','b')` or `"SET"(...)` type, one string
/// literal per parameter.
fn members(col_type: &ast::Type) -> Option<Vec<String>> {
    col_type
        .size
        .as_ref()?
        .parameters()
        .iter()
        .map(|parameter| match parameter.as_ref() {
            ast::Expr::Literal(ast::Literal::String(literal)) => unquote(literal),
            _ => None,
        })
        .collect()
}

/// The text of a whole SQL string literal.
fn unquote(literal: &str) -> Option<String> {
    let (text, rest) = quoted_prefix(literal)?;
    rest.is_empty().then_some(text)
}

/// The text of the SQL string literal at the start of `text`, and what
/// follows it.
fn quoted_prefix(text: &str) -> Option<(String, &str)> {
    let mut chars = text.char_indices();
    if chars.next()?.1 != '\'' {
        return None;
    }
    let mut value = String::new();
    while let Some((index, c)) = chars.next() {
        if c != '\'' {
            value.push(c);
            continue;
        }
        if text[index + 1..].starts_with('\'') {
            chars.next();
            value.push('\'');
        } else {
            return Some((value, &text[index + 1..]));
        }
    }
    None
}

fn is_integer(base: &str) -> bool {
    matches!(
        base,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT"
    )
}

/// The columns and unique keys of a `CREATE TABLE`.
#[derive(Clone)]
pub(crate) struct TableDefinition {
    pub(crate) columns: Vec<ColumnDefinition>,
    pub(crate) unique_keys: Vec<Vec<String>>,
}

impl TableDefinition {
    pub(crate) fn parse(sql: &str) -> Result<Self, MysqlParserError> {
        let mut parser =
            turso_parser::parser::Parser::new(sql.as_bytes()).with_type_parameter_lists();
        match parser.next_cmd() {
            Ok(Some(ast::Cmd::Stmt(ast::Stmt::CreateTable {
                body:
                    ast::CreateTableBody::ColumnsAndConstraints {
                        columns,
                        constraints,
                        ..
                    },
                ..
            }))) => Ok(Self::from_body(&columns, &constraints)),
            _ => Err(MysqlParserError::Unsupported(format!(
                "a foreign key to the table of `{sql}` is not lowered yet"
            ))),
        }
    }

    pub(crate) fn from_body(
        columns: &[ast::ColumnDefinition],
        constraints: &[ast::NamedTableConstraint],
    ) -> Self {
        let mut unique_keys = Vec::new();
        for column in columns {
            if column.constraints.iter().any(|constraint| {
                matches!(
                    constraint.constraint,
                    ast::ColumnConstraint::PrimaryKey { .. } | ast::ColumnConstraint::Unique(_)
                )
            }) {
                unique_keys.push(vec![column.col_name.as_str().to_lowercase()]);
            }
        }
        for constraint in constraints {
            let key_columns = match &constraint.constraint {
                ast::TableConstraint::PrimaryKey { columns, .. }
                | ast::TableConstraint::Unique { columns, .. } => columns,
                _ => continue,
            };
            let key = key_columns
                .iter()
                .map(|column| match column.expr.as_ref() {
                    ast::Expr::Id(name) => Some(name.as_str().to_lowercase()),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            if let Some(key) = key {
                unique_keys.push(key);
            }
        }
        Self {
            columns: columns.iter().map(ColumnDefinition::new).collect(),
            unique_keys,
        }
    }

    pub(crate) fn column(&self, name: &str) -> Option<&ColumnDefinition> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }
}

//! `JSON_TABLE(document, path COLUMNS (...)) AS t`: a derived table of the
//! rows `__mysql_json_table(document, spec)` computes as MySQL does, one JSON
//! array per row, each column read from its row by
//! `__mysql_json_table_cell(row, index)`. The table is `LATERAL` when the
//! document reads a column, as MySQL lets it read the tables before it.

use crate::ddl_type::{translate_column_type, Family};
use crate::expr::{function_call, string_expr, translate_expr};
use crate::names::translate_identifier;
use crate::query::TranslationContext;
use crate::MysqlParserError;
use sqlparser::ast::{
    Expr, JsonTableColumn, JsonTableColumnErrorHandling, JsonTableNamedColumn, TableAlias, Value,
    ValueWithSpan,
};
use turso_parser::ast;

const ROW: &str = "__mysql_json_table_row";

pub(crate) fn translate_json_table(
    document: &Expr,
    path: &ValueWithSpan,
    columns: &[JsonTableColumn],
    alias: Option<&TableAlias>,
    context: &mut TranslationContext,
) -> Result<ast::SelectTable, MysqlParserError> {
    let Some(alias) = alias else {
        return Err(MysqlParserError::Rejected(
            "ERROR 3667 (42000): Every table function must have an alias".to_string(),
        ));
    };
    check_document(document, context)?;
    if !alias.columns.is_empty() {
        return Err(unsupported(format!(
            "JSON_TABLE alias with columns `{alias}`"
        )));
    }
    let mut table = Table::default();
    let mut spec = String::from("{\"path\": ");
    spec.push_str(&json_string(literal_text(path)?));
    spec.push_str(", \"columns\": ");
    table.write_columns(columns, &mut spec)?;
    spec.push('}');
    let row = || {
        ast::Expr::Qualified(
            ast::Name::exact(ROW.to_string()),
            ast::Name::exact("value".to_string()),
        )
    };
    let mut result_columns = Vec::with_capacity(table.names.len());
    for (index, (name, text)) in table.names.iter().zip(&table.texts).enumerate() {
        let cell = function_call(
            "__mysql_json_table_cell",
            vec![
                row(),
                ast::Expr::Literal(ast::Literal::Numeric(index.to_string())),
            ],
        );
        let cell = if *text {
            ast::Expr::collate(cell, ast::Name::exact("und-u-ks-level1".to_string()))
        } else {
            cell
        };
        result_columns.push(ast::ResultColumn::Expr(
            Box::new(cell),
            Some(ast::As::As(translate_identifier(name)?)),
        ));
    }
    let lateral = reads_a_column(document);
    let rows = function_call(
        "__mysql_json_table",
        vec![translate_expr(document, context)?, string_expr(&spec)],
    );
    let select = ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns: result_columns,
                from: Some(ast::FromClause {
                    select: Box::new(ast::SelectTable::TableCall(
                        ast::QualifiedName::single(ast::Name::exact("json_each".to_string())),
                        vec![Box::new(rows)],
                        Some(ast::As::As(ast::Name::exact(ROW.to_string()))),
                    )),
                    joins: Vec::new(),
                }),
                where_clause: None,
                group_by: None,
                window_clause: Vec::new(),
            },
            compounds: Vec::new(),
        },
        order_by: Vec::new(),
        limit: None,
    };
    Ok(ast::SelectTable::Select {
        select,
        alias: Some(ast::As::As(translate_identifier(&alias.name)?)),
        lateral,
    })
}

/// The output columns of a `JSON_TABLE`, in order, nested columns in place.
#[derive(Default)]
struct Table {
    names: Vec<sqlparser::ast::Ident>,
    /// Whether each column is a string, which compares with the default
    /// collation.
    texts: Vec<bool>,
}

impl Table {
    fn add(&mut self, name: &sqlparser::ast::Ident, text: bool) -> Result<(), MysqlParserError> {
        if self
            .names
            .iter()
            .any(|earlier| earlier.value.eq_ignore_ascii_case(&name.value))
        {
            return Err(MysqlParserError::Rejected(format!(
                "ERROR 1060 (42S21): Duplicate column name '{}'",
                name.value
            )));
        }
        self.names.push(name.clone());
        self.texts.push(text);
        Ok(())
    }

    fn write_columns(
        &mut self,
        columns: &[JsonTableColumn],
        spec: &mut String,
    ) -> Result<(), MysqlParserError> {
        spec.push('[');
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                spec.push_str(", ");
            }
            match column {
                JsonTableColumn::ForOrdinality(name) => {
                    self.add(name, false)?;
                    spec.push_str("{\"ordinality\": true}");
                }
                JsonTableColumn::Named(named) => self.write_named(named, spec)?,
                JsonTableColumn::Nested(nested) => {
                    spec.push_str("{\"nested\": ");
                    spec.push_str(&json_string(literal_text(&nested.path)?));
                    spec.push_str(", \"columns\": ");
                    self.write_columns(&nested.columns, spec)?;
                    spec.push('}');
                }
            }
        }
        spec.push(']');
        Ok(())
    }

    fn write_named(
        &mut self,
        column: &JsonTableNamedColumn,
        spec: &mut String,
    ) -> Result<(), MysqlParserError> {
        let (kind, text) = column_kind(column)?;
        let name = json_string(&column.name.value);
        self.add(&column.name, text)?;
        let path = json_string(literal_text(&column.path)?);
        if column.exists {
            if column.on_empty.is_some() || column.on_error.is_some() {
                return Err(MysqlParserError::Rejected(format!(
                    "EXISTS PATH column `{}` with ON EMPTY or ON ERROR",
                    column.name
                )));
            }
            spec.push_str(&format!("{{\"exists\": {path}, \"type\": {kind}}}"));
            return Ok(());
        }
        spec.push_str(&format!(
            "{{\"name\": {name}, \"path\": {path}, \"type\": {kind}, \"empty\": {}, \"error\": {}}}",
            handling(column.on_empty.as_ref())?,
            handling(column.on_error.as_ref())?
        ));
        Ok(())
    }
}

/// The spec of a column's type, and whether it is a string type.
fn column_kind(column: &JsonTableNamedColumn) -> Result<(String, bool), MysqlParserError> {
    let column_type = translate_column_type(&column.r#type)?;
    let refused = || {
        unsupported(format!(
            "JSON_TABLE column `{}` of type {}",
            column.name, column.r#type
        ))
    };
    Ok(match column_type.family {
        Family::Integer { min, max } if max <= i128::from(i64::MAX) => (
            format!("{{\"kind\": \"integer\", \"min\": {min}, \"max\": {max}}}"),
            false,
        ),
        Family::Float {
            unsigned: false,
            digits: None,
        } => {
            let kind = if column_type.declared.name.eq_ignore_ascii_case("FLOAT") {
                "float"
            } else {
                "double"
            };
            (format!("{{\"kind\": \"{kind}\"}}"), false)
        }
        Family::Char { length } | Family::Varchar { length } => (
            format!("{{\"kind\": \"text\", \"length\": {length}}}"),
            true,
        ),
        Family::Json => ("{\"kind\": \"json\"}".to_string(), false),
        _ => return Err(refused()),
    })
}

fn handling(handling: Option<&JsonTableColumnErrorHandling>) -> Result<String, MysqlParserError> {
    Ok(match handling {
        None | Some(JsonTableColumnErrorHandling::Null) => "\"null\"".to_string(),
        Some(JsonTableColumnErrorHandling::Error) => "\"error\"".to_string(),
        Some(JsonTableColumnErrorHandling::Default(value)) => {
            format!("{{\"default\": {}}}", json_string(literal_text(value)?))
        }
    })
}

fn literal_text(value: &ValueWithSpan) -> Result<&str, MysqlParserError> {
    match &value.value {
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Ok(text),
        other => Err(unsupported(format!("JSON_TABLE path or default `{other}`"))),
    }
}

/// MySQL refuses a document that is a subquery or reads a common table
/// expression (1210). Refused: a document with a subquery elsewhere, or
/// reading a column that may be a derived table's or a common table
/// expression's.
fn check_document(document: &Expr, context: &TranslationContext) -> Result<(), MysqlParserError> {
    let incorrect = || {
        MysqlParserError::Rejected(
            "ERROR 1210 (HY000): Incorrect arguments to JSON_TABLE".to_string(),
        )
    };
    let mut bare = document;
    while let Expr::Nested(inner) = bare {
        bare = inner;
    }
    if matches!(bare, Expr::Subquery(_)) {
        return Err(incorrect());
    }
    if !reads_a_column(document) {
        return Ok(());
    }
    if document.to_string().to_ascii_lowercase().contains("select") {
        return Err(unsupported(format!(
            "JSON_TABLE document `{document}`, which may hold a subquery MySQL refuses,"
        )));
    }
    if context.common_tables.is_empty() && context.derived_tables.is_empty() {
        return Ok(());
    }
    let named = |names: &[String], qualifier: &str| {
        names
            .iter()
            .any(|name| name.eq_ignore_ascii_case(qualifier))
    };
    match document {
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let qualifier = &parts[0].value;
            if named(&context.common_tables, qualifier) {
                Err(incorrect())
            } else if named(&context.derived_tables, qualifier) {
                Err(unsupported(format!(
                    "JSON_TABLE document `{document}` read from a derived table"
                )))
            } else {
                Ok(())
            }
        }
        _ => Err(unsupported(format!(
            "JSON_TABLE document `{document}` beside a derived table or common table expression"
        ))),
    }
}

/// Whether `expr` may read a column of a table before the `JSON_TABLE`.
fn reads_a_column(expr: &Expr) -> bool {
    !matches!(
        expr,
        Expr::Value(_) | Expr::TypedString(_) | Expr::Prefixed { .. }
    )
}

fn json_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for ch in text.chars() {
        match ch {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            ch if u32::from(ch) < 0x20 => quoted.push_str(&format!("\\u{:04x}", u32::from(ch))),
            ch => quoted.push(ch),
        }
    }
    quoted.push('"');
    quoted
}

fn unsupported(what: String) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

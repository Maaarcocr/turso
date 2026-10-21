use crate::MysqlParserError;
use sqlparser::ast::{Ident, ObjectName, ObjectNamePart};
use turso_parser::ast;

pub(crate) fn translate_unqualified_name(name: &ObjectName) -> Result<ast::Name, MysqlParserError> {
    let [ObjectNamePart::Identifier(identifier)] = name.0.as_slice() else {
        return Err(MysqlParserError::Unsupported(
            "CREATE TABLE requires one unqualified identifier".to_string(),
        ));
    };
    translate_identifier(identifier)
}

pub(crate) fn translate_identifier(identifier: &Ident) -> Result<ast::Name, MysqlParserError> {
    match identifier.quote_style {
        None | Some('`') => Ok(ast::Name::from_string(identifier.to_string())),
        Some(quote) => Err(MysqlParserError::Unsupported(format!(
            "identifier quote style `{quote}` cannot be preserved faithfully"
        ))),
    }
}

pub(crate) fn quote_string(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

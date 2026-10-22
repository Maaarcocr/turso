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

const MAX_IDENTIFIER_LENGTH: usize = 64;

pub(crate) fn translate_ddl_name(name: &ObjectName) -> Result<ast::Name, MysqlParserError> {
    let translated = translate_unqualified_name(name)?;
    ensure_identifier_length(&translated)?;
    Ok(translated)
}

pub(crate) fn translate_ddl_identifier(identifier: &Ident) -> Result<ast::Name, MysqlParserError> {
    let translated = translate_identifier(identifier)?;
    ensure_identifier_length(&translated)?;
    Ok(translated)
}

fn ensure_identifier_length(name: &ast::Name) -> Result<(), MysqlParserError> {
    if name.as_str().chars().count() > MAX_IDENTIFIER_LENGTH {
        return Err(MysqlParserError::Rejected(format!(
            "identifier `{}` is longer than {MAX_IDENTIFIER_LENGTH} characters",
            name.as_str()
        )));
    }
    Ok(())
}

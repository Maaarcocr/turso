//! `PREPARE`, `EXECUTE` and `DEALLOCATE PREPARE`. The session keeps each
//! prepared statement's text by name: `PREPARE` checks and stores the text
//! when it runs, and `EXECUTE` lowers the stored text with each `?` reading
//! the user variable of its `USING` list.

use crate::dialect::tokens;
use crate::expr::{function_call, string_expr};
use crate::session::{statement_with_effect, user_variable_name};
use crate::{Environment, MysqlParserError};
use sqlparser::tokenizer::{Location, Token};
use turso_parser::ast;

pub(crate) enum Command {
    Prepare {
        name: String,
        source: Source,
    },
    Execute {
        name: String,
        variables: Vec<String>,
    },
    Deallocate {
        name: String,
    },
}

pub(crate) enum Source {
    Text(String),
    Variable(String),
}

/// The prepared-statement command `sql` is, or `None` when it is another
/// statement.
pub(crate) fn command(sql: &str) -> Result<Option<Command>, MysqlParserError> {
    let tokens = tokens(sql)?;
    let mut words: Vec<&Token> = tokens
        .iter()
        .map(|token| &token.token)
        .filter(|token| !matches!(token, Token::Whitespace(_) | Token::EOF))
        .collect();
    while words.last() == Some(&&Token::SemiColon) {
        words.pop();
    }
    let keyword = |token: &Token, keyword: &str| {
        matches!(token, Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(keyword))
    };
    let not_lowered =
        || MysqlParserError::Unsupported(format!("`{}` is not lowered yet", sql.trim()));
    let Some(first) = words.first() else {
        return Ok(None);
    };
    let command = if keyword(first, "PREPARE") {
        let [_, name, from, source] = words.as_slice() else {
            return Err(not_lowered());
        };
        if !keyword(from, "FROM") {
            return Err(not_lowered());
        }
        let source = match source {
            Token::SingleQuotedString(text) | Token::DoubleQuotedString(text) => {
                Source::Text(text.clone())
            }
            Token::Word(word) if word.quote_style.is_none() => {
                Source::Variable(user_variable_name(&word.value).ok_or_else(not_lowered)?)
            }
            _ => return Err(not_lowered()),
        };
        Command::Prepare {
            name: statement_name(name).ok_or_else(not_lowered)?,
            source,
        }
    } else if keyword(first, "EXECUTE") {
        let (name, variables) = match words.as_slice() {
            [_, name] => (name, &[][..]),
            [_, name, using, variables @ ..]
                if keyword(using, "USING") && variables.len() % 2 == 1 =>
            {
                (name, variables)
            }
            _ => return Err(not_lowered()),
        };
        let mut names = Vec::new();
        for (index, token) in variables.iter().enumerate() {
            match token {
                Token::Comma if index % 2 == 1 => {}
                Token::Word(word)
                    if index % 2 == 0
                        && word.quote_style.is_none()
                        && user_variable_name(&word.value).is_some() =>
                {
                    names.push(word.value.clone());
                }
                _ => return Err(not_lowered()),
            }
        }
        Command::Execute {
            name: statement_name(name).ok_or_else(not_lowered)?,
            variables: names,
        }
    } else if keyword(first, "DEALLOCATE") || keyword(first, "DROP") {
        match words.as_slice() {
            [_, prepare, name] if keyword(prepare, "PREPARE") => Command::Deallocate {
                name: statement_name(name).ok_or_else(not_lowered)?,
            },
            _ if keyword(first, "DROP") => return Ok(None),
            _ => return Err(not_lowered()),
        }
    } else {
        return Ok(None);
    };
    Ok(Some(command))
}

/// The name of a prepared statement, which MySQL matches without regard to
/// case.
fn statement_name(token: &Token) -> Option<String> {
    match token {
        Token::Word(word) if matches!(word.quote_style, None | Some('`')) => {
            Some(word.value.to_lowercase())
        }
        _ => None,
    }
}

pub(crate) fn translate(
    command: Command,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    match command {
        Command::Prepare { name, source } => {
            let text = match source {
                Source::Text(text) => string_expr(&text),
                Source::Variable(variable) => {
                    function_call("__mysql_user_variable_value", vec![string_expr(&variable)])
                }
            };
            Ok(statement_with_effect(function_call(
                "__mysql_prepare",
                vec![string_expr(&name), text],
            )))
        }
        Command::Deallocate { name } => Ok(statement_with_effect(function_call(
            "__mysql_deallocate_prepare",
            vec![string_expr(&name)],
        ))),
        Command::Execute { name, variables } => {
            let text = environment.prepared_statement(&name).ok_or_else(|| {
                MysqlParserError::Rejected(format!(
                    "ERROR 1243 (HY000): Unknown prepared statement handler ({name}) given to EXECUTE"
                ))
            })?;
            let sql = with_parameters(&text, &variables)?;
            crate::translate_text(&sql, environment)
        }
    }
}

/// Lowers `text` for `PREPARE`, as `EXECUTE` would with user variables for
/// its parameters, so that the caller can check it as MySQL does.
pub fn lower_prepared(
    text: &str,
    environment: &dyn Environment,
) -> Result<ast::Stmt, MysqlParserError> {
    if command(text)?.is_some() {
        return Err(MysqlParserError::Rejected(
            "ERROR 1295 (HY000): This command is not supported in the prepared statement protocol yet"
                .to_string(),
        ));
    }
    let count = placeholders(text)?.len();
    let sql = with_parameters(text, &vec!["@parameter".to_string(); count])?;
    crate::translate_text(&sql, environment)
}

fn placeholders(text: &str) -> Result<Vec<Location>, MysqlParserError> {
    Ok(tokens(text)?
        .into_iter()
        .filter(
            |token| matches!(&token.token, Token::Placeholder(placeholder) if placeholder == "?"),
        )
        .map(|token| token.span.start)
        .collect())
}

/// `text` with its `?` parameters replaced, in order, by the user variables
/// `variables`; MySQL rejects a different number of variables (1210).
fn with_parameters(text: &str, variables: &[String]) -> Result<String, MysqlParserError> {
    let placeholders = placeholders(text)?;
    if placeholders.len() != variables.len() {
        return Err(MysqlParserError::Rejected(
            "ERROR 1210 (HY000): Incorrect arguments to EXECUTE".to_string(),
        ));
    }
    let mut offsets = placeholders
        .iter()
        .map(|location| byte_offset(text, *location));
    let mut next = offsets.next();
    let mut variables = variables.iter();
    let mut sql = String::with_capacity(text.len() + variables.len() * 8);
    for (offset, ch) in text.char_indices() {
        if Some(offset) == next {
            assert_eq!(ch, '?', "a parameter is a `?`");
            sql.push(' ');
            sql.push_str(variables.next().expect("a variable for each parameter"));
            sql.push(' ');
            next = offsets.next();
        } else {
            sql.push(ch);
        }
    }
    assert!(next.is_none(), "every parameter is replaced");
    Ok(sql)
}

/// The byte offset in `text` of the 1-based line and character column
/// `location`.
pub(crate) fn byte_offset(text: &str, location: Location) -> usize {
    let (mut line, mut column) = (1, 1);
    for (offset, ch) in text.char_indices() {
        if line == location.line && column == location.column {
            return offset;
        }
        if ch == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    unreachable!("a token location is inside its text")
}

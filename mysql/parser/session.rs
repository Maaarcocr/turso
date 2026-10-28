use crate::MysqlParserError;
use sqlparser::ast::{ContextModifier, Expr, Ident, ObjectName, ObjectNamePart, Set, Value};
use turso_parser::ast;

pub(crate) fn translate_set(set: &Set) -> Result<ast::Stmt, MysqlParserError> {
    let changes = match set {
        Set::SetNames {
            charset_name,
            collation_name,
        } => vec![names(charset_name, collation_name.as_deref())?],
        Set::SingleAssignment {
            scope,
            hivevar: false,
            variable,
            values,
        } => match values.as_slice() {
            [value] => vec![assignment(scope.as_ref(), variable, value)?],
            _ => return Err(unsupported(set)),
        },
        Set::MultipleAssignments { assignments } => assignments
            .iter()
            .map(|assignment_| {
                assignment(
                    assignment_.scope.as_ref(),
                    &assignment_.name,
                    &assignment_.value,
                )
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return Err(unsupported(set)),
    };
    let foreign_key_checks = changes
        .iter()
        .filter_map(|change| match change {
            Change::KeepsTheTargetProfile => None,
            Change::ForeignKeyChecks(enabled) => Some(*enabled),
        })
        .collect::<Vec<_>>();
    match foreign_key_checks.as_slice() {
        [] => Ok(statement_without_effect()),
        [enabled] => Ok(foreign_keys_pragma(*enabled)),
        _ => Err(unsupported(set)),
    }
}

enum Change {
    KeepsTheTargetProfile,
    ForeignKeyChecks(bool),
}

fn names(charset: &Ident, collation: Option<&str>) -> Result<Change, MysqlParserError> {
    let collation_is_default =
        collation.is_none_or(|collation| collation.eq_ignore_ascii_case(TARGET_COLLATION));
    if charset.value.eq_ignore_ascii_case(TARGET_CHARSET) && collation_is_default {
        Ok(Change::KeepsTheTargetProfile)
    } else {
        Err(MysqlParserError::Unsupported(format!(
            "SET NAMES {charset}, which changes the connection character set or collation from utf8mb4_0900_ai_ci, is not lowered yet"
        )))
    }
}

const TARGET_CHARSET: &str = "utf8mb4";

const TARGET_COLLATION: &str = "utf8mb4_0900_ai_ci";

const TARGET_SQL_MODE: &[&str] = &[
    "ONLY_FULL_GROUP_BY",
    "STRICT_TRANS_TABLES",
    "NO_ZERO_IN_DATE",
    "NO_ZERO_DATE",
    "ERROR_FOR_DIVISION_BY_ZERO",
    "NO_ENGINE_SUBSTITUTION",
];

fn assignment(
    scope: Option<&ContextModifier>,
    name: &ObjectName,
    value: &Expr,
) -> Result<Change, MysqlParserError> {
    let variable = session_variable(scope, name)?;
    let change = match variable.as_str() {
        "foreign_key_checks" => switch(value).map(Change::ForeignKeyChecks),
        "autocommit" => (switch(value) == Some(true)).then_some(Change::KeepsTheTargetProfile),
        "time_zone" => string(value)
            .filter(|zone| *zone == "+00:00" || zone.eq_ignore_ascii_case("UTC"))
            .map(|_| Change::KeepsTheTargetProfile),
        "sql_mode" => string(value)
            .filter(|modes| is_target_sql_mode(modes))
            .map(|_| Change::KeepsTheTargetProfile),
        "character_set_client" | "character_set_connection" | "character_set_results" => {
            text_or_name(value)
                .filter(|charset| charset.eq_ignore_ascii_case(TARGET_CHARSET))
                .map(|_| Change::KeepsTheTargetProfile)
        }
        "collation_connection" => text_or_name(value)
            .filter(|collation| collation.eq_ignore_ascii_case(TARGET_COLLATION))
            .map(|_| Change::KeepsTheTargetProfile),
        _ => None,
    };
    change.ok_or_else(|| {
        MysqlParserError::Unsupported(format!(
            "SET {variable} = {value}, which changes the session from the target profile, is not lowered yet"
        ))
    })
}

fn session_variable(
    scope: Option<&ContextModifier>,
    name: &ObjectName,
) -> Result<String, MysqlParserError> {
    let unsupported_variable =
        || MysqlParserError::Unsupported(format!("SET of variable `{name}` is not lowered yet"));
    if matches!(scope, Some(ContextModifier::Global)) {
        return Err(unsupported_variable());
    }
    let parts = name
        .0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(identifier) if identifier.quote_style.is_none() => {
                Some(identifier.value.as_str())
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(unsupported_variable)?;
    let variable = match parts.as_slice() {
        [variable] if !variable.starts_with('@') => variable,
        [variable] => variable
            .strip_prefix("@@")
            .filter(|variable| !variable.contains('@'))
            .ok_or_else(unsupported_variable)?,
        [prefix, variable]
            if prefix.eq_ignore_ascii_case("@@session")
                || prefix.eq_ignore_ascii_case("@@local") =>
        {
            variable
        }
        _ => return Err(unsupported_variable()),
    };
    Ok(variable.to_ascii_lowercase())
}

fn switch(value: &Expr) -> Option<bool> {
    match value {
        Expr::Value(value) => match &value.value {
            Value::Number(number, false) if number == "0" => Some(false),
            Value::Number(number, false) if number == "1" => Some(true),
            Value::Boolean(enabled) => Some(*enabled),
            _ => None,
        },
        Expr::Identifier(word) if word.quote_style.is_none() => {
            match word.value.to_ascii_uppercase().as_str() {
                "ON" => Some(true),
                "OFF" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

fn string(value: &Expr) -> Option<&str> {
    match value {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Some(text),
            _ => None,
        },
        _ => None,
    }
}

fn text_or_name(value: &Expr) -> Option<&str> {
    match value {
        Expr::Identifier(word) if word.quote_style.is_none() => Some(&word.value),
        other => string(other),
    }
}

fn is_target_sql_mode(modes: &str) -> bool {
    let mut modes = modes
        .split(',')
        .map(str::to_ascii_uppercase)
        .collect::<Vec<_>>();
    modes.sort();
    modes.dedup();
    let mut target = TARGET_SQL_MODE
        .iter()
        .map(|mode| mode.to_string())
        .collect::<Vec<_>>();
    target.sort();
    modes == target
}

fn statement_without_effect() -> ast::Stmt {
    let zero = || Box::new(ast::Expr::Literal(ast::Literal::Numeric("0".to_string())));
    ast::Stmt::Select(ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns: vec![ast::ResultColumn::Expr(zero(), None)],
                from: None,
                where_clause: Some(zero()),
                group_by: None,
                window_clause: Vec::new(),
            },
            compounds: Vec::new(),
        },
        order_by: Vec::new(),
        limit: None,
    })
}

fn foreign_keys_pragma(enabled: bool) -> ast::Stmt {
    let value = if enabled { "1" } else { "0" };
    ast::Stmt::Pragma {
        name: ast::QualifiedName::single(ast::Name::exact("foreign_keys".to_string())),
        body: Some(ast::PragmaBody::Equals(Box::new(ast::Expr::Literal(
            ast::Literal::Numeric(value.to_string()),
        )))),
    }
}

fn unsupported(set: &Set) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("SET statement `{set}` is not lowered yet"))
}

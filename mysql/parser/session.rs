use crate::expr::{collation_of, function_call, string_expr, translate_bytes, DEFAULT_COLLATION};
use crate::query::TranslationContext;
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
    let mut foreign_key_checks = Vec::new();
    let mut checked = Vec::new();
    let mut user_variables = Vec::new();
    for change in &changes {
        match change {
            Change::KeepsTheTargetProfile => {}
            Change::ForeignKeyChecks(enabled) => foreign_key_checks.push(*enabled),
            Change::Checked(name, value) => checked.push((name.as_str(), *value)),
            Change::UserVariable(name, value) => user_variables.push((name.as_str(), *value)),
        }
    }
    let mut context = TranslationContext::default();
    let mut effects = Vec::new();
    if !checked.is_empty() {
        let mut args = Vec::with_capacity(checked.len() * 2);
        for (name, value) in &checked {
            args.push(string_expr(name));
            args.push(match value {
                Expr::Identifier(word) if word.quote_style.is_none() => {
                    match user_variable_name(&word.value) {
                        Some(variable) => function_call(
                            "__mysql_user_variable_value",
                            vec![string_expr(&variable)],
                        ),
                        None => translate_bytes(value, &mut context)?,
                    }
                }
                _ => translate_bytes(value, &mut context)?,
            });
        }
        effects.push(function_call("__mysql_keep_session_settings", args));
    }
    if !user_variables.is_empty() {
        effects.push(user_variable_assignments(&user_variables, &mut context)?);
    }
    match (foreign_key_checks.as_slice(), effects.is_empty()) {
        ([], _) => Ok(statement_with_effect(
            effects
                .into_iter()
                .reduce(|first, second| ast::Expr::binary(first, ast::Operator::Or, second))
                .unwrap_or_else(|| number("0")),
        )),
        ([enabled], true) => Ok(foreign_keys_pragma(*enabled)),
        _ => Err(unsupported(set)),
    }
}

enum Change<'a> {
    KeepsTheTargetProfile,
    ForeignKeyChecks(bool),
    /// A setting assigned an expression, which keeps the target profile only
    /// if its value at run time is the profile's.
    Checked(String, &'a Expr),
    UserVariable(String, &'a Expr),
}

/// Whether `SET variable = 'value'` keeps the target profile, for the
/// settings that can be assigned an expression.
pub fn keeps_target_profile(variable: &str, value: &str) -> bool {
    match variable {
        "time_zone" => value == "+00:00" || value.eq_ignore_ascii_case("UTC"),
        "sql_mode" => is_target_sql_mode(value),
        "character_set_client" | "character_set_connection" | "character_set_results" => {
            value.eq_ignore_ascii_case(TARGET_CHARSET)
        }
        "collation_connection" => value.eq_ignore_ascii_case(TARGET_COLLATION),
        _ => false,
    }
}

/// The name of user variable `@name`, which MySQL matches without regard to
/// case.
pub(crate) fn user_variable_name(text: &str) -> Option<String> {
    let name = text.strip_prefix('@')?;
    (!name.is_empty() && !name.starts_with('@')).then(|| name.to_lowercase())
}

/// `SET @a = x, @b = y`: one call that stores the values in the connection
/// and keeps no row. Its arguments are all read before it stores any, so an
/// assignment that reads a variable assigned earlier in the same statement is
/// refused. Each value comes with whether its collation is known to be
/// `utf8mb4_0900_ai_ci` (a literal or another variable), which a string
/// read back from the variable needs: comparisons give `@a` that collation.
fn user_variable_assignments(
    assignments: &[(&str, &Expr)],
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    for (index, (name, value)) in assignments.iter().enumerate() {
        let text = value.to_string().to_lowercase();
        if let Some(earlier) = assignments[..index]
            .iter()
            .find(|(earlier, _)| reads_user_variable(&text, earlier))
        {
            return Err(MysqlParserError::Unsupported(format!(
                "SET of `@{name}` from `@{}`, assigned earlier in the same SET, is not lowered yet",
                earlier.0
            )));
        }
    }
    let mut args = Vec::with_capacity(assignments.len() * 3);
    for (name, value) in assignments {
        let default_collation = collation_of(value) == Some(DEFAULT_COLLATION);
        args.push(string_expr(name));
        args.push(number(if default_collation { "1" } else { "0" }));
        args.push(translate_bytes(value, context)?);
    }
    Ok(function_call("__mysql_set_user_variables", args))
}

fn reads_user_variable(text: &str, name: &str) -> bool {
    let reference = format!("@{name}");
    text.match_indices(&reference).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + reference.len()..].chars().next();
        before != Some('@')
            && !after.is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.'))
    })
}

/// The value of `@@name` or `@@scope.name` for the system variables that keep
/// MySQL 8.4's default in the target profile, which every session has since
/// SET accepts only those values for them.
pub(crate) fn system_variable(scope: &str, name: &Ident) -> Result<ast::Expr, MysqlParserError> {
    let refused = || {
        MysqlParserError::Unsupported(format!(
            "system variable `{scope}{}{name}` is not lowered yet",
            if scope == "@@" { "" } else { "." }
        ))
    };
    if name.quote_style.is_some() || scope.eq_ignore_ascii_case("@@global") {
        return Err(refused());
    }
    let sql_mode = TARGET_SQL_MODE.join(",");
    Ok(match name.value.to_ascii_lowercase().as_str() {
        "sql_mode" => string_expr(&sql_mode),
        "time_zone" => string_expr("+00:00"),
        "transaction_isolation" => string_expr("REPEATABLE-READ"),
        "character_set_client"
        | "character_set_connection"
        | "character_set_results"
        | "character_set_server"
        | "character_set_database" => string_expr(TARGET_CHARSET),
        "collation_connection" | "collation_server" | "collation_database" => {
            string_expr(TARGET_COLLATION)
        }
        "version" => string_expr("8.4.10"),
        "autocommit"
        | "auto_increment_increment"
        | "auto_increment_offset"
        | "unique_checks"
        | "explicit_defaults_for_timestamp" => number("1"),
        "lower_case_table_names"
        | "sql_auto_is_null"
        | "transaction_read_only"
        | "max_execution_time"
        | "sql_safe_updates"
        | "default_week_format" => number("0"),
        "div_precision_increment" => number("4"),
        "group_concat_max_len" => number("1024"),
        "max_allowed_packet" => number("67108864"),
        "default_storage_engine" => string_expr("InnoDB"),
        _ => return Err(refused()),
    })
}

fn number(digits: &str) -> ast::Expr {
    ast::Expr::Literal(ast::Literal::Numeric(digits.to_string()))
}

fn names(charset: &Ident, collation: Option<&str>) -> Result<Change<'static>, MysqlParserError> {
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

fn assignment<'a>(
    scope: Option<&ContextModifier>,
    name: &ObjectName,
    value: &'a Expr,
) -> Result<Change<'a>, MysqlParserError> {
    if let (None, [ObjectNamePart::Identifier(identifier)]) = (scope, name.0.as_slice()) {
        if identifier.quote_style.is_none() {
            if let Some(user_variable) = user_variable_name(&identifier.value) {
                return Ok(Change::UserVariable(user_variable, value));
            }
        }
    }
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
    let literal = match value {
        Expr::Value(_) => true,
        Expr::Identifier(word) => user_variable_name(&word.value).is_none(),
        _ => false,
    };
    match change {
        Some(change) => Ok(change),
        None if !literal && keeps_target_profile(&variable, target_value(&variable)) => {
            Ok(Change::Checked(variable, value))
        }
        None => Err(MysqlParserError::Unsupported(format!(
            "SET {variable} = {value}, which changes the session from the target profile, is not lowered yet"
        ))),
    }
}

fn target_value(variable: &str) -> &'static str {
    match variable {
        "time_zone" => "+00:00",
        "character_set_client" | "character_set_connection" | "character_set_results" => {
            TARGET_CHARSET
        }
        "collation_connection" => TARGET_COLLATION,
        "sql_mode" => "ONLY_FULL_GROUP_BY,STRICT_TRANS_TABLES,NO_ZERO_IN_DATE,NO_ZERO_DATE,ERROR_FOR_DIVISION_BY_ZERO,NO_ENGINE_SUBSTITUTION",
        _ => "",
    }
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

/// `SELECT 0 WHERE condition`, for a statement that returns no row: the
/// condition runs for its effect and is false.
fn statement_with_effect(condition: ast::Expr) -> ast::Stmt {
    ast::Stmt::Select(ast::Select {
        with: None,
        body: ast::SelectBody {
            select: ast::OneSelect::Select {
                distinctness: None,
                columns: vec![ast::ResultColumn::Expr(Box::new(number("0")), None)],
                from: None,
                where_clause: Some(Box::new(condition)),
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

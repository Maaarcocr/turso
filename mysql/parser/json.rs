use crate::expr::{
    function_call, has_a_temporal_type, is_binary_literal, is_boolean,
    json_outside_a_value_position, number_literal, parenthesized, string_literal, times_minus_one,
    translate_bytes, translate_expr, translate_in, translate_json_value,
};
use crate::invalid_json::mysql_rejects_json_text;
use crate::literal::{is_utf8_binary_literal, temporal_literal, Context, TemporalKind};
use crate::query::TranslationContext;
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, DataType, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments,
    ObjectNamePart, UnaryOperator, Value,
};
use turso_parser::ast;

pub(crate) fn is_json_value(expr: &Expr) -> bool {
    match expr {
        Expr::Cast {
            data_type: DataType::JSON,
            ..
        }
        | Expr::Convert {
            data_type: Some(DataType::JSON),
            ..
        }
        | Expr::BinaryOp {
            op: BinaryOperator::Arrow,
            ..
        } => true,
        Expr::Function(function) => json_function_name(function).is_some(),
        Expr::Nested(inner) => is_json_value(inner),
        _ => false,
    }
}

fn json_function_name(function: &Function) -> Option<String> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    let upper_name = name.value.to_ascii_uppercase();
    let returns_json = matches!(
        upper_name.as_str(),
        "JSON_ARRAY"
            | "JSON_OBJECT"
            | "JSON_EXTRACT"
            | "JSON_SET"
            | "JSON_INSERT"
            | "JSON_REPLACE"
            | "JSON_REMOVE"
            | "JSON_ARRAY_APPEND"
            | "JSON_ARRAY_INSERT"
            | "JSON_MERGE"
            | "JSON_MERGE_PRESERVE"
            | "JSON_MERGE_PATCH"
            | "JSON_KEYS"
            | "JSON_SEARCH"
    );
    (name.quote_style.is_none() && returns_json).then_some(upper_name)
}

fn json_cast_operand(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Cast {
            expr,
            data_type: DataType::JSON,
            ..
        }
        | Expr::Convert {
            expr,
            data_type: Some(DataType::JSON),
            ..
        } => Some(expr),
        Expr::Nested(inner) => json_cast_operand(inner),
        _ => None,
    }
}

fn cast_succeeds(operand: &Expr) -> bool {
    match operand {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text)
            | Value::DoubleQuotedString(text)
            | Value::NationalStringLiteral(text) => !mysql_rejects_json_text(text),
            Value::Number(digits, _) => fits_bigint(digits),
            Value::Null
            | Value::Boolean(_)
            | Value::HexStringLiteral(_)
            | Value::SingleQuotedByteStringLiteral(_) => true,
            _ => false,
        },
        Expr::Prefixed { prefix, value } => {
            prefix.value.eq_ignore_ascii_case("_binary")
                || string_literal(value).is_some_and(|text| !mysql_rejects_json_text(text))
        }
        Expr::Nested(inner) => cast_succeeds(inner),
        Expr::TypedString(typed) => temporal_literal(typed).is_some(),
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => number_literal(expr).is_some_and(fits_bigint),
        _ if is_json_value(operand) => json_value_succeeds(operand),
        _ => is_boolean(operand),
    }
}

fn fits_bigint(digits: &str) -> bool {
    let significant = digits.trim_start_matches('0');
    !significant.bytes().all(|byte| byte.is_ascii_digit())
        || significant.len() < 19
        || (significant.len() == 19 && significant <= "9223372036854775807")
}

pub(crate) fn is_json_value_that_succeeds(expr: &Expr) -> bool {
    is_json_value(expr) && json_value_succeeds(expr)
}

fn json_value_succeeds(expr: &Expr) -> bool {
    if let Some(operand) = json_cast_operand(expr) {
        return cast_succeeds(operand);
    }
    let Expr::Function(function) = unnested(expr) else {
        return false;
    };
    let Some(arguments) = plain_arguments(function) else {
        return false;
    };
    match json_function_name(function).as_deref() {
        Some("JSON_ARRAY") => arguments.iter().all(|arg| member_succeeds(arg)),
        Some("JSON_OBJECT") => arguments.chunks(2).all(|pair| match pair {
            [key, value] => member_name_is_literal(key) && member_succeeds(value),
            _ => false,
        }),
        _ => false,
    }
}

fn is_lowered_operand(expr: &Expr) -> bool {
    if json_cast_operand(expr).is_some() {
        return json_value_succeeds(expr);
    }
    match unnested(expr) {
        Expr::Function(function) => match json_function_name(function).as_deref() {
            Some("JSON_ARRAY" | "JSON_OBJECT") => json_value_succeeds(expr),
            _ => plain_arguments(function).is_some_and(|arguments| {
                arguments
                    .iter()
                    .all(|arg| !is_json_value(arg) || is_lowered_operand(arg))
            }),
        },
        Expr::BinaryOp {
            op: BinaryOperator::Arrow,
            left,
            ..
        } => !is_json_value(left) || is_lowered_operand(left),
        _ => false,
    }
}

fn member_succeeds(expr: &Expr) -> bool {
    if is_json_value(expr) {
        return json_value_succeeds(expr) && !is_opaque_or_temporal_cast(expr);
    }
    comparable_scalar(expr) && !is_binary_literal(expr)
}

fn member_name_is_literal(expr: &Expr) -> bool {
    match unnested(expr) {
        Expr::Value(value) => matches!(
            value.value,
            Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::NationalStringLiteral(_)
        ),
        _ => false,
    }
}

fn is_opaque_or_temporal_cast(expr: &Expr) -> bool {
    json_cast_operand(expr)
        .is_some_and(|operand| is_binary_literal(operand) || has_a_temporal_type(operand))
}

fn plain_arguments(function: &Function) -> Option<Vec<&Expr>> {
    let FunctionArguments::List(list) = &function.args else {
        return None;
    };
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() || function.over.is_some() {
        return None;
    }
    list.args
        .iter()
        .map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Some(expr),
            _ => None,
        })
        .collect()
}

fn unnested(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unnested(inner),
        other => other,
    }
}

fn comparable_scalar(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, _) => fits_bigint(digits),
            Value::SingleQuotedString(_)
            | Value::DoubleQuotedString(_)
            | Value::NationalStringLiteral(_)
            | Value::Null
            | Value::Boolean(_)
            | Value::HexStringLiteral(_)
            | Value::SingleQuotedByteStringLiteral(_) => true,
            _ => false,
        },
        Expr::Prefixed { .. } => true,
        Expr::TypedString(typed) => temporal_literal(typed).is_some(),
        Expr::Nested(inner) => comparable_scalar(inner),
        _ => is_number(expr) || is_boolean(expr),
    }
}

fn is_number(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, _) => fits_bigint(digits),
            Value::Null => true,
            _ => false,
        },
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => is_number(inner),
        Expr::BinaryOp { left, op, right } => {
            matches!(
                op,
                BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Divide
                    | BinaryOperator::Modulo
            ) && is_number(left)
                && is_number(right)
        }
        _ => false,
    }
}

fn json_operand(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if !is_lowered_operand(expr) {
        return Err(json_outside_a_value_position(expr));
    }
    match json_cast_operand(expr) {
        Some(operand) if has_a_temporal_type(operand) => {
            temporal_operand(operand).ok_or_else(|| {
                MysqlParserError::Unsupported(format!(
                    "JSON value `{expr}` of a temporal value in a comparison, a computation or a condition is not lowered yet"
                ))
            })
        }
        Some(operand) if is_binary_literal(operand) => translate_bytes(operand, context),
        _ => translate_expr(expr, context),
    }
}

fn compared_operand(
    expr: &Expr,
    other_side: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if is_json_value(expr) {
        return json_operand(expr, context);
    }
    if !comparable_scalar(expr) {
        return Err(MysqlParserError::Unsupported(format!(
            "comparison of `{expr}` with the JSON value `{other_side}` is not lowered yet"
        )));
    }
    match temporal_operand(expr) {
        Some(temporal) => Ok(temporal),
        None => translate_json_value(expr, context),
    }
}

pub(crate) fn temporal_json_text(expr: &Expr) -> Option<ast::Expr> {
    let Expr::TypedString(typed) = unnested(json_cast_operand(expr)?) else {
        return None;
    };
    let literal = temporal_literal(typed).filter(|literal| literal.within_a_day)?;
    let printed = match literal.kind {
        TemporalKind::Date => literal.text,
        TemporalKind::Time | TemporalKind::DateTime => {
            let (whole, fraction) = literal
                .text
                .split_once('.')
                .unwrap_or((literal.text.as_str(), ""));
            if fraction.len() > 6 {
                return None;
            }
            format!("{whole}.{fraction:0<6}")
        }
    };
    let json = format!("\"{printed}\"");
    Some(function_call(
        "__mysql_cast_as_json",
        vec![ast::Expr::Literal(ast::Literal::String(
            crate::names::quote_string(&json),
        ))],
    ))
}

fn temporal_operand(expr: &Expr) -> Option<ast::Expr> {
    let Expr::TypedString(typed) = unnested(expr) else {
        return None;
    };
    let literal = temporal_literal(typed)?;
    let (name, precision) = match literal.kind {
        TemporalKind::Date => ("DATE", None),
        TemporalKind::Time => ("TIME", Some(6)),
        TemporalKind::DateTime => ("DATETIME", Some(6)),
    };
    let target = ast::Type {
        name: name.to_string(),
        size: precision.map(|precision: u64| {
            ast::TypeSize::MaxSize(Box::new(ast::Expr::Literal(ast::Literal::Numeric(
                precision.to_string(),
            ))))
        }),
        array_dimensions: 0,
    };
    Some(ast::Expr::cast(
        ast::Expr::Literal(ast::Literal::String(crate::names::quote_string(
            &literal.text,
        ))),
        Some(target),
    ))
}

pub(crate) fn translate_comparison(
    left: &Expr,
    operator: ast::Operator,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let lhs = compared_operand(left, right, context)?;
    let rhs = compared_operand(right, left, context)?;
    if operator == ast::Operator::Is {
        return Ok(function_call(
            "__mysql_json_null_safe_equal",
            vec![lhs, rhs],
        ));
    }
    Ok(parenthesized(ast::Expr::binary(
        function_call("__mysql_json_compare", vec![lhs, rhs]),
        operator,
        ast::Expr::Literal(ast::Literal::Numeric("0".to_string())),
    )))
}

pub(crate) fn translate_truth(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(function_call(
        "__mysql_json_truth",
        vec![json_operand(expr, context)?],
    ))
}

pub(crate) fn translate_number(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(function_call(
        "__mysql_json_double",
        vec![json_operand(expr, context)?],
    ))
}

pub(crate) fn number_cast_operand(expr: &Expr) -> Option<&Expr> {
    let operand = json_cast_operand(expr)?;
    let digits = match unnested(operand) {
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => number_literal(expr)?,
        other => number_literal(other)?,
    };
    (fits_bigint(digits) && !digits.contains(['e', 'E'])).then_some(operand)
}

pub(crate) fn translate_numeric_coercion(
    expr: &Expr,
    coercion: &str,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let coerced = match json_cast_operand(expr) {
        Some(operand) => match unnested(operand) {
            Expr::Value(value) => match &value.value {
                Value::SingleQuotedString(text)
                | Value::DoubleQuotedString(text)
                | Value::NationalStringLiteral(text) => !is_numeric_json_string(text),
                Value::Number(digits, _) => fits_bigint(digits),
                _ => true,
            },
            Expr::Prefixed { prefix, value } => {
                prefix.value.eq_ignore_ascii_case("_binary")
                    || string_literal(value).is_some_and(|text| !is_numeric_json_string(text))
            }
            Expr::TypedString(_) | Expr::UnaryOp { .. } => true,
            other => is_boolean(other),
        },
        None => matches!(
            unnested(expr),
            Expr::Function(function)
                if matches!(json_function_name(function).as_deref(), Some("JSON_ARRAY" | "JSON_OBJECT"))
        ),
    };
    if !coerced {
        return Err(json_outside_a_value_position(expr));
    }
    Ok(function_call(coercion, vec![json_operand(expr, context)?]))
}

fn is_numeric_json_string(text: &str) -> bool {
    text.trim_start().starts_with('"') && text.bytes().any(|byte| byte.is_ascii_digit())
}

pub(crate) fn translate_arithmetic_operand(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if is_json_value(expr) {
        translate_number(expr, context)
    } else {
        translate_in(expr, Context::Arithmetic, context)
    }
}

pub(crate) fn translate_negation(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(times_minus_one(translate_number(expr, context)?))
}

pub(crate) fn translate_like(
    expr: &Expr,
    negated: bool,
    pattern: &Expr,
    escape: char,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let subject = like_operand(expr, context)?;
    let pattern = like_operand(pattern, context)?;
    let escape = ast::Expr::Literal(ast::Literal::String(crate::names::quote_string(
        &escape.to_string(),
    )));
    let matched = function_call("__mysql_like_bin", vec![subject, pattern, escape]);
    Ok(if negated {
        ast::Expr::unary(ast::UnaryOperator::Not, matched)
    } else {
        matched
    })
}

fn like_operand(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if is_binary_literal(expr) && !is_utf8_binary_literal(expr) {
        return Err(MysqlParserError::Rejected(format!(
            "cannot convert the binary string `{expr}` of LIKE to utf8mb4"
        )));
    }
    if !is_json_value(expr) {
        return translate_in(expr, Context::Text, context);
    }
    if let Some(text) = temporal_json_text(expr) {
        return Ok(text);
    }
    if !is_lowered_operand(expr) {
        return Err(json_outside_a_value_position(expr));
    }
    translate_expr(expr, context)
}

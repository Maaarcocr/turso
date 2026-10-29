use crate::expr::{function_call, hex_or_bit_literal, translate_in};
use crate::json;
use crate::literal::{binary_literal_value, Context};
use crate::query::TranslationContext;
use crate::MysqlParserError;
use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator, Value};
use turso_parser::ast;

pub(crate) fn is_bit_operation(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_bit_operation(inner),
        other => bit_operation(other).is_some(),
    }
}
pub(crate) fn translate_bit_operation(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let shown = std::mem::take(&mut context.unsigned_as_text);
    let bits = operand_bits(expr, context)?;
    Ok(function_call(
        if shown {
            "__mysql_unsigned_shown"
        } else {
            "__mysql_unsigned_value"
        },
        vec![bits],
    ))
}

fn bit_operation(expr: &Expr) -> Option<(&'static str, Vec<&Expr>)> {
    match expr {
        Expr::UnaryOp {
            op: UnaryOperator::BitwiseNot,
            expr,
        } => Some(("__mysql_bit_not", vec![expr])),
        Expr::BinaryOp { left, op, right } => {
            let function = match op {
                BinaryOperator::BitwiseOr => "__mysql_bit_or",
                BinaryOperator::BitwiseAnd => "__mysql_bit_and",
                BinaryOperator::BitwiseXor => "__mysql_bit_xor",
                BinaryOperator::PGBitwiseShiftLeft => "__mysql_shift_left",
                BinaryOperator::PGBitwiseShiftRight => "__mysql_shift_right",
                _ => return None,
            };
            Some((function, vec![left, right]))
        }
        _ => None,
    }
}

fn operand_bits(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Expr::Nested(inner) = expr {
        return operand_bits(inner, context);
    }
    if let Some((function, operands)) = bit_operation(expr) {
        let args = operands
            .into_iter()
            .map(|operand| operand_bits(operand, context))
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(function_call(function, args));
    }
    if json::is_json_value(expr) {
        return json::translate_numeric_coercion(expr, "__mysql_json_integer", context);
    }
    if let Some(literal) = hex_or_bit_literal(expr) {
        return Ok(bits_literal(binary_literal_value(literal)? as i64));
    }
    if let Some(bits) = literal_bits(expr)? {
        return Ok(bits);
    }
    Ok(function_call(
        "__mysql_bit_operand",
        vec![translate_in(expr, Context::Bitwise, context)?],
    ))
}

fn literal_bits(expr: &Expr) -> Result<Option<ast::Expr>, MysqlParserError> {
    let (negative, value) = match expr {
        Expr::Value(value) => (false, &value.value),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match unnested(expr) {
            Expr::Value(value) if matches!(value.value, Value::Number(..)) => (true, &value.value),
            _ => return Ok(None),
        },
        Expr::Prefixed { prefix, value } if prefix.quote_style.is_none() => match value.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)
                    if reads_as_text(&prefix.value, text) =>
                {
                    return Ok(Some(bits_literal(string_bits(text)?)));
                }
                _ => return Ok(None),
            },
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let bits = match value {
        Value::Null => return Ok(Some(ast::Expr::Literal(ast::Literal::Null))),
        Value::Boolean(value) => i64::from(*value),
        Value::Number(digits, _) => number_bits(digits, negative)?,
        Value::SingleQuotedString(text)
        | Value::DoubleQuotedString(text)
        | Value::NationalStringLiteral(text) => string_bits(text)?,
        _ => return Ok(None),
    };
    Ok(Some(bits_literal(bits)))
}

fn reads_as_text(charset: &str, text: &str) -> bool {
    match charset.to_ascii_lowercase().as_str() {
        "_utf8mb4" | "_utf8mb3" | "_utf8" => true,
        "_ascii" | "_latin1" => text.is_ascii(),
        _ => false,
    }
}

fn unnested(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unnested(inner),
        other => other,
    }
}

fn bits_literal(bits: i64) -> ast::Expr {
    let literal = ast::Expr::Literal(ast::Literal::Numeric(bits.unsigned_abs().to_string()));
    if bits < 0 {
        ast::Expr::unary(ast::UnaryOperator::Negative, literal)
    } else {
        literal
    }
}
fn number_bits(digits: &str, negative: bool) -> Result<i64, MysqlParserError> {
    const BIGINT_LIMIT: f64 = 9_223_372_036_854_775_808.0;
    let unsupported = || {
        MysqlParserError::Unsupported(format!(
            "bit operation on `{}{digits}`, a DOUBLE below the BIGINT range, is not lowered yet",
            if negative { "-" } else { "" }
        ))
    };
    if digits.contains(['e', 'E']) {
        let double = digits.parse::<f64>().map_err(|_| unsupported())?;
        let rounded = if negative { -double } else { double }.round_ties_even();
        return if rounded < -BIGINT_LIMIT {
            Err(unsupported())
        } else if rounded >= BIGINT_LIMIT {
            Ok(i64::MAX)
        } else {
            Ok(rounded as i64)
        };
    }
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
    {
        return Err(MysqlParserError::Unsupported(format!(
            "bit operation on the number `{digits}` is not lowered yet"
        )));
    }
    let whole = whole.trim_start_matches('0');
    let magnitude = if whole.len() > 30 {
        u128::MAX
    } else {
        whole.parse::<u128>().unwrap_or(0)
    };
    if !negative && !digits.contains('.') {
        if let Ok(unsigned) = u64::try_from(magnitude) {
            return Ok(unsigned as i64);
        }
    }
    let magnitude =
        magnitude.saturating_add(u128::from(fraction.starts_with(['5', '6', '7', '8', '9'])));
    Ok(if negative {
        i64::try_from(magnitude).map_or(i64::MIN, |magnitude| -magnitude)
    } else {
        i64::try_from(magnitude).unwrap_or(i64::MAX)
    })
}
fn string_bits(text: &str) -> Result<i64, MysqlParserError> {
    let unsupported = || {
        MysqlParserError::Unsupported(format!(
            "bit operation on the string '{text}', whose number MySQL reads with an exponent or outside the BIGINT range, is not lowered yet"
        ))
    };
    let trimmed = text.trim_start_matches([' ', '\t', '\n', '\r', '\u{b}', '\u{c}']);
    let (negative, unsigned) = match trimmed.as_bytes().first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
    let count = unsigned.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, rest) = unsigned.split_at(count);
    let fraction = rest.strip_prefix('.').map_or(rest, |fraction| {
        fraction.trim_start_matches(|character: char| character.is_ascii_digit())
    });
    if fraction.starts_with(['e', 'E']) {
        return Err(unsupported());
    }
    let digits = digits.trim_start_matches('0');
    if digits.len() > 19 {
        return Err(unsupported());
    }
    let magnitude = digits.parse::<i128>().unwrap_or(0);
    i64::try_from(if negative { -magnitude } else { magnitude }).map_err(|_| unsupported())
}

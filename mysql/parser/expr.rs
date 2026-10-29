use crate::bitwise::{is_bit_operation, translate_bit_operation};
use crate::dialect::{MAX_TEMPORAL_PRECISION, TEMPORAL_UNITS};
use crate::function_names::{BINARY_STRING_FUNCTIONS, DATE_TIME_FUNCTIONS};
use crate::json;
use crate::literal::{
    binary_literal_as_double, binary_literal_value, bit_bytes, blob_literal, hex_bytes,
    is_latin1_char, is_utf8_binary_literal, latin1_text, negated_binary_literal,
    translate_as_bytes, translate_introduced_string, translate_temporal_text,
    translate_typed_string, translate_typed_string_as_bytes, translate_value, Context,
};
use crate::names::{quote_string, translate_identifier};
use crate::query::{translate_order_by_expr, translate_subquery, TranslationContext};
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, CaseWhen, CastKind, CeilFloorKind, CharacterLength, DataType, DateTimeField,
    DuplicateTreatment, ExactNumberInfo, Expr, Function, FunctionArg, FunctionArgExpr,
    FunctionArgumentClause, FunctionArgumentList, FunctionArguments, Ident, Interval, ObjectName,
    ObjectNamePart, Query, TimezoneInfo, TrimWhereField, TypedString, UnaryOperator, Value,
    WindowFrame, WindowFrameBound, WindowFrameUnits, WindowSpec, WindowType,
};
use turso_parser::ast;

pub(crate) fn translate_expr(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    translate_in(expr, Context::Value, context)
}

pub(crate) fn translate_bytes(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    translate_in(expr, Context::Bytes, context)
}

pub(crate) fn translate_condition(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if json::is_json_value(expr) {
        return json::translate_truth(expr, context);
    }
    translate_in(expr, Context::Number, context)
}

pub(crate) fn translate_in(
    expr: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    context.enter_expression()?;
    let translated = translate_node(expr, literal_context, context);
    context.leave_expression();
    translated
}

fn translate_node(
    expr: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    match expr {
        Expr::Value(value) => translate_value_expr(&value.value, literal_context, context),
        Expr::Prefixed { prefix, value } => {
            translate_introduced_string(prefix, value, literal_context, context.latin1_as_text)
        }
        Expr::TypedString(typed) => translate_typed_string(typed, literal_context),
        Expr::Nested(inner) => translate_nested(inner, literal_context, context),
        Expr::Identifier(identifier) => translate_identifier_expr(identifier),
        Expr::CompoundIdentifier(parts) => translate_compound_identifier(parts),
        expr if is_bit_operation(expr) => translate_bit_operation(expr, context),
        Expr::UnaryOp { op, expr } => translate_unary(*op, expr, literal_context, context),
        Expr::BinaryOp { left, op, right } => {
            translate_binary(left, op, right, literal_context, context)
        }
        Expr::IsNull(inner) => translate_null_test(inner, false, Context::Value, context),
        Expr::IsNotNull(inner) => translate_null_test(inner, true, Context::Value, context),
        Expr::IsUnknown(inner) => translate_null_test(inner, false, Context::Number, context),
        Expr::IsNotUnknown(inner) => translate_null_test(inner, true, Context::Number, context),
        Expr::IsTrue(inner) => truth_test(inner, ast::Operator::Is, ast::Literal::True, context),
        Expr::IsNotTrue(inner) => {
            truth_test(inner, ast::Operator::IsNot, ast::Literal::True, context)
        }
        Expr::IsFalse(inner) => truth_test(inner, ast::Operator::Is, ast::Literal::False, context),
        Expr::IsNotFalse(inner) => {
            truth_test(inner, ast::Operator::IsNot, ast::Literal::False, context)
        }
        Expr::InList {
            expr,
            list,
            negated,
        } => translate_in_list(expr, list, *negated, context),
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => translate_in_subquery(expr, subquery, *negated, context),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => translate_between(expr, *negated, low, high, context),
        Expr::Like {
            negated,
            any: false,
            expr,
            pattern,
            escape_char,
        } => translate_like(expr, *negated, pattern, escape_char.as_ref(), context),
        Expr::RLike {
            negated,
            expr,
            pattern,
            regexp: _,
        } => translate_regexp(expr, *negated, pattern, context),
        Expr::AnyOp {
            left,
            compare_op: BinaryOperator::Eq,
            right,
            is_some: _,
        } => translate_quantified_subquery(left, false, right, context),
        Expr::AllOp {
            left,
            compare_op: BinaryOperator::NotEq,
            right,
        } => translate_quantified_subquery(left, true, right, context),
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => translate_case(
            operand.as_deref(),
            conditions,
            else_result.as_deref(),
            literal_context,
            context,
        ),
        Expr::Exists { subquery, negated } => translate_exists(subquery, *negated, context),
        Expr::Subquery(subquery) => Ok(ast::Expr::Subquery(Box::new(translate_subquery(
            subquery, context,
        )?))),
        Expr::Tuple(items) => translate_tuple(items, context),
        Expr::Cast {
            kind: CastKind::Cast,
            expr,
            data_type,
            array: false,
            format: None,
        } => translate_cast(expr, data_type, literal_context, context),
        Expr::Convert {
            is_try: false,
            expr,
            data_type,
            charset,
            target_before_value: false,
            styles,
        } if styles.is_empty() => translate_convert(
            expr,
            data_type.as_ref(),
            charset.as_ref(),
            literal_context,
            context,
        ),
        Expr::Collate { expr, collation } => translate_collate(expr, collation, context),
        Expr::MemberOf(member_of) => {
            translate_member_of(&member_of.value, &member_of.array, context)
        }
        Expr::Function(function) => translate_function(function, literal_context, context),
        Expr::Extract { field, expr, .. } => translate_extract(field, expr, context),
        Expr::Ceil {
            expr,
            field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
        } => translate_single_argument_call("CEIL", expr, context),
        Expr::Floor {
            expr,
            field: CeilFloorKind::DateTimeField(DateTimeField::NoDateTime),
        } => translate_single_argument_call("FLOOR", expr, context),
        Expr::Position { expr, r#in } => translate_position(expr, r#in, context),
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            special: _,
            shorthand,
        } => translate_substring(
            expr,
            substring_from.as_deref(),
            substring_for.as_deref(),
            *shorthand,
            context,
        ),
        Expr::Trim {
            trim_where,
            trim_what,
            expr,
            trim_characters: None,
        } => translate_trim(trim_where.as_ref(), trim_what.as_deref(), expr, context),
        _ => Err(unsupported(expr)),
    }
}

pub(crate) fn shows_latin1_as_text(expr: &Expr) -> bool {
    const CHARACTER_LIMIT: u64 = 16_777_216;
    const PACKET_BYTES: f64 = 67_108_864.0;
    fn number(text: &str) -> f64 {
        let text = text.trim_start();
        (1..=text.len())
            .rev()
            .filter(|end| text.is_char_boundary(*end))
            .find_map(|end| text[..end].parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .unwrap_or(0.0)
    }
    fn count(expr: &Expr) -> Option<f64> {
        match expr {
            Expr::Value(value)
                if matches!(
                    value.value,
                    Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_)
                ) =>
            {
                binary_literal_value(&value.value)
                    .ok()
                    .map(|value| value as f64)
            }
            Expr::Prefixed { .. } => {
                binary_literal_bytes(expr).map(|bytes| number(&String::from_utf8_lossy(&bytes)))
            }
            Expr::Value(value) => match &value.value {
                Value::Number(text, false)
                | Value::SingleQuotedString(text)
                | Value::DoubleQuotedString(text) => Some(number(text)),
                Value::Null => Some(-1.0),
                _ => None,
            },
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => count(expr).map(|value| -value),
            Expr::Nested(inner) => count(inner),
            _ => None,
        }
    }
    fn mentions_latin1(expr: &Expr) -> bool {
        expr.to_string().to_ascii_lowercase().contains("_latin1")
    }
    fn latin1_strings(expr: &Expr) -> bool {
        match expr {
            Expr::Value(value) => match &value.value {
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                    text.chars().all(is_latin1_char)
                }
                _ => true,
            },
            Expr::TypedString(_) => true,
            Expr::Prefixed { prefix, value } => match value.as_ref() {
                Expr::Value(value) if is_utf8_introducer(&prefix.value) => matches!(
                    &value.value,
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)
                        if text.chars().all(is_latin1_char)
                ),
                _ => true,
            },
            Expr::BinaryOp { op, .. } => is_arithmetic(op),
            Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => latin1_strings(expr),
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                latin1_strings(expr)
                    && substring_from.as_deref().is_none_or(latin1_strings)
                    && substring_for.as_deref().is_none_or(latin1_strings)
            }
            Expr::Trim {
                expr, trim_what, ..
            } => latin1_strings(expr) && trim_what.as_deref().is_none_or(latin1_strings),
            Expr::Function(function) => match &function.args {
                FunctionArguments::List(list) => list.args.iter().all(|arg| {
                    matches!(arg, FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) if latin1_strings(arg))
                }),
                _ => false,
            },
            _ => false,
        }
    }
    fn is_arithmetic(op: &BinaryOperator) -> bool {
        matches!(
            op,
            BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo
                | BinaryOperator::MyIntegerDivide
        )
    }
    fn is_utf8_introducer(introducer: &str) -> bool {
        matches!(
            introducer.to_ascii_lowercase().as_str(),
            "_utf8mb4" | "_utf8mb3" | "_utf8"
        )
    }
    fn negated_binary_bytes(expr: &Expr) -> Option<Vec<u8>> {
        match expr {
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => negated_binary_bytes(expr),
            Expr::Nested(inner) => negated_binary_bytes(inner),
            _ => binary_literal_bytes(expr),
        }
    }
    fn latin1_hex(expr: &Expr) -> Option<Vec<u8>> {
        match expr {
            Expr::Prefixed { prefix, value }
                if prefix.quote_style.is_none() && prefix.value.eq_ignore_ascii_case("_latin1") =>
            {
                match value.as_ref() {
                    Expr::Value(value) => match &value.value {
                        Value::HexStringLiteral(digits) => hex_bytes(digits).ok(),
                        _ => None,
                    },
                    _ => None,
                }
            }
            Expr::Nested(inner) => latin1_hex(inner),
            _ => None,
        }
    }
    fn compared_text(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Value(value) => match &value.value {
                Value::Number(text, false) => Some(format!("{text}e")),
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                    Some(text.clone())
                }
                Value::Null => Some(String::new()),
                _ => None,
            },
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => compared_text(expr).map(|text| format!("-{text}")),
            Expr::TypedString(_) => Some("0123456789-:. ".to_string()),
            Expr::Nested(inner) => compared_text(inner),
            Expr::Prefixed { prefix, value } if is_utf8_introducer(&prefix.value) => {
                match value.as_ref() {
                    Expr::Value(value) => match &value.value {
                        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                            Some(text.clone())
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }
            _ => latin1_hex(expr).map(|bytes| latin1_text(&bytes)),
        }
    }
    fn base_letter(character: char) -> Option<char> {
        if character.is_ascii() {
            return character
                .is_ascii_alphabetic()
                .then(|| character.to_ascii_lowercase());
        }
        if !('\u{c0}'..='\u{ff}').contains(&character) {
            return None;
        }
        Some(match character.to_lowercase().next()? {
            '\u{e0}'..='\u{e3}' => 'a',
            '\u{e7}' => 'c',
            '\u{e8}'..='\u{eb}' => 'e',
            '\u{ec}'..='\u{ef}' => 'i',
            '\u{f1}' => 'n',
            '\u{f2}'..='\u{f5}' => 'o',
            '\u{f9}'..='\u{fb}' => 'u',
            '\u{fd}' | '\u{ff}' => 'y',
            _ => return None,
        })
    }
    fn compares_exactly(operands: &[&Expr], needle: &Expr, positioned: bool) -> bool {
        let Some(texts) = operands
            .iter()
            .map(|operand| compared_text(operand))
            .collect::<Option<Vec<_>>>()
        else {
            return false;
        };
        let characters = texts
            .iter()
            .flat_map(|text| text.chars())
            .collect::<Vec<_>>();
        let mut non_ascii = false;
        for &character in &characters {
            if character.is_ascii() {
                continue;
            }
            non_ascii = true;
            let Some(base) = base_letter(character) else {
                return false;
            };
            if characters
                .iter()
                .any(|&other| other != character && base_letter(other) == Some(base))
            {
                return false;
            }
        }
        let empty_needle = !matches!(unnested(needle), Expr::Value(value) if value.value == Value::Null)
            && compared_text(needle).is_some_and(|text| text.is_empty());
        !(positioned && non_ascii && empty_needle)
    }
    fn field_compares_exactly(args: &[&Expr]) -> bool {
        reads_number("FIELD", 0, args)
            || (compares_exactly(args, args[0], false)
                && args
                    .iter()
                    .all(|arg| compared_text(arg).is_some_and(|text| !text.ends_with(' '))))
    }
    fn inserts_exactly(subject: &Expr, position: &Expr, replacement: &Expr) -> bool {
        fn integer_prefix(text: &str) -> f64 {
            let text = text.trim_start();
            let end = text
                .char_indices()
                .take_while(|&(index, character)| {
                    character.is_ascii_digit() || (index == 0 && matches!(character, '-' | '+'))
                })
                .count();
            text[..end].parse::<f64>().unwrap_or(0.0)
        }
        let Some(bytes) = latin1_hex(subject) else {
            return false;
        };
        let Some(start) = count(position) else {
            return false;
        };
        let truncated = match unnested(position) {
            Expr::Value(value) => match &value.value {
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                    integer_prefix(text)
                }
                _ => start.trunc(),
            },
            _ => start.trunc(),
        };
        let characters = bytes.len() as f64;
        let utf8_bytes = latin1_text(&bytes).len() as f64;
        [start.floor(), start.ceil(), truncated]
            .into_iter()
            .all(|start| start <= characters || start > utf8_bytes)
            && latin1_strings(replacement)
    }
    fn converts_to_subject(subject: &Expr, rest: &[&Expr]) -> bool {
        !mentions_latin1(subject) || rest.iter().all(|arg| latin1_strings(arg))
    }
    fn padded(subject: u64, length: f64) -> Option<u64> {
        if !(0.0..=PACKET_BYTES).contains(&length) {
            return Some(0);
        }
        let length = length.ceil() as u64;
        (length <= CHARACTER_LIMIT).then_some(subject.max(length))
    }
    fn characters(expr: &Expr, found: &mut bool) -> Option<u64> {
        let characters = match expr {
            Expr::Prefixed { prefix, value } => match value.as_ref() {
                Expr::Value(_) if latin1_hex(expr).is_some() => {
                    *found = true;
                    latin1_hex(expr)?.len() as u64 + 1
                }
                Expr::Value(value) if is_utf8_introducer(&prefix.value) => match &value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        text.chars().count() as u64
                    }
                    _ => return None,
                },
                _ => return None,
            },
            Expr::Value(value) => match &value.value {
                Value::Number(text, false) => text.len() as u64 + 32,
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                    text.chars().count() as u64
                }
                Value::Null => 0,
                _ => return None,
            },
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } if matches!(expr.as_ref(), Expr::Value(_)) => characters(expr, found)? + 1,
            Expr::Nested(inner) => characters(inner, found)?,
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                for position in [substring_from, substring_for].into_iter().flatten() {
                    characters(position, found)?;
                }
                characters(expr, found)?
            }
            Expr::Trim {
                expr,
                trim_what,
                trim_characters: None,
                ..
            } => {
                if let Some(what) = trim_what.as_deref() {
                    characters(what, found)?;
                    if !converts_to_subject(expr, &[what]) {
                        return None;
                    }
                }
                characters(expr, found)?
            }
            Expr::TypedString(_) => 32,
            Expr::BinaryOp { left, op, right } if is_arithmetic(op) => {
                characters(left, found)? + characters(right, found)? + 32
            }
            Expr::Cast {
                kind: CastKind::Cast,
                expr,
                data_type: DataType::JSON,
                array: false,
                format: None,
            } => characters(expr, found)? * 4 + 64,
            Expr::Position {
                expr: needle,
                r#in: haystack,
            } => {
                characters(needle, found)?;
                characters(haystack, found)?;
                if !compares_exactly(&[needle, haystack], needle, false) {
                    return None;
                }
                21
            }
            Expr::Function(function) => {
                let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
                    return None;
                };
                let FunctionArguments::List(list) = &function.args else {
                    return None;
                };
                if function.over.is_some()
                    || function.filter.is_some()
                    || !function.within_group.is_empty()
                    || list.duplicate_treatment.is_some()
                    || !list.clauses.is_empty()
                {
                    return None;
                }
                let mut args = Vec::with_capacity(list.args.len());
                for arg in &list.args {
                    let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) = arg else {
                        return None;
                    };
                    args.push(arg);
                }
                let upper_name = name.value.to_ascii_uppercase();
                let mut lengths = Vec::with_capacity(args.len());
                for (position, arg) in args.iter().enumerate() {
                    let binary = if reads_number(&upper_name, position, &args) {
                        negated_binary_bytes(arg)
                    } else if binary_converts_to_latin1_subject(&upper_name, position, &args) {
                        binary_literal_bytes(arg)
                    } else {
                        None
                    };
                    lengths.push(match binary {
                        Some(bytes) => bytes.len() as u64 + 20,
                        None => characters(arg, found)?,
                    });
                }
                let total = lengths.iter().sum::<u64>();
                match (upper_name.as_str(), args.as_slice(), lengths.as_slice()) {
                    ("CONCAT", _, _) => total,
                    ("CONCAT_WS", _, [separator, ..]) => total + separator * lengths.len() as u64,
                    ("ELT", _, _) => lengths.iter().copied().max().unwrap_or(0),
                    ("LOCATE", [needle, haystack, rest @ ..], _)
                        if rest.len() <= 1
                            && compares_exactly(&[needle, haystack], needle, !rest.is_empty()) =>
                    {
                        21
                    }
                    ("INSTR", [haystack, needle], _)
                        if compares_exactly(&[haystack, needle], needle, false) =>
                    {
                        21
                    }
                    ("FIELD", [_, _, ..], _) if field_compares_exactly(&args) => 21,
                    ("INSERT", [subject, ..], [text, _, _, replacement])
                        if !mentions_latin1(subject) =>
                    {
                        text + replacement
                    }
                    ("INSERT", [subject, position, _, replacement], [text, _, _, inserted])
                        if inserts_exactly(subject, position, replacement) =>
                    {
                        text + inserted
                    }
                    (
                        "LEFT" | "RIGHT" | "SUBSTR" | "SUBSTRING" | "MID" | "LTRIM" | "RTRIM"
                        | "REVERSE",
                        _,
                        [text, ..],
                    ) => *text,
                    ("LPAD" | "RPAD", [subject, length, padding], [text, _, _])
                        if converts_to_subject(subject, &[length, padding]) =>
                    {
                        padded(*text, count(length)?)?
                    }
                    ("REPLACE", [subject, from, to], [text, _, replacement])
                        if converts_to_subject(subject, &[from, to]) =>
                    {
                        text * replacement.max(&1)
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        (characters <= CHARACTER_LIMIT).then_some(characters)
    }
    let mut found = false;
    characters(expr, &mut found).is_some() && found
}

fn translate_value_expr(
    value: &Value,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    match value {
        Value::Placeholder(placeholder) if placeholder == "?" => {
            Ok(ast::Expr::Variable(context.next_parameter()?))
        }
        Value::Number(digits, false)
            if literal_context != Context::Bitwise && exceeds_unsigned_bigint(digits) =>
        {
            Ok(exact_decimal(digits, false))
        }
        Value::Number(digits, false)
            if matches!(literal_context, Context::Value | Context::Bytes)
                && exceeds_signed_bigint(digits) =>
        {
            Ok(exact_decimal(digits, false))
        }
        Value::Number(digits, false)
            if literal_context != Context::Bitwise && is_decimal_literal(digits) =>
        {
            Ok(match kept_digits(digits) {
                kept if exceeds_unsigned_bigint(&kept) => exact_decimal(&kept, false),
                kept => ast::Expr::Literal(ast::Literal::Numeric(kept)),
            })
        }
        other => translate_value(other, literal_context),
    }
}

/// The digits MySQL keeps of an exact literal: nine groups of nine digits,
/// the whole part first, so long fractions are cut off. A whole part longer
/// than that is the largest DECIMAL, 65 nines.
fn kept_digits(digits: &str) -> String {
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let significant = whole.trim_start_matches('0');
    let whole_digits = match (significant.len(), whole.is_empty()) {
        (0, false) => 1,
        (count, _) => count,
    };
    let whole_groups = whole_digits.div_ceil(DIGITS_PER_GROUP);
    if whole_groups > GROUPS_PER_DECIMAL {
        return "9".repeat(LARGEST_DECIMAL_DIGITS);
    }
    match fraction {
        Some(fraction) => {
            let kept = fraction
                .len()
                .min((GROUPS_PER_DECIMAL - whole_groups) * DIGITS_PER_GROUP);
            format!("{whole}.{}", &fraction[..kept])
        }
        None => whole.to_string(),
    }
}

const DIGITS_PER_GROUP: usize = 9;
const GROUPS_PER_DECIMAL: usize = 9;
const LARGEST_DECIMAL_DIGITS: usize = 65;

/// A literal MySQL reads as an exact DECIMAL that core does not type from
/// its digits: a negative decimal, or an integer above the BIGINT UNSIGNED
/// range. It becomes a CAST of its text to a DECIMAL as wide as its digits.
fn exact_decimal(digits: &str, negative: bool) -> ast::Expr {
    let digits = &kept_digits(digits);
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let scale = fraction.len() as u64;
    let precision = (whole.trim_start_matches('0').len() as u64 + scale).max(1);
    let sign = if negative { "-" } else { "" };
    ast::Expr::cast(
        ast::Expr::Literal(ast::Literal::String(quote_string(&format!(
            "{sign}{digits}"
        )))),
        Some(sized_decimal_type(precision, scale)),
    )
}

fn exceeds_unsigned_bigint(digits: &str) -> bool {
    let significant = digits.trim_start_matches('0');
    significant.bytes().all(|byte| byte.is_ascii_digit())
        && (significant.len() > 20
            || (significant.len() == 20 && significant > "18446744073709551615"))
}

fn exceeds_signed_bigint(digits: &str) -> bool {
    digits
        .parse::<u64>()
        .is_ok_and(|value| value > i64::MAX as u64)
}

fn exceeds_negated_bigint(digits: &str) -> bool {
    digits
        .parse::<u64>()
        .is_ok_and(|value| value > i64::MIN.unsigned_abs())
}

fn is_decimal_literal(digits: &str) -> bool {
    digits.split_once('.').is_some_and(|(whole, fraction)| {
        !(whole.is_empty() && fraction.is_empty())
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn translate_nested(
    inner: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(parenthesized(translate_in(
        inner,
        literal_context,
        context,
    )?))
}

fn translate_identifier_expr(identifier: &Ident) -> Result<ast::Expr, MysqlParserError> {
    if identifier.quote_style.is_none() {
        let upper_name = identifier.value.to_ascii_uppercase();
        if let Some(literal) = current_time_literal(&upper_name) {
            return Ok(literal);
        }
    }
    Ok(ast::Expr::Id(column_name(identifier)?))
}

fn translate_compound_identifier(parts: &[Ident]) -> Result<ast::Expr, MysqlParserError> {
    match parts {
        [table, column] => Ok(ast::Expr::Qualified(
            column_name(table)?,
            column_name(column)?,
        )),
        [schema, table, column] => Ok(ast::Expr::DoublyQualified(
            column_name(schema)?,
            column_name(table)?,
            column_name(column)?,
        )),
        _ => Err(MysqlParserError::Unsupported(format!(
            "column reference `{}` is not lowered yet",
            ObjectName::from(parts.to_vec())
        ))),
    }
}

fn column_name(identifier: &Ident) -> Result<ast::Name, MysqlParserError> {
    if identifier.quote_style.is_none() && identifier.value.starts_with('@') {
        return Err(MysqlParserError::Unsupported(format!(
            "variable `{identifier}` is not lowered yet"
        )));
    }
    translate_identifier(identifier)
}

fn translate_unary(
    op: UnaryOperator,
    expr: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let sign_context = if literal_context == Context::Bitwise {
        Context::Bitwise
    } else {
        Context::Arithmetic
    };
    let (op, operand_context) = match op {
        UnaryOperator::Minus if sign_context != Context::Bitwise => {
            if let Some(literal) = hex_or_bit_literal(expr) {
                return match literal_context {
                    Context::Number | Context::Text | Context::Bytes => {
                        negated_binary_literal(literal)
                    }
                    _ => Err(decimals_of_negated_binary_literal_show(expr)),
                };
            }
            if temporal_has_fraction(expr) {
                return Err(temporal_operand(expr, "unary `-`"));
            }
            if let Some(digits) = number_literal(expr) {
                if is_decimal_literal(digits)
                    || exceeds_unsigned_bigint(digits)
                    || exceeds_negated_bigint(digits)
                {
                    return Ok(exact_decimal(digits, true));
                }
                if digits.contains(['e', 'E']) {
                    return Ok(ast::Expr::Literal(ast::Literal::Numeric(format!(
                        "-{digits}"
                    ))));
                }
            }
            if json::is_json_value(expr) {
                return json::translate_negation(expr, context);
            }
            if number_literal(expr).is_none() {
                let negated = translate_in(expr, sign_context, context)?;
                return Ok(times_minus_one(negated));
            }
            (ast::UnaryOperator::Negative, sign_context)
        }
        UnaryOperator::Minus => (ast::UnaryOperator::Negative, sign_context),
        UnaryOperator::Plus => return translate_in(expr, sign_context, context),
        UnaryOperator::Not | UnaryOperator::BangNot => {
            return Ok(ast::Expr::unary(
                ast::UnaryOperator::Not,
                translate_condition(expr, context)?,
            ))
        }
        other => {
            return Err(MysqlParserError::Unsupported(format!(
                "unary operator `{other}` is not lowered yet"
            )))
        }
    };
    Ok(ast::Expr::unary(
        op,
        translate_in(expr, operand_context, context)?,
    ))
}

/// `-x` as `x * -1`, which gives the same value, so that the product has
/// the type and the scale of `x` when `x` is a DECIMAL.
pub(crate) fn times_minus_one(expr: ast::Expr) -> ast::Expr {
    let minus_one = ast::Expr::unary(
        ast::UnaryOperator::Negative,
        ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
    );
    parenthesized(ast::Expr::binary(
        parenthesized(expr),
        ast::Operator::Multiply,
        minus_one,
    ))
}

pub(crate) fn number_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(digits, false) => Some(digits),
            _ => None,
        },
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr: inner,
        } => number_literal(inner),
        _ => None,
    }
}

fn translate_binary(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    parent_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let arithmetic = |operator, context: &mut TranslationContext| {
        arithmetic(left, operator, right, parent_context, context)
    };
    if matches!(op, BinaryOperator::Modulo | BinaryOperator::MyIntegerDivide)
        && (is_binary_string(left) || is_binary_string(right))
    {
        return Err(binary_string_outside_arithmetic(op));
    }
    match op {
        BinaryOperator::Plus | BinaryOperator::Minus
            if matches!(left, Expr::Interval(_)) || matches!(right, Expr::Interval(_)) =>
        {
            translate_interval_arithmetic(left, op, right, context)
        }
        BinaryOperator::Modulo
            if temporal_type(left).is_some() || temporal_type(right).is_some() =>
        {
            let operand = if temporal_type(left).is_some() {
                left
            } else {
                right
            };
            Err(temporal_operand(operand, &format!("`{op}`")))
        }
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo
            if json::is_json_value(left) || json::is_json_value(right) =>
        {
            let left = json::translate_arithmetic_operand(left, context)?;
            let right = json::translate_arithmetic_operand(right, context)?;
            let operator = match op {
                BinaryOperator::Plus => ast::Operator::Add,
                BinaryOperator::Minus => ast::Operator::Subtract,
                BinaryOperator::Multiply => ast::Operator::Multiply,
                BinaryOperator::Divide => ast::Operator::Divide,
                _ => return Ok(function_call("__mysql_json_remainder", vec![left, right])),
            };
            Ok(ast::Expr::binary(left, operator, right))
        }
        BinaryOperator::And | BinaryOperator::Or | BinaryOperator::StringConcat => {
            let operator = match op {
                BinaryOperator::And => ast::Operator::And,
                _ => ast::Operator::Or,
            };
            Ok(ast::Expr::binary(
                translate_condition(left, context)?,
                operator,
                translate_condition(right, context)?,
            ))
        }
        BinaryOperator::Plus => arithmetic(ast::Operator::Add, context),
        BinaryOperator::Minus => arithmetic(ast::Operator::Subtract, context),
        BinaryOperator::Multiply => arithmetic(ast::Operator::Multiply, context),
        BinaryOperator::Divide => arithmetic(ast::Operator::Divide, context),
        BinaryOperator::Modulo => arithmetic(ast::Operator::Modulus, context),
        BinaryOperator::Eq => comparison(left, ast::Operator::Equals, right, context),
        BinaryOperator::NotEq => comparison(left, ast::Operator::NotEquals, right, context),
        BinaryOperator::Lt => comparison(left, ast::Operator::Less, right, context),
        BinaryOperator::LtEq => comparison(left, ast::Operator::LessEquals, right, context),
        BinaryOperator::Gt => comparison(left, ast::Operator::Greater, right, context),
        BinaryOperator::GtEq => comparison(left, ast::Operator::GreaterEquals, right, context),
        BinaryOperator::Spaceship => comparison(left, ast::Operator::Is, right, context),
        BinaryOperator::MyIntegerDivide => translate_integer_division(left, right, context),
        BinaryOperator::Xor => translate_logical_xor(left, right, context),
        BinaryOperator::Arrow => translate_json_arrow(left, right, false, context),
        BinaryOperator::LongArrow => translate_json_arrow(left, right, true, context),
        other => Err(unsupported_operator(other)),
    }
}

fn translate_integer_division(
    left: &Expr,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if exceeds_bigint(left) || exceeds_bigint(right) {
        return Err(MysqlParserError::Unsupported(
            "DIV of a number outside the BIGINT range, which MySQL rejects at run time".to_string(),
        ));
    }
    let operand = |expr: &Expr, context: &mut TranslationContext| {
        if json::is_json_value(expr) {
            json::translate_numeric_coercion(expr, "__mysql_json_decimal", context)
        } else {
            translate_in(expr, Context::Arithmetic, context)
        }
    };
    let quotient = ast::Expr::binary(
        operand(left, context)?,
        ast::Operator::Divide,
        operand(right, context)?,
    );
    Ok(ast::Expr::cast(quotient, Some(type_name("INTEGER"))))
}

const BIGINT_LIMIT: f64 = 9_223_372_036_854_775_808.0;

fn exceeds_bigint(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(number, _) => number
                .parse::<f64>()
                .map_or(true, |number| number.abs() >= BIGINT_LIMIT),
            _ => false,
        },
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => exceeds_bigint(inner),
        _ => false,
    }
}

fn operation(
    left: &Expr,
    operator: ast::Operator,
    right: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let lhs = translate_in(left, literal_context, context)?;
    let rhs = translate_in(right, literal_context, context)?;
    Ok(ast::Expr::binary(lhs, operator, rhs))
}

fn arithmetic(
    left: &Expr,
    operator: ast::Operator,
    right: &Expr,
    parent_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Some(unsigned) = unsigned_arithmetic(left, operator, right, parent_context, context)? {
        return Ok(unsigned);
    }
    if operator == ast::Operator::Modulus {
        return operation(left, operator, right, Context::Arithmetic, context);
    }
    let lhs = arithmetic_operand(left, right, context)?;
    let rhs = arithmetic_operand(right, left, context)?;
    Ok(ast::Expr::binary(lhs, operator, rhs))
}

fn unsigned_arithmetic(
    left: &Expr,
    operator: ast::Operator,
    right: &Expr,
    parent_context: Context,
    context: &mut TranslationContext,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    let (left_binary, right_binary) = (hex_or_bit_literal(left), hex_or_bit_literal(right));
    if left_binary.is_none() && right_binary.is_none() {
        return Ok(None);
    }
    let left_kind = OperandKind::of(left)?;
    let right_kind = OperandKind::of(right)?;
    let (Some(left_kind), Some(right_kind)) = (left_kind, right_kind) else {
        return Ok(None);
    };
    let keeps_unsigned = !matches!(left_kind, OperandKind::Decimal | OperandKind::Null)
        && !matches!(right_kind, OperandKind::Decimal | OperandKind::Null)
        && operator != ast::Operator::Divide;
    if keeps_unsigned {
        let (OperandKind::Integer(lhs), OperandKind::Integer(rhs)) = (left_kind, right_kind) else {
            return Ok(None);
        };
        if parent_context == Context::Arithmetic {
            return Err(MysqlParserError::Unsupported(format!(
                "`{left} {operator} {right}` inside arithmetic, where its BIGINT UNSIGNED type changes the result, is not lowered yet"
            )));
        }
        let result = match operator {
            ast::Operator::Add => lhs.checked_add(rhs),
            ast::Operator::Subtract => lhs.checked_sub(rhs),
            ast::Operator::Multiply => lhs.checked_mul(rhs),
            ast::Operator::Modulus => Some(if rhs == 0 { 0 } else { lhs % rhs }),
            _ => return Ok(None),
        };
        if !result.is_some_and(|result| (0..=i128::from(i64::MAX)).contains(&result)) {
            return Err(MysqlParserError::Unsupported(format!(
                "`{left} {operator} {right}`, which MySQL computes as BIGINT UNSIGNED outside the BIGINT range, is not lowered yet"
            )));
        }
    }
    let lhs = unsigned_operand(left, left_binary, context)?;
    let rhs = unsigned_operand(right, right_binary, context)?;
    Ok(Some(ast::Expr::binary(lhs, operator, rhs)))
}

fn unsigned_operand(
    expr: &Expr,
    binary: Option<&Value>,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let Some(literal) = binary else {
        return translate_in(expr, Context::Arithmetic, context);
    };
    let value = i64::try_from(binary_literal_value(literal)?).map_err(|_| {
        MysqlParserError::Unsupported(format!(
            "`{expr}` above the BIGINT range in arithmetic is not lowered yet"
        ))
    })?;
    Ok(ast::Expr::Literal(ast::Literal::Numeric(value.to_string())))
}

#[derive(Clone, Copy)]
enum OperandKind {
    Integer(i128),
    Decimal,
    Null,
}

impl OperandKind {
    fn of(expr: &Expr) -> Result<Option<OperandKind>, MysqlParserError> {
        if let Some(literal) = hex_or_bit_literal(expr) {
            return Ok(Some(OperandKind::Integer(i128::from(
                binary_literal_value(literal)?,
            ))));
        }
        if is_null_literal(expr) {
            return Ok(Some(OperandKind::Null));
        }
        let Some(digits) = signed_number_literal(expr) else {
            return Ok(None);
        };
        if is_decimal_literal(digits) || exceeds_unsigned_bigint(digits) {
            return Ok(Some(OperandKind::Decimal));
        }
        let Ok(magnitude) = digits.parse::<i64>() else {
            return Ok(None);
        };
        let value = i128::from(magnitude);
        Ok(Some(OperandKind::Integer(if is_negated(expr) {
            -value
        } else {
            value
        })))
    }
}

fn is_null_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(value.value, Value::Null),
        Expr::Nested(inner) => is_null_literal(inner),
        _ => false,
    }
}

fn arithmetic_operand(
    expr: &Expr,
    other_operand: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if !is_double_literal(other_operand) && !is_text_literal(other_operand) {
        return translate_in(expr, Context::Arithmetic, context);
    }
    if let Some(literal) = negated_binary(expr) {
        return negated_binary_literal(literal);
    }
    match hex_or_bit_literal(expr) {
        Some(literal) => binary_literal_as_double(literal),
        None => translate_in(expr, Context::Arithmetic, context),
    }
}

fn negated_binary(expr: &Expr) -> Option<&Value> {
    match expr {
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => hex_or_bit_literal(expr),
        Expr::Nested(inner) => negated_binary(inner),
        _ => None,
    }
}

fn decimals_of_negated_binary_literal_show(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "`{expr}`, a DOUBLE without decimals whose decimals MySQL can show here, is not lowered yet"
    ))
}

fn hides_decimals(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::Null
                | Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::NationalStringLiteral(_)
                | Value::HexStringLiteral(_)
                | Value::SingleQuotedByteStringLiteral(_)
        ),
        Expr::Prefixed { .. } => true,
        Expr::Nested(inner) => hides_decimals(inner),
        _ => is_double_literal(expr) || negated_binary(expr).is_some(),
    }
}

fn translate_logical_xor(
    left: &Expr,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let lhs = negation(translate_condition(left, context)?);
    let rhs = negation(translate_condition(right, context)?);
    Ok(parenthesized(ast::Expr::binary(
        lhs,
        ast::Operator::NotEquals,
        rhs,
    )))
}

fn translate_json_arrow(
    left: &Expr,
    right: &Expr,
    unquote: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let document = translate_expr(left, context)?;
    let path = translate_expr(right, context)?;
    let extracted = function_call("JSON_EXTRACT", vec![document, path]);
    Ok(if unquote {
        function_call("JSON_UNQUOTE", vec![extracted])
    } else {
        extracted
    })
}

fn comparison(
    left: &Expr,
    operator: ast::Operator,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let explicit = single_explicit_collation(&[left, right])?;
    if json::is_json_value(left) || json::is_json_value(right) {
        return json::translate_comparison(left, operator, right, context);
    }
    let lhs = translate_compared(left, right, context)?;
    let rhs = translate_compared(right, left, context)?;
    let lhs = default_collated(lhs, explicit, &[left, right]);
    Ok(ast::Expr::binary(lhs, operator, rhs))
}

fn default_collated(lhs: ast::Expr, explicit: Option<String>, operands: &[&Expr]) -> ast::Expr {
    let literals_only = operands
        .iter()
        .all(|operand| collation_of(operand) == Some(DEFAULT_COLLATION));
    if explicit.is_none() && literals_only {
        let sequence = collation_sequence(DEFAULT_COLLATION).expect("the default collation maps");
        ast::Expr::collate(lhs, ast::Name::exact(sequence.to_string()))
    } else {
        lhs
    }
}

fn translate_compared(
    expr: &Expr,
    other_side: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if is_json_cast(expr) {
        return Err(json_outside_a_value_position(expr));
    }
    if is_temporal_cast(expr) {
        return Err(temporal_outside_a_value_position(expr));
    }
    translate_compared_value(expr, other_side, context)
}

fn translate_compared_value(
    expr: &Expr,
    other_side: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let (Some(own), Some(other)) = (temporal_type(expr), temporal_type(other_side)) {
        let literal_context = match (own, other) {
            (DataType::Time(..), DataType::Time(..)) => Context::Number,
            (DataType::Date, DataType::Date) => Context::Value,
            (DataType::Timestamp(..), DataType::Timestamp(..))
                if fraction_length(expr) == fraction_length(other_side) =>
            {
                Context::Value
            }
            _ => {
                return Err(MysqlParserError::Unsupported(format!(
                    "comparison of `{expr}` with `{other_side}`, which MySQL compares as DATETIME values, is not lowered yet"
                )))
            }
        };
        return translate_in(expr, literal_context, context);
    }
    if let Some(data_type) = temporal_type(other_side) {
        if let Some(text) = string_literal(expr) {
            return translate_temporal_text(data_type, text);
        }
        if number_literal(expr)
            .is_some_and(|digits| is_decimal_literal(digits) && same_double(digits, other_side))
        {
            return Err(MysqlParserError::Unsupported(format!(
                "comparison of `{other_side}` with the DECIMAL `{expr}`, which MySQL compares as DOUBLE, is not lowered yet"
            )));
        }
        if is_literal(expr)
            && temporal_type(expr).is_none()
            && comparison_context(expr) != Context::Number
        {
            return Err(MysqlParserError::Unsupported(format!(
                "comparison of `{other_side}` with `{expr}`, which MySQL converts to a temporal value, is not lowered yet"
            )));
        }
    }
    translate_in(expr, comparison_context(other_side), context)
}

fn same_double(digits: &str, temporal: &Expr) -> bool {
    let Expr::TypedString(typed) = temporal else {
        return temporal_type(temporal).is_some();
    };
    let value = match translate_typed_string(typed, Context::Number) {
        Ok(ast::Expr::Literal(ast::Literal::Numeric(number))) => number.parse::<f64>().ok(),
        Ok(ast::Expr::Unary(ast::UnaryOperator::Negative, inner)) => match *inner {
            ast::Expr::Literal(ast::Literal::Numeric(number)) => {
                number.parse::<f64>().ok().map(|number| -number)
            }
            _ => None,
        },
        _ => None,
    };
    value.is_none_or(|value| digits.parse::<f64>().ok() == Some(value))
}

fn comparison_context(other_side: &Expr) -> Context {
    match other_side {
        Expr::Value(value) => match &value.value {
            Value::Number(..) | Value::Boolean(_) => Context::Number,
            Value::SingleQuotedString(_)
            | Value::DoubleQuotedString(_)
            | Value::NationalStringLiteral(_) => Context::Text,
            _ => Context::Value,
        },
        Expr::Prefixed { prefix, .. } if !prefix.value.eq_ignore_ascii_case("_binary") => {
            Context::Text
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus | UnaryOperator::BitwiseNot,
            ..
        } => Context::Number,
        Expr::BinaryOp {
            op:
                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo
                | BinaryOperator::MyIntegerDivide
                | BinaryOperator::BitwiseOr
                | BinaryOperator::BitwiseAnd
                | BinaryOperator::BitwiseXor
                | BinaryOperator::PGBitwiseShiftLeft
                | BinaryOperator::PGBitwiseShiftRight,
            ..
        } => Context::Number,
        Expr::Nested(inner) => comparison_context(inner),
        _ => Context::Value,
    }
}

fn truth_test(
    expr: &Expr,
    operator: ast::Operator,
    truth: ast::Literal,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(ast::Expr::binary(
        translate_condition(expr, context)?,
        operator,
        ast::Expr::Literal(truth),
    ))
}

fn translate_null_test(
    inner: &Expr,
    not_null: bool,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    context.unsigned_as_text = is_bit_operation(inner);
    let inner = translate_in(inner, literal_context, context);
    context.unsigned_as_text = false;
    let inner = Box::new(inner?);
    Ok(if not_null {
        ast::Expr::NotNull(inner)
    } else {
        ast::Expr::IsNull(inner)
    })
}

fn translate_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let operands = std::iter::once(expr).chain(list).collect::<Vec<_>>();
    let explicit = single_explicit_collation(&operands)?;
    let lhs = match list.first() {
        Some(first) => translate_compared(expr, first, context)?,
        None => translate_expr(expr, context)?,
    };
    let lhs = default_collated(lhs, explicit, &operands);
    let rhs = list
        .iter()
        .map(|item| translate_compared(item, expr, context).map(Box::new))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::Expr::InList {
        lhs: Box::new(lhs),
        not: negated,
        rhs,
    })
}

fn translate_in_subquery(
    expr: &Expr,
    subquery: &Query,
    negated: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let lhs = translate_expr(expr, context)?;
    Ok(ast::Expr::in_select(
        lhs,
        negated,
        translate_subquery(subquery, context)?,
    ))
}

fn translate_between(
    expr: &Expr,
    negated: bool,
    low: &Expr,
    high: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let explicit = single_explicit_collation(&[expr, low, high])?;
    let lhs = translate_compared(expr, low, context)?;
    let lhs = default_collated(lhs, explicit, &[expr, low, high]);
    let start = translate_compared(low, expr, context)?;
    let end = translate_compared(high, expr, context)?;
    Ok(ast::Expr::between(lhs, negated, start, end))
}

fn is_json_cast(expr: &Expr) -> bool {
    match expr {
        Expr::Cast {
            data_type: DataType::JSON,
            ..
        }
        | Expr::Convert {
            data_type: Some(DataType::JSON),
            ..
        } => true,
        Expr::Nested(inner) => is_json_cast(inner),
        _ => false,
    }
}

fn temporal_type(expr: &Expr) -> Option<&DataType> {
    match expr {
        Expr::TypedString(typed) => Some(&typed.data_type),
        Expr::Nested(inner) => temporal_type(inner),
        _ => None,
    }
}

fn is_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => !matches!(value.value, Value::Null | Value::Placeholder(_)),
        Expr::Prefixed { .. } | Expr::TypedString(_) => true,
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => is_literal(inner),
        _ => false,
    }
}

fn translate_regexp(
    expr: &Expr,
    negated: bool,
    pattern: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    ensure_default_collation(&[expr, pattern], "REGEXP")?;
    if string_literal(pattern).is_some_and(str::is_empty) {
        return Err(MysqlParserError::Unsupported(
            "an empty regular expression, which MySQL rejects at run time, is not lowered yet"
                .to_string(),
        ));
    }
    if is_binary_literal(expr) || is_binary_literal(pattern) {
        return Err(MysqlParserError::Unsupported(
            "a regular expression over a binary string, which MySQL rejects, is not lowered yet"
                .to_string(),
        ));
    }
    let lhs = translate_regexp_operand(expr, context)?;
    let rhs = translate_regexp_operand(pattern, context)?;
    Ok(ast::Expr::like(
        lhs,
        negated,
        ast::LikeOperator::Regexp,
        rhs,
        None,
    ))
}

fn translate_regexp_operand(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let literal_context = if is_json_cast(expr) {
        Context::Value
    } else {
        Context::Text
    };
    translate_in(expr, literal_context, context)
}

fn is_binary_string(expr: &Expr) -> bool {
    match expr {
        Expr::Prefixed { prefix, .. } => prefix.value.eq_ignore_ascii_case("_binary"),
        Expr::Nested(inner) => is_binary_string(inner),
        _ => false,
    }
}

fn binary_string_outside_arithmetic(op: &dyn std::fmt::Display) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "a binary string as an operand of `{op}`, which MySQL computes on its bytes or as a DOUBLE Turso truncates, is not lowered yet"
    ))
}

pub(crate) fn is_binary_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_)
        ),
        Expr::Prefixed { prefix, .. } => prefix.value.eq_ignore_ascii_case("_binary"),
        Expr::Nested(inner) => is_binary_literal(inner),
        _ => false,
    }
}

fn translate_case(
    operand: Option<&Expr>,
    conditions: &[CaseWhen],
    else_result: Option<&Expr>,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let base = match (operand, conditions.first()) {
        (Some(operand), Some(first)) => Some(Box::new(translate_compared(
            operand,
            &first.condition,
            context,
        )?)),
        (Some(operand), None) => Some(Box::new(translate_expr(operand, context)?)),
        (None, _) => None,
    };
    let results: Vec<&Expr> = conditions
        .iter()
        .map(|case| &case.result)
        .chain(else_result)
        .collect();
    let results = Results::new(&results, literal_context);
    let when_then_pairs = conditions
        .iter()
        .map(|case| {
            let when = match operand {
                Some(operand) => translate_compared(&case.condition, operand, context)?,
                None => translate_condition(&case.condition, context)?,
            };
            let then = translate_result(&case.result, results, context)?;
            Ok((Box::new(when), Box::new(then)))
        })
        .collect::<Result<Vec<_>, MysqlParserError>>()?;
    let else_expr = else_result
        .map(|result| translate_result(result, results, context).map(Box::new))
        .transpose()?;
    Ok(ast::Expr::Case {
        base,
        when_then_pairs,
        else_expr,
    })
}

#[derive(Clone, Copy)]
struct Results {
    doubles: bool,
    hide_decimals: bool,
    literal_context: Context,
}

impl Results {
    fn new(results: &[&Expr], parent_context: Context) -> Self {
        Results {
            doubles: results.iter().any(|result| is_double_literal(result))
                && !results.iter().any(|result| is_string_literal(result)),
            hide_decimals: results.iter().all(|result| hides_decimals(result)),
            literal_context: if parent_context == Context::Bytes {
                Context::Bytes
            } else {
                Context::Value
            },
        }
    }
}

/// Translates one result of a CASE, IF, IFNULL or COALESCE. When another
/// result is a DOUBLE literal and none is a string literal, MySQL computes them
/// all as DOUBLE, so an exact number literal becomes the same number written as
/// a DOUBLE.
fn translate_result(
    expr: &Expr,
    results: Results,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Some(literal) = negated_binary(expr) {
        return if results.hide_decimals {
            negated_binary_literal(literal)
        } else {
            Err(decimals_of_negated_binary_literal_show(expr))
        };
    }
    match signed_number_literal(expr) {
        Some(digits) if results.doubles && !digits.contains(['e', 'E']) => {
            let double = ast::Expr::Literal(ast::Literal::Numeric(format!("{digits}e0")));
            Ok(if is_negated(expr) {
                ast::Expr::unary(ast::UnaryOperator::Negative, double)
            } else {
                double
            })
        }
        _ => translate_in(expr, results.literal_context, context),
    }
}

fn is_string_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::HexStringLiteral(_)
                | Value::SingleQuotedByteStringLiteral(_)
        ),
        Expr::Collate { expr, .. } => is_string_literal(expr),
        Expr::Prefixed { .. } => true,
        _ => false,
    }
}

fn is_text_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::NationalStringLiteral(_)
        ),
        Expr::Nested(inner) => is_text_literal(inner),
        _ => false,
    }
}

fn is_double_literal(expr: &Expr) -> bool {
    signed_number_literal(expr).is_some_and(|digits| digits.contains(['e', 'E']))
}

fn signed_number_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: inner,
        } => signed_number_literal(inner),
        _ => number_literal(expr),
    }
}

fn is_negated(expr: &Expr) -> bool {
    match expr {
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => !is_negated(expr),
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr: inner,
        } => is_negated(inner),
        _ => false,
    }
}

fn translate_exists(
    subquery: &Query,
    negated: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let exists = ast::Expr::Exists(Box::new(translate_subquery(subquery, context)?));
    Ok(if negated {
        ast::Expr::unary(ast::UnaryOperator::Not, exists)
    } else {
        exists
    })
}

fn translate_tuple(
    items: &[Expr],
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if items.iter().any(is_row_value) {
        return Err(MysqlParserError::Unsupported(
            "a row value inside a row value is not lowered yet".to_string(),
        ));
    }
    Ok(ast::Expr::Parenthesized(
        items
            .iter()
            .map(|item| translate_expr(item, context).map(Box::new))
            .collect::<Result<Vec<_>, _>>()?,
    ))
}

fn is_row_value(expr: &Expr) -> bool {
    match expr {
        Expr::Tuple(_) => true,
        Expr::Nested(inner) => is_row_value(inner),
        _ => false,
    }
}

fn translate_convert(
    expr: &Expr,
    data_type: Option<&DataType>,
    charset: Option<&ObjectName>,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    match (data_type, charset) {
        (Some(data_type), None) => translate_cast(expr, data_type, literal_context, context),
        (None, Some(charset)) => translate_convert_using(expr, charset, context),
        _ => Err(unsupported(expr)),
    }
}

fn translate_member_of(
    value: &Expr,
    array: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let value = translate_json_value(value, context)?;
    let array = translate_expr(array, context)?;
    Ok(function_call("__mysql_member_of", vec![value, array]))
}

fn translate_extract(
    field: &DateTimeField,
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let unit = unit_literal(field)?;
    let date = translate_expr(expr, context)?;
    Ok(function_call("EXTRACT", vec![unit, date]))
}

fn translate_single_argument_call(
    name: &str,
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(function_call(
        name,
        vec![translate_in(
            expr,
            argument_context(name, 0, &[expr]),
            context,
        )?],
    ))
}

fn translate_position(
    needle: &Expr,
    haystack: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    ensure_default_collation(&[needle, haystack], "POSITION")?;
    let needle = translate_bytes(needle, context)?;
    let haystack = translate_bytes(haystack, context)?;
    Ok(function_call("LOCATE", vec![needle, haystack]))
}

fn translate_substring(
    expr: &Expr,
    from: Option<&Expr>,
    length: Option<&Expr>,
    shorthand: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let name = if shorthand { "SUBSTR" } else { "SUBSTRING" };
    let parts: Vec<&Expr> = [Some(expr), from, length].into_iter().flatten().collect();
    let args = parts
        .iter()
        .enumerate()
        .map(|(position, part)| {
            translate_in(part, argument_context(name, position, &parts), context)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(function_call(name, args))
}

fn negation(expr: ast::Expr) -> ast::Expr {
    parenthesized(ast::Expr::unary(ast::UnaryOperator::Not, expr))
}

pub(crate) fn parenthesized(expr: ast::Expr) -> ast::Expr {
    ast::Expr::Parenthesized(vec![Box::new(expr)])
}

fn translate_like(
    expr: &Expr,
    negated: bool,
    pattern: &Expr,
    escape: Option<&sqlparser::ast::ValueWithSpan>,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    ensure_default_collation(&[expr, pattern], "LIKE")?;
    let escape = match escape {
        Some(escape) => match &escape.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text)
                if text.chars().count() == 1 =>
            {
                text.chars().next()
            }
            other => {
                return Err(MysqlParserError::Unsupported(format!(
                    "LIKE escape `{other}` is not lowered yet"
                )))
            }
        },
        None => Some('\\'),
    };
    if json::is_json_value(expr) || json::is_json_value(pattern) {
        let escape = escape.expect("a LIKE escape is one character");
        return json::translate_like(expr, negated, pattern, escape, context);
    }
    if is_binary_literal(expr) || is_binary_literal(pattern) {
        let escape = escape.expect("a LIKE escape is one character");
        let mut operand = |operand: &Expr| {
            if is_binary_literal(operand) {
                translate_bytes(operand, context)
            } else {
                translate_in(operand, Context::Text, context)
            }
        };
        let matched = function_call(
            "__mysql_like_bin",
            vec![
                operand(expr)?,
                operand(pattern)?,
                string_expr(&escape.to_string()),
            ],
        );
        return Ok(if negated {
            ast::Expr::unary(ast::UnaryOperator::Not, matched)
        } else {
            matched
        });
    }
    let lhs = translate_in(expr, Context::Text, context)?;
    let (rhs, escape) = match (string_literal(pattern), escape) {
        (Some(text), Some(escape)) if !text.contains(escape) => (string_expr(text), None),
        (Some(text), Some(escape)) => (
            string_expr(&with_dangling_escape_doubled(text, escape)),
            Some(escape),
        ),
        (_, escape) => (translate_in(pattern, Context::Text, context)?, escape),
    };
    let escape = escape.map(|escape| string_expr(&escape.to_string()));
    Ok(ast::Expr::like(
        lhs,
        negated,
        ast::LikeOperator::Like,
        rhs,
        escape,
    ))
}

pub(crate) fn string_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text)
            | Value::DoubleQuotedString(text)
            | Value::NationalStringLiteral(text) => Some(text),
            _ => None,
        },
        Expr::Nested(inner) => string_literal(inner),
        _ => None,
    }
}

fn string_expr(text: &str) -> ast::Expr {
    ast::Expr::Literal(ast::Literal::String(quote_string(text)))
}

fn with_dangling_escape_doubled(pattern: &str, escape: char) -> String {
    let trailing = pattern.chars().rev().take_while(|ch| *ch == escape).count();
    if trailing % 2 == 1 {
        format!("{pattern}{escape}")
    } else {
        pattern.to_string()
    }
}

fn translate_quantified_subquery(
    left: &Expr,
    not: bool,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let Expr::Subquery(subquery) = right else {
        return Err(unsupported(right));
    };
    let lhs = translate_expr(left, context)?;
    Ok(ast::Expr::in_select(
        lhs,
        not,
        translate_subquery(subquery, context)?,
    ))
}

fn translate_cast(
    expr: &Expr,
    data_type: &DataType,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let numeric = matches!(
        data_type,
        DataType::Signed
            | DataType::SignedInteger
            | DataType::Unsigned
            | DataType::UnsignedInteger
            | DataType::Double(ExactNumberInfo::None)
            | DataType::Real
            | DataType::Float(ExactNumberInfo::None)
            | DataType::Decimal(_)
    );
    if numeric && json::is_json_value(expr) {
        if let Some(number) = json::number_cast_operand(expr) {
            return translate_cast(number, data_type, literal_context, context);
        }
        return translate_json_numeric_cast(expr, data_type, context);
    }
    let shows_text = match data_type {
        DataType::Char(_) | DataType::Character(_) => true,
        DataType::Custom(name, _) => is_nchar(name),
        _ => false,
    };
    if let Some(text) = json::temporal_json_text(expr).filter(|_| shows_text) {
        return Ok(match data_type {
            DataType::Char(Some(length)) | DataType::Character(Some(length)) => function_call(
                "__mysql_cast_as_char",
                vec![text, number(character_length(length)?)],
            ),
            DataType::Custom(_, modifiers) => match modifiers.as_slice() {
                [] => ast::Expr::cast(text, Some(type_name("NCHAR"))),
                [length] => function_call(
                    "__mysql_cast_as_nchar",
                    vec![text, number(nchar_length(length)?)],
                ),
                _ => return Err(unsupported_cast(data_type)),
            },
            _ => ast::Expr::cast(text, Some(type_name("CHAR"))),
        });
    }
    let (target, operand_context) = match data_type {
        DataType::Signed | DataType::SignedInteger => {
            return translate_integer_cast(expr, "__mysql_cast_as_signed", context)
        }
        DataType::Unsigned | DataType::UnsignedInteger => {
            return translate_integer_cast(expr, "__mysql_cast_as_unsigned", context)
        }
        DataType::Double(ExactNumberInfo::None) => (type_name("DOUBLE"), Context::Number),
        DataType::Real => (type_name("REAL"), Context::Number),
        DataType::Float(ExactNumberInfo::None) => (type_name("FLOAT"), Context::Number),
        DataType::Decimal(precision) => (decimal_type(precision), Context::Number),
        DataType::Char(None) | DataType::Character(None) => (type_name("CHAR"), Context::Value),
        DataType::JSON => return translate_cast_as_json(expr, literal_context, context),
        DataType::Date => {
            return translate_temporal_cast(expr, TemporalCast::Date, literal_context, context)
        }
        DataType::Datetime(precision) => {
            let target = TemporalCast::DateTime(precision.unwrap_or(0));
            return translate_temporal_cast(expr, target, literal_context, context);
        }
        DataType::Time(precision, TimezoneInfo::None) => {
            let target = TemporalCast::Time(precision.unwrap_or(0));
            return translate_temporal_cast(expr, target, literal_context, context);
        }
        DataType::Char(Some(length)) | DataType::Character(Some(length)) => {
            let length = character_length(length)?;
            return translate_text_cast(expr, "__mysql_cast_as_char", length, context);
        }
        DataType::Custom(name, modifiers) if is_nchar(name) => match modifiers.as_slice() {
            [] => (type_name("NCHAR"), Context::Value),
            [length] => {
                let length = nchar_length(length)?;
                return translate_text_cast(expr, "__mysql_cast_as_nchar", length, context);
            }
            _ => return Err(unsupported_cast(data_type)),
        },
        other => return Err(unsupported_cast(other)),
    };
    Ok(ast::Expr::cast(
        translate_in(expr, operand_context, context)?,
        Some(target),
    ))
}

fn translate_json_numeric_cast(
    expr: &Expr,
    data_type: &DataType,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let coerced = |coercion, context: &mut TranslationContext| {
        json::translate_numeric_coercion(expr, coercion, context)
    };
    Ok(match data_type {
        DataType::Signed | DataType::SignedInteger => function_call(
            "__mysql_cast_as_signed",
            vec![coerced("__mysql_json_integer", context)?],
        ),
        DataType::Unsigned | DataType::UnsignedInteger => function_call(
            "__mysql_cast_as_unsigned",
            vec![coerced("__mysql_json_integer", context)?],
        ),
        DataType::Decimal(precision) => ast::Expr::cast(
            coerced("__mysql_json_decimal", context)?,
            Some(decimal_type(precision)),
        ),
        DataType::Double(ExactNumberInfo::None) => ast::Expr::cast(
            coerced("__mysql_json_double", context)?,
            Some(type_name("DOUBLE")),
        ),
        DataType::Real => ast::Expr::cast(
            coerced("__mysql_json_double", context)?,
            Some(type_name("REAL")),
        ),
        DataType::Float(ExactNumberInfo::None) => ast::Expr::cast(
            coerced("__mysql_json_double", context)?,
            Some(type_name("FLOAT")),
        ),
        other => unreachable!("`{other}` is not a numeric cast target"),
    })
}

fn translate_integer_cast(
    expr: &Expr,
    name: &str,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let operand = match number_literal(expr) {
        Some(digits) if exceeds_unsigned_bigint(digits) => exact_decimal(digits, false),
        Some(digits)
            if digits
                .parse::<u64>()
                .is_ok_and(|number| number > i64::MAX as u64) =>
        {
            string_expr(digits)
        }
        _ => translate_in(expr, Context::Number, context)?,
    };
    Ok(function_call(name, vec![operand]))
}

fn translate_cast_as_json(
    expr: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if !literal_context.is_value() {
        return Err(json_outside_a_value_position(expr));
    }
    if has_a_temporal_type(expr) {
        return Err(temporal_outside_a_value_position(expr));
    }
    let value = if let Some(digits) = number_literal(expr).filter(|digits| {
        digits
            .parse::<u64>()
            .is_ok_and(|number| number > i64::MAX as u64)
    }) {
        string_expr(digits)
    } else if is_binary_literal(expr) {
        translate_bytes(expr, context)?
    } else {
        translate_expr(expr, context)?
    };
    let name = if is_boolean(expr) {
        "__mysql_cast_boolean_as_json"
    } else {
        "__mysql_cast_as_json"
    };
    Ok(function_call(name, vec![value]))
}

pub(crate) fn failing_json_cast(text: &str) -> ast::Expr {
    function_call("__mysql_cast_as_json", vec![string_expr(text)])
}

pub(crate) fn json_outside_a_value_position(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "JSON value `{expr}` in a comparison, a computation or a function MySQL evaluates lazily is not lowered yet"
    ))
}

fn unsupported_cast(data_type: &DataType) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("CAST to `{data_type}` is not lowered yet"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TemporalCast {
    Date,
    DateTime(u64),
    Time(u64),
}

fn translate_temporal_cast(
    expr: &Expr,
    target: TemporalCast,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if !literal_context.is_value() {
        return Err(temporal_outside_a_value_position(expr));
    }
    if !converts_like_its_text(expr, target) {
        return Err(MysqlParserError::Unsupported(format!(
            "a temporal CAST of `{expr}`, which may have a DATE or TIME type MySQL converts differently from its text, is not lowered yet"
        )));
    }
    let value = translate_expr(expr, context)?;
    Ok(match target {
        TemporalCast::Date => function_call("__mysql_cast_as_date", vec![value]),
        TemporalCast::DateTime(precision) => function_call(
            "__mysql_cast_as_datetime",
            vec![value, number(cast_precision(precision))],
        ),
        TemporalCast::Time(precision) => function_call(
            "__mysql_cast_as_time",
            vec![value, number(cast_precision(precision))],
        ),
    })
}

fn temporal_operand(expr: &Expr, operator: &str) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "temporal literal `{expr}` as an operand of {operator}, which MySQL computes as BIGINT UNSIGNED or DOUBLE, is not lowered yet"
    ))
}

fn temporal_has_fraction(expr: &Expr) -> bool {
    fraction_length(expr) > 0
}

fn fraction_length(expr: &Expr) -> usize {
    match expr {
        Expr::TypedString(typed) => match &typed.value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text
                .trim()
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len().min(6)),
            _ => 0,
        },
        Expr::Nested(inner) => fraction_length(inner),
        _ => 0,
    }
}

fn temporal_outside_a_value_position(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "temporal value `{expr}` in a comparison, a computation or a condition, where MySQL reads it as a temporal value or a number, is not lowered yet"
    ))
}

fn converts_like_its_text(operand: &Expr, target: TemporalCast) -> bool {
    match operand {
        Expr::Value(_) | Expr::Prefixed { .. } | Expr::UnaryOp { .. } => true,
        Expr::BinaryOp { left, right, .. } => {
            !matches!(**left, Expr::Interval(_)) && !matches!(**right, Expr::Interval(_))
        }
        Expr::Nested(inner) => converts_like_its_text(inner, target),
        Expr::TypedString(typed) => temporal_type_fits(&typed.data_type, target),
        Expr::Cast {
            expr,
            data_type: DataType::JSON,
            ..
        }
        | Expr::Convert {
            expr,
            data_type: Some(DataType::JSON),
            ..
        } => converts_like_its_text(expr, target),
        Expr::Cast { data_type, .. }
        | Expr::Convert {
            data_type: Some(data_type),
            ..
        } => temporal_type_fits(data_type, target),
        Expr::Function(function) => matches!(
            function.name.0.as_slice(),
            [ObjectNamePart::Identifier(name)]
                if name.quote_style.is_none() && name.value.to_ascii_uppercase().starts_with("JSON_")
        ),
        _ => false,
    }
}

fn temporal_type_fits(data_type: &DataType, target: TemporalCast) -> bool {
    match data_type {
        DataType::Date => !matches!(target, TemporalCast::Time(_)),
        DataType::Time(..) => matches!(target, TemporalCast::Time(_)),
        _ => true,
    }
}

fn cast_precision(precision: u64) -> u64 {
    assert!(
        precision <= MAX_TEMPORAL_PRECISION,
        "the dialect rejects temporal CAST precisions above {MAX_TEMPORAL_PRECISION}"
    );
    precision
}

pub(crate) fn has_a_temporal_type(expr: &Expr) -> bool {
    match expr {
        Expr::TypedString(_) => true,
        Expr::Nested(inner) => has_a_temporal_type(inner),
        other => is_temporal_cast(other),
    }
}

fn is_temporal_cast(expr: &Expr) -> bool {
    match expr {
        Expr::Cast { data_type, .. }
        | Expr::Convert {
            data_type: Some(data_type),
            ..
        } => matches!(
            data_type,
            DataType::Date | DataType::Datetime(_) | DataType::Time(..)
        ),
        Expr::Nested(inner) => is_temporal_cast(inner),
        _ => false,
    }
}

const MAX_CHAR_CAST_LENGTH: u64 = 16 * 1024 * 1024;

fn character_length(length: &CharacterLength) -> Result<u64, MysqlParserError> {
    match length {
        CharacterLength::IntegerLength { length, unit: None }
            if *length <= MAX_CHAR_CAST_LENGTH =>
        {
            Ok(*length)
        }
        other => Err(MysqlParserError::Unsupported(format!(
            "CAST to CHAR({other}) is not lowered yet"
        ))),
    }
}

fn is_nchar(name: &ObjectName) -> bool {
    matches!(
        name.0.as_slice(),
        [ObjectNamePart::Identifier(identifier)]
            if identifier.quote_style.is_none() && identifier.value.eq_ignore_ascii_case("NCHAR")
    )
}

fn nchar_length(length: &str) -> Result<u64, MysqlParserError> {
    match length.parse::<u64>() {
        Ok(length) if length <= MAX_CHAR_CAST_LENGTH => Ok(length),
        _ => Err(MysqlParserError::Unsupported(format!(
            "CAST to NCHAR({length}) is not lowered yet"
        ))),
    }
}

fn translate_text_cast(
    expr: &Expr,
    name: &str,
    length: u64,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let value = translate_expr(expr, context)?;
    Ok(function_call(name, vec![value, number(length)]))
}

fn decimal_type(precision: &ExactNumberInfo) -> ast::Type {
    let (digits, scale) = match *precision {
        ExactNumberInfo::None => (0, 0),
        ExactNumberInfo::Precision(digits) => (digits, 0),
        ExactNumberInfo::PrecisionAndScale(digits, scale) => (digits, scale.unsigned_abs()),
    };
    let digits = if digits == 0 {
        DEFAULT_DECIMAL_PRECISION
    } else {
        digits
    };
    sized_decimal_type(digits, scale)
}

const DEFAULT_DECIMAL_PRECISION: u64 = 10;

fn sized_decimal_type(precision: u64, scale: u64) -> ast::Type {
    ast::Type {
        name: "DECIMAL".to_string(),
        size: Some(ast::TypeSize::TypeSize(
            Box::new(number(precision)),
            Box::new(number(scale)),
        )),
        array_dimensions: 0,
    }
}

fn number(value: u64) -> ast::Expr {
    ast::Expr::Literal(ast::Literal::Numeric(value.to_string()))
}

fn translate_convert_using(
    expr: &Expr,
    charset: &ObjectName,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let target = match single_name(charset)?.to_ascii_lowercase().as_str() {
        "utf8mb4" | "utf8mb3" | "utf8" => "TEXT",
        "binary" => "BLOB",
        other => {
            return Err(MysqlParserError::Unsupported(format!(
                "CONVERT(... USING {other}) is not lowered yet"
            )))
        }
    };
    Ok(ast::Expr::cast(
        translate_expr(expr, context)?,
        Some(type_name(target)),
    ))
}

fn type_name(name: &str) -> ast::Type {
    ast::Type {
        name: name.to_string(),
        size: None,
        array_dimensions: 0,
    }
}

fn translate_collate(
    expr: &Expr,
    collation: &ObjectName,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let name = single_name(collation)?.to_ascii_lowercase();
    let Some(sequence) = collation_sequence(&name) else {
        return Err(MysqlParserError::Unsupported(format!(
            "collation `{name}` is not lowered yet"
        )));
    };
    if !matches!(name.as_str(), "binary" | "utf8mb4_0900_bin")
        && collation_of(expr).is_some_and(|operand| !operand.starts_with("utf8mb4_"))
    {
        return Err(MysqlParserError::Unsupported(format!(
            "collation `{name}` of `{expr}`, which is not a utf8mb4 string, is not lowered yet"
        )));
    }
    Ok(ast::Expr::collate(
        translate_expr(expr, context)?,
        ast::Name::exact(sequence.to_string()),
    ))
}

pub(crate) fn collation_sequence(name: &str) -> Option<&'static str> {
    Some(match name {
        "utf8mb4_0900_ai_ci" => "und-u-ks-level1",
        "utf8mb4_0900_as_ci" => "und-u-ks-level2",
        "utf8mb4_0900_as_cs" => "und-u-ks-level3",
        "utf8mb4_ja_0900_as_cs" => "ja-u-ks-level3",
        "utf8mb4_ja_0900_as_cs_ks" => "ja-u-ks-level4",
        "binary" | "utf8mb4_0900_bin" => "BINARY",
        "utf8mb4_bin" => "RTRIM",
        _ => return None,
    })
}

fn explicit_collations(expr: &Expr, found: &mut Vec<String>) -> Result<(), MysqlParserError> {
    match expr {
        Expr::Collate { expr, collation } => {
            let name = single_name(collation)?.to_ascii_lowercase();
            if !found.contains(&name) {
                found.push(name);
            }
            explicit_collations(expr, found)
        }
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            explicit_collations(inner, found)
        }
        Expr::BinaryOp { left, right, .. } => {
            explicit_collations(left, found)?;
            explicit_collations(right, found)
        }
        Expr::Function(function) => match &function.args {
            FunctionArguments::List(list) => list.args.iter().try_for_each(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) => explicit_collations(arg, found),
                _ => Ok(()),
            }),
            _ => Ok(()),
        },
        Expr::Value(_)
        | Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::TypedString(_)
        | Expr::Prefixed { .. }
        | Expr::Subquery(_) => Ok(()),
        other if other.to_string().to_ascii_uppercase().contains("COLLATE") => {
            Err(MysqlParserError::Unsupported(format!(
                "a collation inside `{other}` is not lowered yet"
            )))
        }
        _ => Ok(()),
    }
}

fn single_explicit_collation(exprs: &[&Expr]) -> Result<Option<String>, MysqlParserError> {
    let mut found = Vec::new();
    for expr in exprs {
        explicit_collations(expr, &mut found)?;
    }
    match found.as_slice() {
        [] => Ok(None),
        [name] => Ok(Some(name.clone())),
        _ if is_illegal_mix(exprs)? => Err(MysqlParserError::Rejected(format!(
            "illegal mix of collations {}",
            found.join(", ")
        ))),
        _ => Err(MysqlParserError::Unsupported(format!(
            "operands with the collations {}, which MySQL rejects as an illegal mix, are not lowered yet",
            found.join(", ")
        ))),
    }
}

fn is_illegal_mix(exprs: &[&Expr]) -> Result<bool, MysqlParserError> {
    let mut names = Vec::new();
    for expr in exprs {
        let mut own = Vec::new();
        explicit_collations(expr, &mut own)?;
        if own.is_empty() {
            continue;
        }
        let mut operand = *expr;
        while let Expr::Nested(inner) = operand {
            operand = inner;
        }
        let Expr::Collate { expr, collation } = operand else {
            return Ok(false);
        };
        let mut inner = Vec::new();
        explicit_collations(expr, &mut inner)?;
        if !inner.is_empty() {
            return Ok(false);
        }
        let name = single_name(collation)?.to_ascii_lowercase();
        if name == "binary" {
            return Ok(false);
        }
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names.len() > 1)
}

fn ensure_default_collation(exprs: &[&Expr], operation: &str) -> Result<(), MysqlParserError> {
    match single_explicit_collation(exprs)? {
        Some(name) if name != DEFAULT_COLLATION => Err(MysqlParserError::Unsupported(format!(
            "{operation} under collation `{name}` is not lowered yet"
        ))),
        _ => Ok(()),
    }
}

fn single_name(name: &ObjectName) -> Result<&str, MysqlParserError> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(identifier)] => Ok(&identifier.value),
        _ => Err(MysqlParserError::Unsupported(format!(
            "qualified name `{name}` is not lowered yet"
        ))),
    }
}

fn translate_trim(
    trim_where: Option<&TrimWhereField>,
    trim_what: Option<&Expr>,
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let removed = trim_what
        .map(|removed| translate_bytes(removed, context))
        .transpose()?;
    let text = translate_bytes(expr, context)?;
    let ends = match trim_where {
        None | Some(TrimWhereField::Both) => TrimEnds::Both,
        Some(TrimWhereField::Leading) => TrimEnds::Leading,
        Some(TrimWhereField::Trailing) => TrimEnds::Trailing,
    };
    Ok(trim_call(ends, text, removed))
}

fn translate_interval_arithmetic(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    match (left, op, right) {
        (_, BinaryOperator::Plus | BinaryOperator::Minus, Expr::Interval(interval)) => {
            let date = translate_expr(left, context)?;
            let (amount, unit) = translate_interval(interval, context)?;
            let name = if *op == BinaryOperator::Plus {
                "DATE_ADD"
            } else {
                "DATE_SUB"
            };
            Ok(function_call(name, vec![date, amount, unit]))
        }
        (Expr::Interval(interval), BinaryOperator::Plus, _) => {
            let (amount, unit) = translate_interval(interval, context)?;
            let date = translate_expr(right, context)?;
            Ok(function_call("DATE_ADD", vec![date, amount, unit]))
        }
        _ => Err(unsupported_operator(op)),
    }
}

fn unsupported_operator(op: &BinaryOperator) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("binary operator `{op}` is not lowered yet"))
}

fn translate_function(
    function: &Function,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let Function {
        name,
        uses_odbc_syntax: false,
        parameters: FunctionArguments::None,
        args,
        filter: None,
        null_treatment: None,
        over,
        within_group,
    } = function
    else {
        return Err(unsupported_function(function));
    };
    if !within_group.is_empty() {
        return Err(unsupported_function(function));
    }
    let [ObjectNamePart::Identifier(identifier)] = name.0.as_slice() else {
        return Err(unsupported_function(function));
    };
    let upper_name = identifier.value.to_ascii_uppercase();
    let list = match args {
        FunctionArguments::None => {
            return translate_function_without_parentheses(&upper_name, identifier, function)
        }
        FunctionArguments::List(list) => list,
        FunctionArguments::Subquery(_) => return Err(unsupported_function(function)),
    };
    if identifier.quote_style.is_none() && over.is_none() {
        if let Some(expr) =
            translate_collated_function(&upper_name, list, literal_context, context)?
        {
            return Ok(expr);
        }
        if let Some(expr) = translate_special_function(&upper_name, list, context)? {
            return Ok(expr);
        }
    }
    let function_name = translate_identifier(identifier)?;
    let filter_over = ast::FunctionTail {
        filter_clause: None,
        over_clause: None,
    };
    if let [FunctionArg::Unnamed(FunctionArgExpr::Wildcard)] = list.args.as_slice() {
        if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
            return Err(unsupported_function(function));
        }
        let filter_over = ast::FunctionTail {
            over_clause: translate_over(over.as_ref(), context)?,
            ..filter_over
        };
        return Ok(ast::Expr::FunctionCallStar {
            name: function_name,
            filter_over,
        });
    }
    let distinctness = match list.duplicate_treatment {
        None => None,
        Some(DuplicateTreatment::All) => Some(ast::Distinctness::All),
        Some(DuplicateTreatment::Distinct) => Some(ast::Distinctness::Distinct),
    };
    let mut args = function_arguments(&upper_name, list, literal_context, context)?;
    let mut order_by = Vec::new();
    let mut separator = None;
    for clause in &list.clauses {
        match clause {
            FunctionArgumentClause::OrderBy(terms) if upper_name == "GROUP_CONCAT" => {
                order_by = terms
                    .iter()
                    .map(|term| translate_order_by_expr(term, context))
                    .collect::<Result<Vec<_>, _>>()?;
            }
            FunctionArgumentClause::Separator(value) if upper_name == "GROUP_CONCAT" => {
                separator = Some(translate_value(&value.value, Context::Value)?);
            }
            _ => return Err(unsupported_function(function)),
        }
    }
    if upper_name == "GROUP_CONCAT" {
        args = group_concat_arguments(args, separator);
    }
    let over_clause = translate_over(over.as_ref(), context)?;
    let call = ast::Expr::FunctionCall {
        name: function_name,
        distinctness,
        args: args.into_iter().map(Box::new).collect(),
        order_by,
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            over_clause,
            ..filter_over
        },
    };
    let json_timestamp = upper_name == "UNIX_TIMESTAMP"
        && matches!(unnamed_arguments(list)?.as_slice(), [arg] if json::is_json_value(arg));
    Ok(if json_timestamp {
        ast::Expr::cast(call, Some(sized_decimal_type(65, 6)))
    } else {
        call
    })
}

fn translate_function_without_parentheses(
    upper_name: &str,
    identifier: &Ident,
    function: &Function,
) -> Result<ast::Expr, MysqlParserError> {
    if identifier.quote_style.is_some() || function.over.is_some() {
        return Err(unsupported_function(function));
    }
    Ok(current_time_literal(upper_name).unwrap_or_else(|| function_call(upper_name, Vec::new())))
}

fn translate_collated_function(
    upper_name: &str,
    list: &FunctionArgumentList,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    let collated = matches!(upper_name, "FIELD" | "GREATEST" | "LEAST" | "STRCMP");
    let compares = matches!(
        upper_name,
        "FIND_IN_SET"
            | "INSTR"
            | "LOCATE"
            | "NULLIF"
            | "REGEXP_INSTR"
            | "REGEXP_LIKE"
            | "REGEXP_REPLACE"
            | "REGEXP_SUBSTR"
    );
    if !collated && !compares {
        return Ok(None);
    }
    let args = unnamed_arguments(list)?;
    let Some(collation) = single_explicit_collation(&args)? else {
        return Ok(None);
    };
    if !collated {
        ensure_default_collation(&args, upper_name)?;
        return Ok(None);
    }
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(MysqlParserError::Unsupported(format!(
            "{upper_name} with a collation and these clauses is not lowered yet"
        )));
    }
    if let Some(binary) = args
        .iter()
        .find(|arg| is_binary_literal(arg) && !is_utf8_binary_literal(arg))
    {
        let message =
            format!("cannot convert the binary string `{binary}` of {upper_name} to utf8mb4");
        return Err(if collation == "binary" {
            MysqlParserError::Unsupported(format!("{message} under COLLATE binary"))
        } else {
            MysqlParserError::Rejected(message)
        });
    }
    if let Some(temporal) = args
        .iter()
        .find(|arg| temporal_type(arg).is_some() || has_a_temporal_type(arg))
    {
        return Err(MysqlParserError::Unsupported(format!(
            "{upper_name} of `{temporal}` and a string with a collation, which MySQL compares as temporal values, is not lowered yet"
        )));
    }
    let mut translated = vec![string_expr(&collation)];
    translated.extend(function_arguments(
        upper_name,
        list,
        literal_context,
        context,
    )?);
    Ok(Some(function_call(
        &format!("__mysql_collated_{}", upper_name.to_ascii_lowercase()),
        translated,
    )))
}

fn translate_special_function(
    upper_name: &str,
    list: &FunctionArgumentList,
    context: &mut TranslationContext,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Ok(None);
    }
    let Ok(args) = unnamed_arguments(list) else {
        return Ok(None);
    };
    match (upper_name, args.as_slice()) {
        ("DATE_ADD" | "ADDDATE", [date, Expr::Interval(interval)]) => {
            let date = translate_expr(date, context)?;
            let (amount, unit) = translate_interval(interval, context)?;
            Ok(Some(function_call("DATE_ADD", vec![date, amount, unit])))
        }
        ("DATE_SUB" | "SUBDATE", [date, Expr::Interval(interval)]) => {
            let date = translate_expr(date, context)?;
            let (amount, unit) = translate_interval(interval, context)?;
            Ok(Some(function_call("DATE_SUB", vec![date, amount, unit])))
        }
        ("TIMESTAMPADD" | "TIMESTAMPDIFF", [Expr::Identifier(unit), rest @ ..])
            if unit.quote_style.is_none() =>
        {
            let unit = unit_literal(&DateTimeField::Custom(unit.clone()))?;
            let mut translated = vec![unit];
            for arg in rest {
                translated.push(translate_expr(arg, context)?);
            }
            Ok(Some(function_call(upper_name, translated)))
        }
        ("GET_FORMAT", [Expr::Identifier(kind), format])
            if kind.quote_style.is_none()
                && matches!(
                    kind.value.to_ascii_uppercase().as_str(),
                    "DATE" | "TIME" | "DATETIME" | "TIMESTAMP"
                ) =>
        {
            let kind = ast::Expr::Literal(ast::Literal::String(quote_string(
                &kind.value.to_ascii_uppercase(),
            )));
            let format = translate_expr(format, context)?;
            Ok(Some(function_call("GET_FORMAT", vec![kind, format])))
        }
        ("CONV", [value, from, to]) => {
            let Some(literal) = hex_or_bit_literal(value) else {
                return Ok(None);
            };
            let from = translate_in(from, argument_context(upper_name, 1, &args), context)?;
            let to = translate_in(to, argument_context(upper_name, 2, &args), context)?;
            Ok(Some(translate_conv_of_literal(literal, from, to)?))
        }
        ("COLLATION", [value]) => Ok(collation_of(value).map(string_expr)),
        ("VALUES", [Expr::Identifier(column)]) if context.inserted_row_is_readable => {
            Ok(Some(ast::Expr::Qualified(
                ast::Name::exact("excluded".to_string()),
                translate_identifier(column)?,
            )))
        }
        ("LTRIM" | "RTRIM", [text]) => {
            let ends = if upper_name == "LTRIM" {
                TrimEnds::Leading
            } else {
                TrimEnds::Trailing
            };
            Ok(Some(trim_call(ends, translate_bytes(text, context)?, None)))
        }
        ("BIN" | "OCT", [value]) => {
            let Some(literal) = hex_or_bit_literal(value) else {
                return Ok(None);
            };
            let to = if upper_name == "BIN" { 2 } else { 8 };
            Ok(Some(translate_conv_of_literal(
                literal,
                number(10),
                number(to),
            )?))
        }
        (_, []) => Ok(current_time_literal(upper_name)),
        _ => Ok(None),
    }
}

fn collation_of(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::Nested(inner) => collation_of(inner),
        Expr::Value(value) => match value.value {
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_) => Some(DEFAULT_COLLATION),
            Value::NationalStringLiteral(_) => Some("utf8mb3_general_ci"),
            Value::Number(..)
            | Value::Null
            | Value::Boolean(_)
            | Value::HexStringLiteral(_)
            | Value::SingleQuotedByteStringLiteral(_) => Some("binary"),
            _ => None,
        },
        Expr::Prefixed { prefix, value } if string_literal(value).is_some() => {
            match prefix.value.to_ascii_lowercase().as_str() {
                "_binary" => Some("binary"),
                "_utf8mb4" => Some(DEFAULT_COLLATION),
                "_utf8mb3" | "_utf8" => Some("utf8mb3_general_ci"),
                "_latin1" => Some("latin1_swedish_ci"),
                "_ascii" => Some("ascii_general_ci"),
                _ => None,
            }
        }
        Expr::Collate { expr, collation } if collation_of(expr) == Some(DEFAULT_COLLATION) => {
            let name = single_name(collation).ok()?.to_ascii_lowercase();
            UTF8MB4_COLLATIONS
                .iter()
                .find(|known| **known == name)
                .copied()
        }
        Expr::UnaryOp { expr, .. } => collation_of(expr).map(|_| "binary"),
        Expr::Function(function) => {
            let (name, args) = function_shape(function)?;
            match (name.as_str(), args.as_slice()) {
                ("RANDOM_BYTES", [argument]) => collation_of(argument).map(|_| "binary"),
                ("UUID", []) => Some("utf8mb3_general_ci"),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(crate) const DEFAULT_COLLATION: &str = "utf8mb4_0900_ai_ci";

const UTF8MB4_COLLATIONS: &[&str] = &[
    "utf8mb4_0900_ai_ci",
    "utf8mb4_0900_as_ci",
    "utf8mb4_0900_as_cs",
    "utf8mb4_0900_bin",
    "utf8mb4_bin",
    "utf8mb4_general_ci",
    "utf8mb4_unicode_ci",
    "utf8mb4_ja_0900_as_cs",
    "utf8mb4_ja_0900_as_cs_ks",
];

fn function_shape(function: &Function) -> Option<(String, Vec<&Expr>)> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    let FunctionArguments::List(list) = &function.args else {
        return None;
    };
    if name.quote_style.is_some()
        || function.over.is_some()
        || function.filter.is_some()
        || list.duplicate_treatment.is_some()
        || !list.clauses.is_empty()
    {
        return None;
    }
    let args = unnamed_arguments(list).ok()?;
    Some((name.value.to_ascii_uppercase(), args))
}

#[derive(Clone, Copy)]
enum TrimEnds {
    Both,
    Leading,
    Trailing,
}

fn trim_call(ends: TrimEnds, text: ast::Expr, removed: Option<ast::Expr>) -> ast::Expr {
    let name = match ends {
        TrimEnds::Both => "__mysql_trim",
        TrimEnds::Leading => "__mysql_ltrim",
        TrimEnds::Trailing => "__mysql_rtrim",
    };
    let removed = removed.unwrap_or_else(|| string_expr(" "));
    function_call(name, vec![text, removed])
}

fn translate_conv_of_literal(
    literal: &Value,
    from: ast::Expr,
    to: ast::Expr,
) -> Result<ast::Expr, MysqlParserError> {
    Ok(function_call(
        "__mysql_conv_binary_literal",
        vec![translate_as_bytes(literal)?, from, to],
    ))
}

fn translate_interval(
    interval: &Interval,
    context: &mut TranslationContext,
) -> Result<(ast::Expr, ast::Expr), MysqlParserError> {
    let Interval {
        value,
        leading_field: Some(field),
        leading_precision: None,
        last_field: None,
        fractional_seconds_precision: None,
    } = interval
    else {
        return Err(MysqlParserError::Unsupported(format!(
            "interval `{interval}` is not lowered yet"
        )));
    };
    let amount = translate_expr(value, context)?;
    Ok((amount, unit_literal(field)?))
}

fn unit_literal(field: &DateTimeField) -> Result<ast::Expr, MysqlParserError> {
    let unit = match field {
        DateTimeField::Custom(identifier) => identifier.value.to_ascii_uppercase(),
        DateTimeField::Week(Some(_)) => String::new(),
        other => other.to_string().to_ascii_uppercase(),
    };
    if !TEMPORAL_UNITS.contains(&unit.as_str()) {
        return Err(MysqlParserError::Unsupported(format!(
            "temporal unit `{field}` is not lowered yet"
        )));
    }
    Ok(ast::Expr::Literal(ast::Literal::String(quote_string(
        &unit,
    ))))
}

fn current_time_literal(upper_name: &str) -> Option<ast::Expr> {
    let literal = match upper_name {
        "CURRENT_DATE" | "UTC_DATE" => ast::Literal::CurrentDate,
        "CURRENT_TIME" | "UTC_TIME" => ast::Literal::CurrentTime,
        "CURRENT_TIMESTAMP" | "UTC_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" => {
            ast::Literal::CurrentTimestamp
        }
        _ => return None,
    };
    Some(ast::Expr::Literal(literal))
}

fn function_arguments(
    upper_name: &str,
    list: &FunctionArgumentList,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<Vec<ast::Expr>, MysqlParserError> {
    let args = unnamed_arguments(list)?;
    if upper_name == "STRCMP"
        && args.iter().any(|arg| is_binary_literal(arg))
        && args.iter().any(|arg| has_a_date_type(arg))
    {
        return Err(MysqlParserError::Unsupported(
            "STRCMP of a binary string and a DATE or DATETIME, which MySQL reads as a DATE or DATETIME, is not lowered yet"
                .to_string(),
        ));
    }
    if let ("ROUND" | "TRUNCATE", [value, places]) = (upper_name, args.as_slice()) {
        let result_ignores_value = matches!(places, Expr::Value(literal) if literal.value == Value::Null)
            || (is_negated(places)
                && signed_number_literal(places)
                    .and_then(|digits| digits.parse::<f64>().ok())
                    .is_some_and(|digits| digits >= 20.0));
        if is_unsigned_bigint_literal(value) && !result_ignores_value {
            return Err(MysqlParserError::Unsupported(format!(
                "{upper_name} of `{value}` by `{places}`, which MySQL computes as BIGINT UNSIGNED, is not lowered yet"
            )));
        }
    }
    let positions = result_positions(upper_name, args.len());
    let result_args: Vec<&Expr> = positions.clone().map(|position| args[position]).collect();
    let results = Results::new(&result_args, literal_context);
    args.iter()
        .enumerate()
        .map(|(position, arg)| {
            let is_result = positions.contains(&position);
            if is_result && (results.doubles || negated_binary(arg).is_some()) {
                return translate_result(arg, results, context);
            }
            if reads_only_text(upper_name, position) {
                if let Some(text) = json::temporal_json_text(arg) {
                    return Ok(text);
                }
            }
            if let Some(read) = json::temporal_json_argument(upper_name, position, &args) {
                return Ok(read);
            }
            if is_json_cast(arg)
                && !upper_name.starts_with("JSON_")
                && (!json::is_json_value_that_succeeds(arg) || compares_by_collation(upper_name))
                && (evaluates_arguments_lazily(upper_name)
                    || !other_arguments_are_present_values(&args, position))
            {
                return Err(json_outside_a_value_position(arg));
            }
            if upper_name == "IF" && position == 0 {
                return translate_condition(arg, context);
            }
            if is_json_value_position(upper_name, position) {
                return translate_json_value(arg, context);
            }
            if is_result {
                return translate_in(arg, results.literal_context, context);
            }
            if let Some(typed) = temporal_matched_with_binary(upper_name, position, &args) {
                return translate_typed_string_as_bytes(typed);
            }
            if let Some(bytes) = latin1_matched_with_binary(upper_name, position, &args)? {
                return Ok(blob_literal(&bytes));
            }
            if context.latin1_as_text
                && binary_converts_to_latin1_subject(upper_name, position, &args)
            {
                let bytes = binary_literal_bytes(arg)
                    .expect("a binary literal converted to a latin1 subject has bytes");
                return Ok(ast::Expr::Literal(ast::Literal::String(quote_string(
                    &latin1_text(&bytes),
                ))));
            }
            if upper_name == "CHAR" && is_negated_double_beyond_bigint(arg) {
                return Err(MysqlParserError::Unsupported(format!(
                    "`{arg}` as a CHAR code, a negated DOUBLE MySQL rejects outside BIGINT, is not lowered yet"
                )));
            }
            if let Some(literal) = negated_binary(arg) {
                match upper_name {
                    "PERIOD_ADD" | "PERIOD_DIFF" => return negated_binary_literal(literal),
                    "MOD" if !other_arguments_hide_decimals(&args, position) => {
                        return Err(decimals_of_negated_binary_literal_show(arg));
                    }
                    _ => {}
                }
            }
            translate_in(arg, argument_context(upper_name, position, &args), context)
        })
        .collect()
}

fn temporal_matched_with_binary<'a>(
    upper_name: &str,
    position: usize,
    args: &[&'a Expr],
) -> Option<&'a TypedString> {
    if !matches!(
        upper_name,
        "REGEXP_LIKE" | "REGEXP_INSTR" | "REGEXP_SUBSTR" | "REGEXP_REPLACE"
    ) || position > 1
    {
        return None;
    }
    let other = args.get(1 - position)?;
    if !is_binary_literal(other) {
        return None;
    }
    typed_string(args[position])
}

fn latin1_matched_with_binary(
    upper_name: &str,
    position: usize,
    args: &[&Expr],
) -> Result<Option<Vec<u8>>, MysqlParserError> {
    fn is_string_position(upper_name: &str, position: usize, args: &[&Expr]) -> bool {
        match upper_name {
            "CONCAT" | "CONCAT_WS" => true,
            "FIELD" => !reads_number(upper_name, position, args),
            "ELT" => position > 0,
            "INSERT" => matches!(position, 0 | 3),
            "LPAD" | "RPAD" => matches!(position, 0 | 2),
            "REPLACE" => position < 3,
            "FIND_IN_SET" | "INSTR" | "LOCATE" | "SUBSTRING_INDEX" => position < 2,
            _ => false,
        }
    }
    let Some(bytes) = latin1_literal_bytes(args[position])? else {
        return Ok(None);
    };
    let converts_to_subject = matches!(upper_name, "INSERT" | "LPAD" | "REPLACE" | "RPAD");
    let matched = is_string_position(upper_name, position, args)
        && if converts_to_subject {
            position != 0 && is_binary_literal(args[0])
        } else {
            args.iter().enumerate().any(|(other, arg)| {
                other != position
                    && is_string_position(upper_name, other, args)
                    && is_binary_literal(arg)
            })
        };
    if matched && args.iter().any(|arg| json::is_json_value(arg)) {
        return Err(MysqlParserError::Unsupported(format!(
            "`{}` next to a binary string and a JSON value in {upper_name}, whose result MySQL converts to utf8mb4, is not lowered yet",
            args[position]
        )));
    }
    Ok(matched.then_some(bytes))
}

fn binary_converts_to_latin1_subject(upper_name: &str, position: usize, args: &[&Expr]) -> bool {
    matches!(
        (upper_name, position),
        ("INSERT", 3) | ("LPAD" | "RPAD", 2) | ("REPLACE", 1 | 2)
    ) && matches!(latin1_literal_bytes(args[0]), Ok(Some(_)))
        && binary_literal_bytes(args[position]).is_some_and(|bytes| !bytes.contains(&0))
}

fn binary_literal_bytes(expr: &Expr) -> Option<Vec<u8>> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::HexStringLiteral(digits) => hex_bytes(digits).ok(),
            Value::SingleQuotedByteStringLiteral(bits) => bit_bytes(bits).ok(),
            _ => None,
        },
        Expr::Prefixed { prefix, value }
            if prefix.quote_style.is_none() && prefix.value.eq_ignore_ascii_case("_binary") =>
        {
            match value.as_ref() {
                Expr::Value(value) => match &value.value {
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        Some(text.as_bytes().to_vec())
                    }
                    Value::HexStringLiteral(digits) => hex_bytes(digits).ok(),
                    _ => None,
                },
                _ => None,
            }
        }
        Expr::Nested(inner) => binary_literal_bytes(inner),
        _ => None,
    }
}

fn latin1_literal_bytes(expr: &Expr) -> Result<Option<Vec<u8>>, MysqlParserError> {
    match expr {
        Expr::Prefixed { prefix, value }
            if prefix.quote_style.is_none() && prefix.value.eq_ignore_ascii_case("_latin1") =>
        {
            match value.as_ref() {
                Expr::Value(value) => match &value.value {
                    Value::HexStringLiteral(digits) => Ok(Some(hex_bytes(digits)?)),
                    Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                        Ok(Some(text.as_bytes().to_vec()))
                    }
                    _ => Ok(None),
                },
                _ => Ok(None),
            }
        }
        Expr::Nested(inner) => latin1_literal_bytes(inner),
        _ => Ok(None),
    }
}

pub(crate) fn unnested(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unnested(inner),
        other => other,
    }
}

fn typed_string(expr: &Expr) -> Option<&TypedString> {
    match expr {
        Expr::TypedString(typed) => Some(typed),
        Expr::Nested(inner) => typed_string(inner),
        _ => None,
    }
}

fn is_unsigned_bigint_literal(expr: &Expr) -> bool {
    number_literal(expr)
        .and_then(|digits| digits.parse::<u64>().ok())
        .is_some_and(|value| value > i64::MAX as u64)
}

fn has_a_date_type(expr: &Expr) -> bool {
    matches!(
        temporal_type(expr),
        Some(DataType::Date | DataType::Datetime(_) | DataType::Timestamp(..))
    )
}

fn is_negated_double_beyond_bigint(expr: &Expr) -> bool {
    is_negated(expr)
        && is_double_literal(expr)
        && signed_number_literal(expr)
            .and_then(|digits| digits.parse::<f64>().ok())
            .is_some_and(|magnitude| magnitude > 9_223_372_036_854_775_808f64)
}

fn other_arguments_hide_decimals(args: &[&Expr], position: usize) -> bool {
    args.iter().enumerate().all(|(other, arg)| {
        other == position
            || hides_decimals(arg)
            || signed_number_literal(arg).is_some_and(|digits| !is_decimal_literal(digits))
    })
}

/// The arguments of IF, IFNULL and COALESCE that can be the result.
fn result_positions(upper_name: &str, arg_count: usize) -> std::ops::Range<usize> {
    match upper_name {
        "IF" => 1.min(arg_count)..arg_count,
        "IFNULL" | "COALESCE" => 0..arg_count,
        _ => 0..0,
    }
}

fn unnamed_arguments(list: &FunctionArgumentList) -> Result<Vec<&Expr>, MysqlParserError> {
    list.args
        .iter()
        .map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => Ok(expr),
            other => Err(MysqlParserError::Unsupported(format!(
                "function argument `{other}` is not lowered yet"
            ))),
        })
        .collect()
}

fn reads_only_text(upper_name: &str, position: usize) -> bool {
    match upper_name {
        "CONCAT" | "CONCAT_WS" | "STRCMP" | "SHA" | "SHA1" | "MD5" | "CRC32" | "TO_BASE64"
        | "FROM_BASE64" | "HEX" | "UNHEX" => true,
        "SHA2" => position == 0,
        _ => false,
    }
}

fn compares_by_collation(upper_name: &str) -> bool {
    matches!(
        upper_name,
        "FIELD" | "GREATEST" | "LEAST" | "LOCATE" | "INSTR" | "POSITION" | "FIND_IN_SET"
    )
}

fn evaluates_arguments_lazily(upper_name: &str) -> bool {
    matches!(upper_name, "CONCAT_WS" | "ELT" | "MAKE_SET")
}

fn other_arguments_are_present_values(args: &[&Expr], position: usize) -> bool {
    args.iter()
        .enumerate()
        .all(|(other, arg)| other == position || is_present_value(arg))
}

fn is_present_value(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(
            value.value,
            Value::Number(..)
                | Value::SingleQuotedString(_)
                | Value::DoubleQuotedString(_)
                | Value::NationalStringLiteral(_)
                | Value::Boolean(_)
        ),
        Expr::TypedString(_) => true,
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => is_present_value(inner),
        _ => false,
    }
}

fn is_json_value_position(upper_name: &str, position: usize) -> bool {
    match upper_name {
        "JSON_ARRAY" => true,
        "JSON_OBJECT" => position % 2 == 1,
        "JSON_SET" | "JSON_INSERT" | "JSON_REPLACE" | "JSON_ARRAY_APPEND" | "JSON_ARRAY_INSERT" => {
            position > 0 && position % 2 == 0
        }
        _ => false,
    }
}

fn argument_context(upper_name: &str, position: usize, args: &[&Expr]) -> Context {
    match upper_name {
        "ABS" | "ACOS" | "ASIN" | "ATAN" | "ATAN2" | "CEIL" | "CEILING" | "CHAR" | "COS"
        | "COT" | "DEGREES" | "EXP" | "FLOOR" | "LN" | "LOG" | "LOG10" | "LOG2" | "MOD" | "POW"
        | "POWER" | "RADIANS" | "ROUND" | "SIGN" | "SIN" | "SQRT" | "TAN" | "TRUNCATE" => {
            Context::Number
        }
        "IF" if position == 0 => Context::Number,
        "CONCAT_WS" if position == 0 && !json::is_json_value(args[0]) => Context::Text,
        "CONCAT" | "CONCAT_WS" | "STRCMP"
            if args
                .iter()
                .any(|arg| is_json_function(arg) || json::is_json_value(arg)) =>
        {
            Context::Value
        }
        _ if hex_or_bit_literal(args[position]).is_some()
            && reads_number(upper_name, position, args) =>
        {
            Context::Number
        }
        _ if temporal_type(args[position]).is_some()
            && (reads_number(upper_name, position, args)
                || reads_temporal_as_number(upper_name, position)) =>
        {
            Context::Number
        }
        _ if BINARY_STRING_FUNCTIONS.contains(&upper_name) => Context::Bytes,
        _ if DATE_TIME_FUNCTIONS.contains(&upper_name) => Context::DateTimeArgument,
        _ => Context::Value,
    }
}

pub(crate) fn reads_temporal_as_number(upper_name: &str, position: usize) -> bool {
    match upper_name {
        "MAKEDATE" | "MAKETIME" | "PERIOD_ADD" | "PERIOD_DIFF" => true,
        "FROM_UNIXTIME" | "SEC_TO_TIME" | "FROM_DAYS" => position == 0,
        _ => false,
    }
}

fn reads_number(upper_name: &str, position: usize, args: &[&Expr]) -> bool {
    match upper_name {
        "ELT" | "MAKE_SET" | "SPACE" => position == 0,
        "LEFT" | "LPAD" | "REPEAT" | "RIGHT" | "RPAD" | "SHA2" => position == 1,
        "CONV" | "INSERT" | "MID" | "SUBSTR" | "SUBSTRING" => matches!(position, 1 | 2),
        "LOCATE" | "SUBSTRING_INDEX" | "REGEXP_SUBSTR" => matches!(position, 2 | 3),
        "REGEXP_INSTR" => matches!(position, 2..=4),
        "REGEXP_REPLACE" => matches!(position, 3 | 4),
        "EXPORT_SET" => matches!(position, 0 | 4),
        "INTERVAL" => true,
        "FIELD" => args
            .iter()
            .any(|arg| comparison_context(arg) == Context::Number),
        _ => false,
    }
}

fn is_json_function(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => matches!(
            function.name.0.as_slice(),
            [ObjectNamePart::Identifier(identifier)]
                if identifier.value.to_ascii_uppercase().starts_with("JSON_")
        ),
        Expr::Nested(inner) => is_json_function(inner),
        _ => false,
    }
}

pub(crate) fn hex_or_bit_literal(expr: &Expr) -> Option<&Value> {
    match expr {
        Expr::Value(value)
            if matches!(
                value.value,
                Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_)
            ) =>
        {
            Some(&value.value)
        }
        Expr::Nested(inner) => hex_or_bit_literal(inner),
        _ => None,
    }
}

pub(crate) fn translate_json_value(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let value = translate_expr(expr, context)?;
    Ok(if is_boolean(expr) {
        function_call("__mysql_cast_boolean_as_json", vec![value])
    } else {
        value
    })
}

pub(crate) fn is_boolean(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(value.value, Value::Boolean(_)),
        Expr::BinaryOp { op, .. } => matches!(
            op,
            BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::Spaceship
                | BinaryOperator::And
                | BinaryOperator::Or
                | BinaryOperator::Xor
                | BinaryOperator::StringConcat
        ),
        Expr::UnaryOp {
            op: UnaryOperator::Not | UnaryOperator::BangNot,
            ..
        }
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsTrue(_)
        | Expr::IsNotTrue(_)
        | Expr::IsFalse(_)
        | Expr::IsNotFalse(_)
        | Expr::IsUnknown(_)
        | Expr::IsNotUnknown(_)
        | Expr::InList { .. }
        | Expr::InSubquery { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::RLike { .. }
        | Expr::Exists { .. }
        | Expr::AnyOp { .. }
        | Expr::AllOp { .. }
        | Expr::MemberOf(_) => true,
        Expr::Nested(inner) => is_boolean(inner),
        _ => false,
    }
}

fn group_concat_arguments(args: Vec<ast::Expr>, separator: Option<ast::Expr>) -> Vec<ast::Expr> {
    let value = args
        .into_iter()
        .reduce(|joined, next| ast::Expr::binary(joined, ast::Operator::Concat, next))
        .expect("the parser requires at least one GROUP_CONCAT argument");
    match separator {
        Some(separator) => vec![value, separator],
        None => vec![value],
    }
}

fn translate_over(
    over: Option<&WindowType>,
    context: &mut TranslationContext,
) -> Result<Option<Box<ast::Over>>, MysqlParserError> {
    let Some(over) = over else {
        return Ok(None);
    };
    let over = match over {
        WindowType::NamedWindow(name) => ast::Over::Name(translate_identifier(name)?),
        WindowType::WindowSpec(spec) => ast::Over::Window(translate_window(spec, context)?),
    };
    Ok(Some(Box::new(over)))
}

pub(crate) fn translate_window(
    spec: &WindowSpec,
    context: &mut TranslationContext,
) -> Result<ast::Window, MysqlParserError> {
    let base = spec
        .window_name
        .as_ref()
        .map(translate_identifier)
        .transpose()?;
    let partition_by = spec
        .partition_by
        .iter()
        .map(|expr| translate_expr(expr, context).map(Box::new))
        .collect::<Result<Vec<_>, _>>()?;
    let order_by = spec
        .order_by
        .iter()
        .map(|term| translate_order_by_expr(term, context))
        .collect::<Result<Vec<_>, _>>()?;
    let frame_clause = spec
        .window_frame
        .as_ref()
        .map(|frame| translate_frame(frame, context))
        .transpose()?;
    Ok(ast::Window {
        base,
        partition_by,
        order_by,
        frame_clause,
    })
}

fn translate_frame(
    frame: &WindowFrame,
    context: &mut TranslationContext,
) -> Result<ast::FrameClause, MysqlParserError> {
    let mode = match frame.units {
        WindowFrameUnits::Rows => ast::FrameMode::Rows,
        WindowFrameUnits::Range => ast::FrameMode::Range,
        WindowFrameUnits::Groups => {
            return Err(MysqlParserError::Unsupported(
                "GROUPS window frames are not MySQL syntax".to_string(),
            ))
        }
    };
    let start = translate_frame_bound(&frame.start_bound, context)?;
    let end = frame
        .end_bound
        .as_ref()
        .map(|bound| translate_frame_bound(bound, context))
        .transpose()?;
    Ok(ast::FrameClause {
        mode,
        start,
        end,
        exclude: None,
    })
}

fn translate_frame_bound(
    bound: &WindowFrameBound,
    context: &mut TranslationContext,
) -> Result<ast::FrameBound, MysqlParserError> {
    Ok(match bound {
        WindowFrameBound::CurrentRow => ast::FrameBound::CurrentRow,
        WindowFrameBound::Preceding(None) => ast::FrameBound::UnboundedPreceding,
        WindowFrameBound::Following(None) => ast::FrameBound::UnboundedFollowing,
        WindowFrameBound::Preceding(Some(offset)) => {
            ast::FrameBound::Preceding(Box::new(translate_expr(offset, context)?))
        }
        WindowFrameBound::Following(Some(offset)) => {
            ast::FrameBound::Following(Box::new(translate_expr(offset, context)?))
        }
    })
}

pub(crate) fn function_call(name: &str, args: Vec<ast::Expr>) -> ast::Expr {
    ast::Expr::FunctionCall {
        name: ast::Name::exact(name.to_string()),
        distinctness: None,
        args: args.into_iter().map(Box::new).collect(),
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    }
}

fn unsupported_function(function: &Function) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("function call `{function}` is not lowered yet"))
}

fn unsupported(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("expression `{expr}` is not lowered yet"))
}

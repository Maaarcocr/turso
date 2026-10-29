use crate::bitwise::{is_bit_operation, translate_bit_operation};
use crate::dialect::{MAX_TEMPORAL_PRECISION, TEMPORAL_UNITS};
use crate::function_names::{BINARY_STRING_FUNCTIONS, DATE_TIME_FUNCTIONS};
use crate::json;
use crate::literal::{
    binary_literal_as_double, binary_literal_value, bit_bytes, blob_literal, hex_bytes,
    is_latin1_char, is_utf8_binary_literal, latin1_text, negated_binary_literal, temporal_literal,
    translate_as_bytes, translate_introduced_string, translate_temporal_text,
    translate_typed_string, translate_typed_string_as_bytes, translate_value, Context,
    TemporalKind,
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
    let above_bigint = hex_or_bit_literal(expr)
        .and_then(|literal| binary_literal_value(literal).ok())
        .is_some_and(|value| i64::try_from(value).is_err());
    if above_bigint {
        return Ok(ast::Expr::Literal(ast::Literal::Numeric("1".to_string())));
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
        Expr::Identifier(identifier) => {
            match context.having_column(&identifier.value).map(str::to_string) {
                Some(qualifier) => {
                    let qualified = Expr::CompoundIdentifier(vec![
                        sqlparser::ast::Ident::with_quote('`', qualifier),
                        identifier.clone(),
                    ]);
                    translate_node(&qualified, literal_context, context)
                }
                None => translate_identifier_expr(identifier),
            }
        }
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [scope, name] if is_system_variable_scope(scope) => {
                crate::session::system_variable(&scope.value, name)
            }
            _ => translate_compound_identifier(parts),
        },
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

/// `-9223372036854775808`, which MySQL reads as the smallest BIGINT, as an
/// integer: core reads its digits as a DOUBLE before negating them.
fn smallest_bigint() -> ast::Expr {
    ast::Expr::Parenthesized(vec![Box::new(ast::Expr::binary(
        ast::Expr::Literal(ast::Literal::Numeric(
            i64::MIN.saturating_add(1).to_string(),
        )),
        ast::Operator::Subtract,
        ast::Expr::Literal(ast::Literal::Numeric("1".to_string())),
    ))])
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
        if let Some(name) = identifier.value.strip_prefix("@@") {
            return crate::session::system_variable("@@", &Ident::new(name.to_string()));
        }
        if let Some(name) = crate::session::user_variable_name(&identifier.value) {
            return Ok(function_call(
                "__mysql_user_variable",
                vec![string_expr(&name)],
            ));
        }
    }
    Ok(ast::Expr::Id(column_name(identifier)?))
}

fn is_system_variable_scope(scope: &Ident) -> bool {
    scope.quote_style.is_none()
        && ["@@session", "@@local", "@@global"]
            .iter()
            .any(|prefix| scope.value.eq_ignore_ascii_case(prefix))
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
            if let Some(digits) = negated_number_literal(expr)
                .filter(|digits| digits.parse::<u64>() == Ok(i64::MIN.unsigned_abs()))
            {
                return Ok(exact_decimal(digits, false));
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
                if digits.parse::<u64>() == Ok(i64::MIN.unsigned_abs()) {
                    return Ok(smallest_bigint());
                }
            }
            if json::is_json_value(expr) {
                return json::translate_negation(expr, context);
            }
            if let Some(value) =
                arithmetic_text_double(expr).filter(|_| !context.compared_beside_non_literal)
            {
                return Ok(double_literal(-value));
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
        BinaryOperator::Plus
        | BinaryOperator::Minus
        | BinaryOperator::Multiply
        | BinaryOperator::Divide
        | BinaryOperator::Modulo
            if json::is_json_value(left) || json::is_json_value(right) =>
        {
            let mut operand = |expr: &Expr| match hex_or_bit_literal(expr) {
                Some(literal) => binary_literal_as_double(literal),
                None => json::translate_arithmetic_operand(expr, context),
            };
            let left = operand(left)?;
            let right = operand(right)?;
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
            let conjunction = matches!(op, BinaryOperator::And);
            let same_operator = |op: &BinaryOperator| match op {
                BinaryOperator::And => conjunction,
                BinaryOperator::Or | BinaryOperator::StringConcat => !conjunction,
                _ => false,
            };
            let mut operands = vec![right];
            let mut first = left;
            while let Expr::BinaryOp { left, op, right } = first {
                if !same_operator(op) {
                    break;
                }
                operands.push(right);
                first = left;
            }
            operands.push(first);
            let operands = operands
                .into_iter()
                .rev()
                .map(|operand| translate_condition(operand, context))
                .collect::<Result<Vec<_>, _>>()?;
            let operator = if conjunction {
                ast::Operator::And
            } else {
                ast::Operator::Or
            };
            Ok(balanced(operands, operator))
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

/// A chain of `AND` or `OR` operands as a balanced tree, which evaluates
/// them in the same order and nests only logarithmically deep, so a long
/// generated chain stays under the expression depth limit.
fn balanced(mut operands: Vec<ast::Expr>, operator: ast::Operator) -> ast::Expr {
    if operands.len() == 1 {
        return operands.pop().expect("one operand");
    }
    let right = operands.split_off(operands.len() / 2);
    ast::Expr::binary(
        balanced(operands, operator),
        operator,
        balanced(right, operator),
    )
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
    let operand = |expr: &Expr, other: &Expr, context: &mut TranslationContext| {
        if json::is_json_value(expr) {
            return json::translate_numeric_coercion(expr, "__mysql_json_decimal", context);
        }
        if let Some(value) = hex_or_bit_literal(expr)
            .and_then(|literal| binary_literal_value(literal).ok())
            .and_then(|value| i64::try_from(value).ok())
            .filter(|_| cannot_be_negative(other))
        {
            return Ok(ast::Expr::Literal(ast::Literal::Numeric(value.to_string())));
        }
        translate_in(expr, Context::Arithmetic, context)
    };
    let quotient = ast::Expr::binary(
        operand(left, right, context)?,
        ast::Operator::Divide,
        operand(right, left, context)?,
    );
    Ok(ast::Expr::cast(quotient, Some(type_name("INTEGER"))))
}

const BIGINT_LIMIT: f64 = 9_223_372_036_854_775_808.0;

fn cannot_be_negative(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => cannot_be_negative(inner),
        Expr::Value(value) => match &value.value {
            Value::Number(..) | Value::Null => true,
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                text_double(text) >= 0.0
            }
            Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_) => {
                binary_literal_value(&value.value).is_ok_and(|value| i64::try_from(value).is_ok())
            }
            _ => false,
        },
        _ => false,
    }
}

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
        let reads_double = |expr: &Expr| {
            is_double_literal(expr)
                || is_text_literal(expr)
                || matches!(expr, Expr::Prefixed { value, .. } if is_text_literal(value))
        };
        let double = reads_double(left) || reads_double(right);
        let mut operand = |expr: &Expr| match (hex_or_bit_literal(expr), negated_binary(expr)) {
            (Some(literal), _) if double => binary_literal_as_double(literal),
            (_, Some(literal)) if double => negated_binary_literal(literal),
            _ => unsigned_bigint_operand(expr, Context::Arithmetic, context),
        };
        let dividend = operand(left)?;
        let divisor = operand(right)?;
        return Ok(if double {
            function_call("MOD", vec![dividend, divisor])
        } else {
            ast::Expr::binary(dividend, operator, divisor)
        });
    }
    let unsigned = |expr: &Expr| number_literal(expr).is_some_and(exceeds_signed_bigint);
    if unsigned(left) || unsigned(right) {
        if let Some(result) = folded_unsigned_bigint(left, operator, right, parent_context)? {
            return Ok(result);
        }
    }
    let lhs = match exact_unsigned_bigint(left, right, operator) {
        Some(lhs) => lhs,
        None => match unsigned_bigint_beside_null(left, right)? {
            Some(lhs) => lhs,
            None => arithmetic_operand(left, right, context)?,
        },
    };
    let rhs = match exact_unsigned_bigint(right, left, operator) {
        Some(rhs) => rhs,
        None => match unsigned_bigint_beside_null(right, left)? {
            Some(rhs) => rhs,
            None => arithmetic_operand(right, left, context)?,
        },
    };
    Ok(ast::Expr::binary(lhs, operator, rhs))
}

/// `a + b`, `a - b` or `a * b` of BIGINT UNSIGNED literals beside a NULL
/// literal, whose value cannot change the NULL result: MySQL still rejects it
/// outside the BIGINT UNSIGNED range.
fn unsigned_bigint_beside_null(
    expr: &Expr,
    other: &Expr,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return Ok(None);
    };
    let operator = match op {
        BinaryOperator::Plus => ast::Operator::Add,
        BinaryOperator::Minus => ast::Operator::Subtract,
        BinaryOperator::Multiply => ast::Operator::Multiply,
        _ => return Ok(None),
    };
    let unsigned = |expr: &Expr| number_literal(expr).is_some_and(exceeds_signed_bigint);
    if !matches!(other, Expr::Value(value) if value.value == Value::Null)
        || !(unsigned(left) || unsigned(right))
    {
        return Ok(None);
    }
    folded_unsigned_bigint(left, operator, right, Context::Value)
}

/// An integer literal between the BIGINT and BIGINT UNSIGNED limits as the
/// exact DECIMAL of its digits where MySQL's result is a DECIMAL: in a
/// division, and next to a DECIMAL literal or a temporal literal.
fn exact_unsigned_bigint(expr: &Expr, other: &Expr, operator: ast::Operator) -> Option<ast::Expr> {
    let digits = number_literal(expr).filter(|digits| exceeds_signed_bigint(digits))?;
    if is_double_literal(other) || is_text_literal(other) {
        return None;
    }
    let exact = operator == ast::Operator::Divide
        || signed_number_literal(other)
            .is_some_and(|digits| is_decimal_literal(digits) || exceeds_unsigned_bigint(digits))
        || temporal_type(other).is_some();
    exact.then(|| exact_decimal(digits, false))
}

/// `a + b`, `a - b` or `a * b` of two integer literals, one between the
/// BIGINT and BIGINT UNSIGNED limits, which MySQL computes as BIGINT
/// UNSIGNED: the result, which MySQL rejects outside that range.
fn folded_unsigned_bigint(
    left: &Expr,
    operator: ast::Operator,
    right: &Expr,
    parent_context: Context,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    let integer = |expr: &Expr| {
        let digits = signed_number_literal(expr)?;
        if is_decimal_literal(digits) || exceeds_unsigned_bigint(digits) {
            return None;
        }
        let magnitude = digits.parse::<i128>().ok()?;
        Some(if is_negated(expr) {
            -magnitude
        } else {
            magnitude
        })
    };
    let (Some(lhs), Some(rhs)) = (integer(left), integer(right)) else {
        return Ok(None);
    };
    let result = match operator {
        ast::Operator::Add => lhs + rhs,
        ast::Operator::Subtract => lhs - rhs,
        ast::Operator::Multiply => lhs * rhs,
        _ => return Ok(None),
    };
    if parent_context == Context::Arithmetic || !(0..=i128::from(u64::MAX)).contains(&result) {
        return Err(MysqlParserError::Unsupported(format!(
            "`{left} {operator} {right}`, which MySQL computes as BIGINT UNSIGNED and rejects outside its range or changes inside arithmetic, is not lowered yet"
        )));
    }
    Ok(Some(match i64::try_from(result) {
        Ok(result) => ast::Expr::Literal(ast::Literal::Numeric(result.to_string())),
        Err(_) => exact_decimal(&result.to_string(), false),
    }))
}

/// An operand whose integer literal between the BIGINT and BIGINT UNSIGNED
/// limits is the exact DECIMAL of its digits, where MySQL's BIGINT UNSIGNED
/// result cannot leave that range and so shows the same digits: `MOD`, `%`,
/// `ABS`, `CEIL`, `FLOOR`, `ROUND`, `TRUNCATE` and the separator of
/// `CONCAT_WS`.
fn unsigned_bigint_operand(
    expr: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    match number_literal(expr) {
        Some(digits) if exceeds_signed_bigint(digits) => Ok(exact_decimal(digits, false)),
        _ => translate_in(expr, literal_context, context),
    }
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
        let (result, range) = match operator {
            ast::Operator::Add => (lhs.checked_add(rhs), 0..=i128::from(u64::MAX)),
            ast::Operator::Subtract => (lhs.checked_sub(rhs), 0..=i128::from(u64::MAX)),
            ast::Operator::Multiply => (lhs.checked_mul(rhs), 0..=i128::from(u64::MAX)),
            ast::Operator::Modulus if rhs == 0 => {
                return Ok(Some(ast::Expr::Literal(ast::Literal::Null)));
            }
            ast::Operator::Modulus if lhs > i128::from(i64::MAX) && rhs < 0 => {
                return Ok(None);
            }
            ast::Operator::Modulus => {
                (Some(lhs % rhs), i128::from(i64::MIN)..=i128::from(u64::MAX))
            }
            _ => return Ok(None),
        };
        let Some(result) = result.filter(|result| range.contains(result)) else {
            return Err(MysqlParserError::Unsupported(format!(
                "`{left} {operator} {right}`, which MySQL computes as BIGINT UNSIGNED and rejects outside its range, is not lowered yet"
            )));
        };
        return Ok(Some(match i64::try_from(result) {
            Ok(result) => ast::Expr::Literal(ast::Literal::Numeric(result.to_string())),
            Err(_) => exact_decimal(&result.to_string(), false),
        }));
    }
    let lhs = unsigned_operand(left, left_binary, context)?;
    let rhs = unsigned_operand(right, right_binary, context)?;
    Ok(Some(ast::Expr::binary(lhs, operator, rhs)))
}

/// An operand of arithmetic MySQL computes as DECIMAL beside a hex or bit
/// literal, which is its exact value.
fn unsigned_operand(
    expr: &Expr,
    binary: Option<&Value>,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let Some(literal) = binary else {
        return translate_in(expr, Context::Arithmetic, context);
    };
    let value = binary_literal_value(literal)?;
    Ok(match i64::try_from(value) {
        Ok(value) => ast::Expr::Literal(ast::Literal::Numeric(value.to_string())),
        Err(_) => exact_decimal(&value.to_string(), false),
    })
}

#[derive(Clone, Copy)]
enum OperandKind {
    Integer(i128),
    Decimal,
    Null,
}

impl OperandKind {
    /// The kind of a literal operand beside a hex or bit literal: integers
    /// up to the BIGINT UNSIGNED limit, `TRUE` and `FALSE`, and DATE, TIME
    /// and TIMESTAMP literals as their numbers; DECIMAL for exact numbers
    /// with decimals or beyond, and for temporal literals with a fraction;
    /// `NULL` for `NULL` and arithmetic of `NULL` and a number literal.
    fn of(expr: &Expr) -> Result<Option<OperandKind>, MysqlParserError> {
        if let Some(literal) = hex_or_bit_literal(expr) {
            return Ok(Some(OperandKind::Integer(i128::from(
                binary_literal_value(literal)?,
            ))));
        }
        if is_null_valued(expr) {
            return Ok(Some(OperandKind::Null));
        }
        match unnested(expr) {
            Expr::Value(value) => {
                if let Value::Boolean(value) = value.value {
                    return Ok(Some(OperandKind::Integer(i128::from(value))));
                }
            }
            Expr::TypedString(typed) => {
                if temporal_has_fraction(expr) {
                    return Ok(Some(OperandKind::Decimal));
                }
                let Some(temporal) = temporal_literal(typed) else {
                    return Ok(None);
                };
                let digits: String = temporal
                    .text
                    .chars()
                    .filter(|character| character.is_ascii_digit())
                    .collect();
                let Ok(magnitude) = digits.parse::<i128>() else {
                    return Ok(None);
                };
                let negative = temporal.text.starts_with('-');
                return Ok(Some(OperandKind::Integer(if negative {
                    -magnitude
                } else {
                    magnitude
                })));
            }
            _ => {}
        }
        let Some(digits) = signed_number_literal(expr) else {
            return Ok(None);
        };
        if is_decimal_literal(digits) || exceeds_unsigned_bigint(digits) {
            return Ok(Some(OperandKind::Decimal));
        }
        let Ok(magnitude) = digits.parse::<u64>() else {
            return Ok(None);
        };
        let magnitude = i128::from(magnitude);
        if !is_negated(expr) {
            return Ok(Some(OperandKind::Integer(magnitude)));
        }
        Ok(Some(if magnitude > -i128::from(i64::MIN) {
            OperandKind::Decimal
        } else {
            OperandKind::Integer(-magnitude)
        }))
    }
}

/// `NULL`, or `+ - * / % DIV` of a `NULL`-valued operand and a number
/// literal, which cannot fail before the result is `NULL`.
fn is_null_valued(expr: &Expr) -> bool {
    match unnested(expr) {
        Expr::BinaryOp {
            left,
            op:
                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo
                | BinaryOperator::MyIntegerDivide,
            right,
        } => {
            let number = |expr: &Expr| signed_number_literal(expr).is_some();
            (is_null_valued(left) && (number(right) || is_null_valued(right)))
                || (number(left) && is_null_valued(right))
        }
        other => is_null_literal(other),
    }
}

fn is_null_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => matches!(value.value, Value::Null),
        Expr::Nested(inner) => is_null_literal(inner),
        _ => false,
    }
}

/// The DOUBLE MySQL computes `+ - * /` and unary `-` of a plain or utf8
/// string literal with: the number its text starts with, 0 for none.
fn arithmetic_text_double(expr: &Expr) -> Option<f64> {
    let text = match unnested(expr) {
        Expr::Prefixed { prefix, value }
            if matches!(
                prefix.value.to_ascii_lowercase().as_str(),
                "_utf8mb4" | "_utf8mb3" | "_utf8"
            ) =>
        {
            string_literal(value)?
        }
        other => string_literal(other)?,
    };
    Some(text_double(text))
}

fn arithmetic_operand(
    expr: &Expr,
    other_operand: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Some(value) =
        arithmetic_text_double(expr).filter(|_| !context.compared_beside_non_literal)
    {
        return Ok(double_literal(value));
    }
    let introduced_text = matches!(
        other_operand,
        Expr::Prefixed { value, .. } if is_text_literal(value)
    );
    if !is_double_literal(other_operand) && !is_text_literal(other_operand) && !introduced_text {
        return translate_in(expr, Context::Arithmetic, context);
    }
    if let Some(digits) = signed_number_literal(expr).filter(|digits| {
        !is_decimal_literal(digits)
            && !digits.contains(['e', 'E'])
            && digits.trim_start_matches('0').len() > 15
    }) {
        let sign = if is_negated(expr) { "-" } else { "" };
        return Ok(ast::Expr::Literal(ast::Literal::Numeric(format!(
            "{sign}{digits}e0"
        ))));
    }
    if introduced_text {
        return match hex_or_bit_literal(expr) {
            Some(literal) => binary_literal_as_double(literal),
            None => translate_in(expr, Context::Arithmetic, context),
        };
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

/// The text MySQL shows for a selected `a + b`, `a - b`, `a % b` or `MOD(a, b)`
/// of a negated hex or bit literal and another such literal or an exact number
/// literal, and for a selected IF, IFNULL, COALESCE or CASE among such
/// results. MySQL computes them as a DOUBLE with the most decimals of the
/// operands and shows it with exactly that many, rounded from its shortest
/// round-trip digits (`-0.0 + -0xffff` is `-65535.0`, `-0 + -0xffffffffffffffff`
/// is `-18446744073709552000`).
pub(crate) fn shown_negated_binary(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Option<Result<ast::Expr, MysqlParserError>> {
    match expr {
        Expr::Nested(inner) => shown_negated_binary(inner, context),
        Expr::BinaryOp { left, op, right } => shown_negated_binary_operation(left, op, right),
        Expr::Function(function) => match function_shape(function)? {
            (name, args) if name == "MOD" && args.len() == 2 => {
                shown_negated_binary_operation(args[0], &BinaryOperator::Modulo, args[1])
            }
            _ => shown_fixed_double_results(expr, context),
        },
        Expr::Case { .. } => shown_fixed_double_results(expr, context),
        _ => None,
    }
}

fn shown_negated_binary_operation(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
) -> Option<Result<ast::Expr, MysqlParserError>> {
    if negated_binary(left).is_none() && negated_binary(right).is_none() {
        return None;
    }
    let operand = |expr: &Expr| {
        fixed_double_operand(expr).or_else(|| match op {
            BinaryOperator::Modulo => {
                let value = binary_literal_value(hex_or_bit_literal(expr)?).ok()?;
                Some((value as f64, 0))
            }
            _ => None,
        })
    };
    let (lhs, lhs_decimals) = operand(left)?;
    let (rhs, rhs_decimals) = operand(right)?;
    let value = match op {
        BinaryOperator::Plus => lhs + rhs,
        BinaryOperator::Minus => lhs - rhs,
        BinaryOperator::Modulo if rhs == 0.0 => {
            return Some(Ok(ast::Expr::Literal(ast::Literal::Null)));
        }
        BinaryOperator::Modulo => lhs % rhs,
        _ => return None,
    };
    Some(fixed_double_text(value, lhs_decimals.max(rhs_decimals)).map(|text| string_expr(&text)))
}

/// A selected IF, IFNULL, COALESCE or CASE whose results are NULL, exact
/// number literals and a negated hex or bit literal, a DOUBLE without
/// decimals. MySQL computes every result as a DOUBLE with the most decimals
/// of any and shows it with exactly that many (`IF(c, 1.5, -0x1)` shows
/// `-1.0` or `1.5`), so each result becomes that text.
fn shown_fixed_double_results(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Option<Result<ast::Expr, MysqlParserError>> {
    let mut shown = expr.clone();
    let results: Vec<&mut Expr> = match &mut shown {
        Expr::Case {
            conditions,
            else_result,
            ..
        } => conditions
            .iter_mut()
            .map(|case| &mut case.result)
            .chain(else_result.as_deref_mut())
            .collect(),
        Expr::Function(function) => {
            let (name, args) = function_shape(function)?;
            let positions = result_positions(&name, args.len());
            if positions.is_empty() {
                return None;
            }
            let FunctionArguments::List(list) = &mut function.args else {
                unreachable!("function_shape accepts only argument lists");
            };
            list.args
                .iter_mut()
                .enumerate()
                .filter(|(position, _)| positions.contains(position))
                .map(|(_, arg)| match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) => arg,
                    _ => unreachable!("function_shape accepts only unnamed arguments"),
                })
                .collect()
        }
        _ => return None,
    };
    if !results
        .iter()
        .any(|result| negated_binary(result).is_some())
    {
        return None;
    }
    let is_null = |expr: &Expr| matches!(expr, Expr::Value(value) if value.value == Value::Null);
    let operands = results
        .iter()
        .map(|result| match is_null(result) {
            true => Some(None),
            false => fixed_double_operand(result).map(Some),
        })
        .collect::<Option<Vec<_>>>()?;
    let decimals = operands
        .iter()
        .flatten()
        .map(|(_, decimals)| *decimals)
        .max()
        .expect("a negated binary literal is among the results");
    for (result, operand) in results.into_iter().zip(operands) {
        if let Some((value, _)) = operand {
            match fixed_double_text(value, decimals) {
                Ok(text) => {
                    *result = Expr::Value(Value::SingleQuotedString(text).with_empty_span())
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
    Some(translate_bytes(&shown, context))
}

fn fixed_double_operand(expr: &Expr) -> Option<(f64, usize)> {
    if let Some(literal) = negated_binary(expr) {
        let value = binary_literal_value(literal).ok()? as f64;
        return Some((-value, 0));
    }
    let digits = signed_number_literal(expr)?;
    if digits.contains(['e', 'E']) {
        return None;
    }
    let decimals = digits
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let magnitude: f64 = digits.parse().expect("the dialect lexes number literals");
    let value = if is_negated(expr) && magnitude != 0.0 {
        -magnitude
    } else {
        magnitude
    };
    Some((value, decimals))
}

fn fixed_double_text(value: f64, decimals: usize) -> Result<String, MysqlParserError> {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` output has an exponent");
    let exponent: i64 = exponent.parse().expect("`{:e}` exponent is an integer");
    let digits = mantissa.replace('.', "");
    let point = exponent + 1;
    let (integer, fraction) = if point <= 0 {
        (
            "0".to_string(),
            format!("{}{digits}", "0".repeat(point.unsigned_abs() as usize)),
        )
    } else if point as usize >= digits.len() {
        (
            format!("{digits}{}", "0".repeat(point as usize - digits.len())),
            String::new(),
        )
    } else {
        let (integer, fraction) = digits.split_at(point as usize);
        (integer.to_string(), fraction.to_string())
    };
    let fraction = fraction.trim_end_matches('0');
    let (integer, fraction) = if fraction.len() > decimals {
        let (kept, dropped) = fraction.split_at(decimals);
        if dropped == "5" {
            return Err(MysqlParserError::Unsupported(format!(
                "a DOUBLE of {value:e} halfway between two values of {decimals} decimals, which MySQL rounds by its exact binary value, is not lowered yet"
            )));
        }
        let mut digits = format!("{integer}{kept}").into_bytes();
        if dropped.as_bytes()[0] >= b'5' {
            let carried = digits.iter_mut().rev().all(|digit| {
                let nine = *digit == b'9';
                *digit = if nine { b'0' } else { *digit + 1 };
                nine
            });
            if carried {
                digits.insert(0, b'1');
            }
        }
        let digits = String::from_utf8(digits).expect("decimal digits are ASCII");
        let (integer, kept) = digits.split_at(digits.len() - decimals);
        (integer.to_string(), kept.to_string())
    } else {
        (integer, fraction.to_string())
    };
    Ok(if decimals == 0 {
        format!("{sign}{integer}")
    } else {
        format!("{sign}{integer}.{fraction:0<decimals$}")
    })
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
    if let Some((lhs, rhs)) = compared_bytes(left, right) {
        return Ok(ast::Expr::binary(
            blob_literal(&lhs),
            operator,
            blob_literal(&rhs),
        ));
    }
    if let Some((lhs, rhs)) = compared_doubles(left, right) {
        return Ok(ast::Expr::binary(
            double_literal(lhs),
            operator,
            double_literal(rhs),
        ));
    }
    let lhs = translate_compared(left, right, context)?;
    let rhs = translate_compared(right, left, context)?;
    let time_with_text = |time: &Expr, text: &Expr| {
        matches!(temporal_type(time), Some(DataType::Time(..))) && string_literal(text).is_some()
    };
    let lhs = if explicit.is_none() && (time_with_text(left, right) || time_with_text(right, left))
    {
        let sequence = collation_sequence(DEFAULT_COLLATION).expect("the default collation maps");
        ast::Expr::collate(lhs, ast::Name::exact(sequence.to_string()))
    } else {
        default_collated(lhs, explicit, &[left, right])
    };
    Ok(ast::Expr::binary(lhs, operator, rhs))
}

enum ComparedLiteral {
    Number(f64),
    Text(f64),
}

fn compared_doubles(left: &Expr, right: &Expr) -> Option<(f64, f64)> {
    match (compared_literal(left)?, compared_literal(right)?) {
        (ComparedLiteral::Number(lhs), ComparedLiteral::Text(rhs))
        | (ComparedLiteral::Text(lhs), ComparedLiteral::Number(rhs)) => Some((lhs, rhs)),
        (ComparedLiteral::Number(_), ComparedLiteral::Number(_))
        | (ComparedLiteral::Text(_), ComparedLiteral::Text(_)) => None,
    }
}

fn compared_literal(expr: &Expr) -> Option<ComparedLiteral> {
    match expr {
        Expr::Nested(inner) => compared_literal(inner),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: inner,
        } => match compared_literal(inner)? {
            ComparedLiteral::Number(value) => Some(ComparedLiteral::Number(-value)),
            ComparedLiteral::Text(_) => None,
        },
        Expr::Prefixed { prefix, value }
            if prefix.quote_style.is_none()
                && matches!(
                    prefix.value.to_ascii_lowercase().as_str(),
                    "_utf8mb4" | "_utf8mb3" | "_utf8" | "_latin1" | "_ascii" | "_binary"
                ) =>
        {
            string_literal(value).map(|text| ComparedLiteral::Text(text_double(text)))
        }
        Expr::Value(value) if matches!(value.value, Value::Boolean(_)) => Some(
            ComparedLiteral::Number(f64::from(u8::from(value.value == Value::Boolean(true)))),
        ),
        _ => match number_literal(expr) {
            Some(digits) => Some(ComparedLiteral::Number(
                digits
                    .parse()
                    .expect("the dialect lexes number literals MySQL reads"),
            )),
            None => string_literal(expr).map(|text| ComparedLiteral::Text(text_double(text))),
        },
    }
}

fn text_double(text: &str) -> f64 {
    let bytes = text.as_bytes();
    let start = bytes
        .iter()
        .take_while(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C))
        .count();
    let digits_from = |from: usize| {
        bytes[from.min(bytes.len())..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
    };
    let mut end = start;
    if matches!(bytes.get(end), Some(b'+' | b'-')) {
        end += 1;
    }
    let whole = digits_from(end);
    end += whole;
    let mut fraction = 0;
    if bytes.get(end) == Some(&b'.') {
        fraction = digits_from(end + 1);
        if whole + fraction > 0 {
            end += 1 + fraction;
        }
    }
    if whole + fraction == 0 {
        return 0.0;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        let digits = digits_from(exponent);
        if digits > 0 {
            end = exponent + digits;
        }
    }
    let value: f64 = text[start..end]
        .parse()
        .expect("the prefix is a decimal number");
    value.clamp(f64::MIN, f64::MAX)
}

fn double_literal(value: f64) -> ast::Expr {
    let literal = ast::Expr::Literal(ast::Literal::Numeric(format!("{:e}", value.abs())));
    if value.is_sign_negative() {
        ast::Expr::unary(ast::UnaryOperator::Negative, literal)
    } else {
        literal
    }
}

fn compared_bytes(left: &Expr, right: &Expr) -> Option<(Vec<u8>, Vec<u8>)> {
    let bytes = |expr: &Expr| {
        binary_literal_bytes(expr)
            .or_else(|| string_literal(expr).map(|text| text.as_bytes().to_vec()))
    };
    let is_plain_text = |bytes: &[u8]| !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok();
    let (lhs, rhs) = (bytes(left)?, bytes(right)?);
    let non_text_binary = (binary_literal_bytes(left).is_some() && !is_plain_text(&lhs))
        || (binary_literal_bytes(right).is_some() && !is_plain_text(&rhs));
    non_text_binary.then_some((lhs, rhs))
}

/// Compares under the default collation when every operand is a string
/// literal of it, or of another case-insensitive collation whose text is
/// ASCII letters and digits, which all of them compare and order alike.
fn default_collated(lhs: ast::Expr, explicit: Option<String>, operands: &[&Expr]) -> ast::Expr {
    fn alphanumeric_insensitive(operand: &Expr) -> bool {
        let text = match operand {
            Expr::Prefixed { value, .. } => string_literal(value),
            other => string_literal(other),
        };
        matches!(
            collation_of(operand),
            Some("latin1_swedish_ci" | "ascii_general_ci" | "utf8mb3_general_ci")
        ) && text.is_some_and(|text| text.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    }
    let literals_only = operands.iter().all(|operand| {
        collation_of(operand) == Some(DEFAULT_COLLATION) || alphanumeric_insensitive(operand)
    });
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
    if is_binary_cast(expr) {
        return Err(binary_cast_outside_a_value_position(expr));
    }
    let outer = std::mem::replace(
        &mut context.compared_beside_non_literal,
        !is_literal(other_side),
    );
    let translated = translate_compared_value(expr, other_side, context);
    context.compared_beside_non_literal = outer;
    translated
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
            if matches!(data_type, DataType::Time(..)) {
                return translate_in(expr, Context::Text, context);
            }
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

/// How MySQL compares number literals with each other in `IN`, `CASE` and
/// `FIELD`: as DOUBLE when one is a double, else as DECIMAL when one has
/// decimals, is above the BIGINT UNSIGNED range or below the BIGINT range,
/// else as BIGINT.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NumberComparison {
    Integer,
    Decimal,
    Double,
}

impl NumberComparison {
    fn of(exprs: &[&Expr]) -> Option<Self> {
        exprs.iter().try_fold(Self::Integer, |kind, expr| {
            let digits = signed_number_literal(expr)?;
            let own = if digits.contains(['e', 'E']) {
                Self::Double
            } else if is_decimal_literal(digits)
                || exceeds_unsigned_bigint(digits)
                || (is_negated(expr) && exceeds_negated_bigint(digits))
            {
                Self::Decimal
            } else {
                Self::Integer
            };
            Some(kind.max(own))
        })
    }

    /// Whether two number literals are equal compared this way. As BIGINT, a
    /// literal above the BIGINT range reads as its 64 bits, as MySQL reads it
    /// in `FIELD` and `CASE` (`FIELD(18446744073709551615, -1)` is 1).
    fn equal(self, left: &Expr, right: &Expr) -> bool {
        fn digits(expr: &Expr) -> &str {
            signed_number_literal(expr).expect("a number literal")
        }
        match self {
            Self::Double => {
                let value = |expr: &Expr| {
                    let magnitude: f64 = digits(expr).parse().expect("a number literal parses");
                    if is_negated(expr) {
                        -magnitude
                    } else {
                        magnitude
                    }
                };
                value(left) == value(right)
            }
            Self::Decimal => {
                let value = |expr: &Expr| {
                    let kept = kept_digits(digits(expr));
                    let (whole, fraction) = kept.split_once('.').unwrap_or((&kept, ""));
                    let whole = whole.trim_start_matches('0').to_string();
                    let fraction = fraction.trim_end_matches('0').to_string();
                    let zero = whole.is_empty() && fraction.is_empty();
                    (is_negated(expr) && !zero, whole, fraction)
                };
                value(left) == value(right)
            }
            Self::Integer => {
                let value = |expr: &Expr| {
                    let magnitude: u64 = digits(expr).parse().expect("an integer literal parses");
                    if is_negated(expr) {
                        0i64.wrapping_sub_unsigned(magnitude)
                    } else {
                        magnitude as i64
                    }
                };
                value(left) == value(right)
            }
        }
    }

    /// Whether comparing as BIGINT signed and as BIGINT UNSIGNED could
    /// differ: a literal above the BIGINT range among negative ones.
    fn sign_matters(self, exprs: &[&Expr]) -> bool {
        let above = |expr: &&Expr| {
            !is_negated(expr)
                && signed_number_literal(expr)
                    .and_then(|digits| digits.parse::<u64>().ok())
                    .is_some_and(|value| value > i64::MAX as u64)
        };
        self == Self::Integer
            && exprs.iter().any(above)
            && exprs.iter().any(|expr| is_negated(expr))
    }
}

/// The signed digits of an integer or decimal literal whose integer part is
/// beyond the BIGINT range.
fn beyond_bigint_digits(expr: &Expr) -> Option<String> {
    let digits = signed_number_literal(expr)?;
    if digits.contains(['e', 'E']) {
        return None;
    }
    let whole = digits.split_once('.').map_or(digits, |(whole, _)| whole);
    let whole = whole.trim_start_matches('0');
    let limit = if is_negated(expr) {
        "9223372036854775808"
    } else {
        "9223372036854775807"
    };
    let beyond = whole.len() > limit.len() || (whole.len() == limit.len() && whole > limit);
    let sign = if is_negated(expr) { "-" } else { "" };
    beyond.then(|| format!("{sign}{digits}"))
}

/// The text `BIN`, `OCT` and `CONV` read a number literal beyond the BIGINT
/// range as: MySQL converts the number's text, saturating at the BIGINT
/// UNSIGNED limit and wrapping a negative number to its 64 bits
/// (`BIN(-9223372036854775809)` is 63 ones), which Turso's conversion of a
/// string does too.
fn converted_number_text(
    upper_name: &str,
    list: &FunctionArgumentList,
) -> Result<Option<String>, MysqlParserError> {
    if !matches!(upper_name, "BIN" | "OCT" | "CONV") {
        return Ok(None);
    }
    let args = unnamed_arguments(list)?;
    let Some(text) = args.first().and_then(|arg| beyond_bigint_digits(arg)) else {
        return Ok(None);
    };
    Ok(Some(text))
}

/// `HEX` and `BIT_COUNT` of an integer literal above the BIGINT range, which
/// MySQL reads as its BIGINT UNSIGNED value.
fn folded_unsigned_bits(
    upper_name: &str,
    list: &FunctionArgumentList,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    if !matches!(upper_name, "HEX" | "BIT_COUNT") {
        return Ok(None);
    }
    let args = unnamed_arguments(list)?;
    let [arg] = args.as_slice() else {
        return Ok(None);
    };
    let Some(value) = number_literal(arg)
        .filter(|_| !is_negated(arg))
        .and_then(|digits| digits.parse::<u64>().ok())
        .filter(|value| *value > i64::MAX as u64)
    else {
        return Ok(None);
    };
    Ok(Some(match upper_name {
        "HEX" => string_expr(&format!("{value:X}")),
        _ => number(u64::from(value.count_ones())),
    }))
}

/// `FIELD` of number literals only, whose position MySQL finds comparing
/// them as `NumberComparison` says.
fn folded_number_field(list: &FunctionArgumentList) -> Result<Option<ast::Expr>, MysqlParserError> {
    let args = unnamed_arguments(list)?;
    let Some((first, rest)) = args.split_first() else {
        return Ok(None);
    };
    let Some(kind) = NumberComparison::of(&args) else {
        return Ok(None);
    };
    let position = rest
        .iter()
        .position(|arg| kind.equal(first, arg))
        .map_or(0, |index| index + 1);
    Ok(Some(number(position as u64)))
}

/// `IN` of number literals only, as the truth MySQL finds comparing the left
/// operand with each item as `NumberComparison` says for that pair
/// (`9223372036854775808 IN (9223372036854775807, 9e0)` compares the first
/// pair as BIGINT), other than a pair of BIGINTs with the same 64 bits, one
/// negative and the other above the BIGINT range.
fn folded_number_in(expr: &Expr, list: &[Expr], negated: bool) -> Option<ast::Expr> {
    let mut found = false;
    for item in list {
        let pair = [expr, item];
        let kind = NumberComparison::of(&pair)?;
        let equal = kind.equal(expr, item);
        if equal && kind.sign_matters(&pair) {
            return None;
        }
        found |= equal;
    }
    Some(number(u64::from(found != negated)))
}

fn translate_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Some(folded) = folded_number_in(expr, list, negated) {
        return Ok(folded);
    }
    let operands = std::iter::once(expr).chain(list).collect::<Vec<_>>();
    let explicit = single_explicit_collation(&operands)?;
    let mixed = list
        .iter()
        .any(|item| compared_doubles(expr, item).is_some());
    if mixed
        && operands
            .iter()
            .all(|operand| compared_literal(operand).is_some())
    {
        let mut matches = list
            .iter()
            .map(|item| comparison(expr, ast::Operator::Equals, item, context));
        let first = matches.next().expect("an IN list has items")?;
        let any = matches.try_fold(first, |any, item| {
            Ok::<_, MysqlParserError>(ast::Expr::binary(any, ast::Operator::Or, item?))
        })?;
        let any = ast::Expr::Parenthesized(vec![Box::new(any)]);
        return Ok(if negated {
            ast::Expr::unary(ast::UnaryOperator::Not, any)
        } else {
            any
        });
    }
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
    reject_limit_in_quantified_subquery(subquery)?;
    let lhs = translate_expr(expr, context)?;
    Ok(ast::Expr::in_select(
        lhs,
        negated,
        translate_subquery(subquery, context)?,
    ))
}

fn reject_limit_in_quantified_subquery(subquery: &Query) -> Result<(), MysqlParserError> {
    if subquery.limit_clause.is_some() {
        return Err(MysqlParserError::Rejected(
            "This version of MySQL doesn't yet support 'LIMIT & IN/ALL/ANY/SOME subquery'"
                .to_string(),
        ));
    }
    Ok(())
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
    let (lhs, rhs) = if is_binary_literal(expr) || is_binary_literal(pattern) {
        (
            binary_regexp_operand(expr, context)?,
            binary_regexp_operand(pattern, context)?,
        )
    } else {
        (
            translate_regexp_operand(expr, context)?,
            translate_regexp_operand(pattern, context)?,
        )
    };
    Ok(ast::Expr::like(
        lhs,
        negated,
        ast::LikeOperator::Regexp,
        rhs,
        None,
    ))
}

/// An operand of a regular expression over a binary string, as the value
/// MySQL matches: binary strings and temporal literals as bytes, numbers as
/// numbers, which MySQL also matches byte by byte, strings as text, which
/// MySQL rejects next to a binary string, and JSON values as their text.
fn binary_regexp_operand(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Some(typed) = typed_string(expr) {
        return translate_typed_string_as_bytes(typed);
    }
    if is_binary_literal(expr) {
        return translate_bytes(expr, context);
    }
    if signed_number_literal(expr).is_some() || is_boolean(expr) || is_null_literal(expr) {
        return translate_in(expr, Context::Number, context);
    }
    let is_string = match expr {
        Expr::Prefixed { value, .. } => is_text_literal(value),
        other => is_text_literal(other),
    };
    if is_string {
        return translate_in(expr, Context::Text, context);
    }
    if is_json_cast(expr) {
        return translate_in(expr, Context::Value, context);
    }
    Err(MysqlParserError::Unsupported(format!(
        "`{expr}` in a regular expression over a binary string is not lowered yet"
    )))
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
    let numbers = operand.and_then(|operand| {
        let compared: Vec<&Expr> = std::iter::once(operand)
            .chain(conditions.iter().map(|case| &case.condition))
            .collect();
        let kind = NumberComparison::of(&compared)?;
        (!kind.sign_matters(&compared)).then(|| {
            conditions
                .iter()
                .map(|case| number(u64::from(kind.equal(operand, &case.condition))))
                .collect::<Vec<_>>()
        })
    });
    let doubles = match numbers {
        Some(conditions) => Some((None, conditions)),
        None => operand
            .and_then(|operand| case_compared_as_doubles(operand, conditions))
            .map(|(base, conditions)| (Some(base), conditions)),
    };
    let base = match (operand, &doubles, conditions.first()) {
        (Some(_), Some((base, _)), _) => base.clone().map(Box::new),
        (Some(operand), None, Some(first)) => {
            let compared: Vec<&Expr> = std::iter::once(operand)
                .chain(conditions.iter().map(|case| &case.condition))
                .collect();
            let explicit = single_explicit_collation(&compared)?;
            let base = translate_compared(operand, &first.condition, context)?;
            Some(Box::new(default_collated(base, explicit, &compared)))
        }
        (Some(operand), None, None) => Some(Box::new(translate_expr(operand, context)?)),
        (None, _, _) => None,
    };
    let results: Vec<&Expr> = conditions
        .iter()
        .map(|case| &case.result)
        .chain(else_result)
        .collect();
    let results = Results::new(&results, literal_context);
    let when_then_pairs = conditions
        .iter()
        .enumerate()
        .map(|(index, case)| {
            let when = match (operand, &doubles) {
                (Some(_), Some((_, conditions))) => conditions[index].clone(),
                (Some(operand), None) => translate_compared(&case.condition, operand, context)?,
                (None, _) => translate_condition(&case.condition, context)?,
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

fn case_compared_as_doubles(
    operand: &Expr,
    conditions: &[CaseWhen],
) -> Option<(ast::Expr, Vec<ast::Expr>)> {
    let literals = std::iter::once(operand)
        .chain(conditions.iter().map(|case| &case.condition))
        .map(compared_literal)
        .collect::<Option<Vec<_>>>()?;
    let numbers = literals
        .iter()
        .any(|literal| matches!(literal, ComparedLiteral::Number(_)));
    let texts = literals
        .iter()
        .any(|literal| matches!(literal, ComparedLiteral::Text(_)));
    if !numbers || !texts {
        return None;
    }
    let mut doubles = literals.into_iter().map(|literal| match literal {
        ComparedLiteral::Number(value) | ComparedLiteral::Text(value) => double_literal(value),
    });
    let base = doubles.next().expect("the operand is among the literals");
    Some((base, doubles.collect()))
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
            hide_decimals: results.iter().all(|result| hides_decimals(result))
                || results
                    .iter()
                    .any(|result| is_double_literal(result) || is_string_literal(result)),
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

fn negated_number_literal(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Nested(inner) => negated_number_literal(inner),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => number_literal(expr),
        _ => None,
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
        vec![unsigned_bigint_operand(
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
    if let Some(matched) = case_sensitive_ascii_like(expr, pattern, escape, context)? {
        return Ok(if negated {
            ast::Expr::unary(ast::UnaryOperator::Not, matched)
        } else {
            matched
        });
    }
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

pub(crate) fn string_expr(text: &str) -> ast::Expr {
    ast::Expr::Literal(ast::Literal::String(quote_string(text)))
}

/// `LIKE` under an explicit `utf8mb4_0900_as_cs`, `utf8mb4_0900_bin` or
/// `utf8mb4_bin` of literals whose text is printable ASCII, where those
/// collations order characters as their bytes: `__mysql_like_bin`. MySQL
/// converts every operand to the explicit collation's `utf8mb4`, so a hex,
/// bit or `_binary` literal that is not UTF-8 is rejected.
fn case_sensitive_ascii_like(
    expr: &Expr,
    pattern: &Expr,
    escape: Option<&sqlparser::ast::ValueWithSpan>,
    context: &mut TranslationContext,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    fn printable(text: &str) -> bool {
        text.bytes().all(|byte| (0x20..0x7f).contains(&byte))
    }
    fn collated(expr: &Expr) -> &Expr {
        match expr {
            Expr::Collate { expr, .. } | Expr::Nested(expr) => collated(expr),
            other => other,
        }
    }
    fn operand(expr: &Expr) -> Result<Option<&Expr>, MysqlParserError> {
        let expr = collated(expr);
        if is_binary_literal(expr) {
            let Some(bytes) = binary_literal_bytes(expr) else {
                return Ok(None);
            };
            return match String::from_utf8(bytes) {
                Ok(text) => Ok(printable(&text).then_some(expr)),
                Err(_) => Err(MysqlParserError::Rejected(format!(
                    "cannot convert the binary string `{expr}` of LIKE to utf8mb4"
                ))),
            };
        }
        if json::is_json_value(expr) {
            let literal_cast = matches!(
                expr,
                Expr::Cast { expr: inner, data_type: DataType::JSON, .. } if is_literal(inner)
            );
            let text = expr.to_string();
            return Ok((literal_cast && printable(&text) && !text.contains('\\')).then_some(expr));
        }
        let text = match expr {
            Expr::Value(value) => match &value.value {
                Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Some(text),
                Value::Number(..) | Value::Boolean(_) | Value::Null => None,
                _ => return Ok(None),
            },
            Expr::Prefixed { prefix, value } => {
                match (prefix.value.to_ascii_lowercase().as_str(), value.as_ref()) {
                    ("_utf8mb4" | "_utf8mb3" | "_utf8", Expr::Value(value)) => match &value.value {
                        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                            Some(text)
                        }
                        _ => return Ok(None),
                    },
                    _ => return Ok(None),
                }
            }
            Expr::TypedString(_) => None,
            _ if signed_number_literal(expr).is_some() || is_null_valued(expr) => None,
            _ => return Ok(None),
        };
        Ok(text.is_none_or(|text| printable(text)).then_some(expr))
    }
    let Some(name) = single_explicit_collation(&[expr, pattern])? else {
        return Ok(None);
    };
    if !matches!(
        name.to_ascii_lowercase().as_str(),
        "utf8mb4_0900_as_cs" | "utf8mb4_0900_bin" | "utf8mb4_bin"
    ) {
        return Ok(None);
    }
    let escape = match escape.map(|escape| &escape.value) {
        None => '\\',
        Some(Value::SingleQuotedString(text) | Value::DoubleQuotedString(text))
            if text.len() == 1 && printable(text) =>
        {
            text.chars().next().expect("the escape has one character")
        }
        Some(_) => return Ok(None),
    };
    let (Some(subject), Some(matched)) = (operand(expr)?, operand(pattern)?) else {
        return Ok(None);
    };
    Ok(Some(function_call(
        "__mysql_like_bin",
        vec![
            json::like_operand(subject, context)?,
            json::like_operand(matched, context)?,
            string_expr(&escape.to_string()),
        ],
    )))
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
    reject_limit_in_quantified_subquery(subquery)?;
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
    if matches!(
        data_type,
        DataType::Date | DataType::Datetime(_) | DataType::Time(_, TimezoneInfo::None)
    ) && is_json_number_literal(expr)
    {
        return Ok(ast::Expr::Literal(ast::Literal::Null));
    }
    let shows_text = match data_type {
        DataType::Char(_) | DataType::Character(_) => true,
        DataType::Custom(name, _) => is_nchar(name),
        _ => false,
    };
    if shows_text
        && binary_literal_bytes(expr).is_some_and(|bytes| std::str::from_utf8(&bytes).is_err())
    {
        return Ok(ast::Expr::Literal(ast::Literal::Null));
    }
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
        DataType::Char(None) | DataType::Character(None) => {
            let text = mysql_text(translate_in(expr, Context::Value, context)?);
            return Ok(ast::Expr::cast(text, Some(type_name("TEXT"))));
        }
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
        DataType::Binary(length) => {
            return translate_binary_cast(expr, *length, literal_context, context)
        }
        DataType::Custom(name, modifiers) if is_nchar(name) => match modifiers.as_slice() {
            [] => {
                return translate_text_cast(
                    expr,
                    "__mysql_cast_as_nchar",
                    MAX_CHAR_CAST_LENGTH,
                    context,
                )
            }
            [length] => {
                let length = nchar_length(length)?;
                return translate_text_cast(expr, "__mysql_cast_as_nchar", length, context);
            }
            _ => return Err(unsupported_cast(data_type)),
        },
        other => return Err(unsupported_cast(other)),
    };
    Ok(ast::Expr::cast(
        function_call(
            "__mysql_numeric_cast_operand",
            vec![translate_in(expr, operand_context, context)?],
        ),
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
    if is_binary_cast(expr) {
        return Err(MysqlParserError::Unsupported(format!(
            "CAST of the binary string `{expr}` AS JSON, whose opaque JSON value Turso passes on as a JSON string, is not lowered yet"
        )));
    }
    if let Some(digits) = signed_number_literal(expr).filter(|digits| !shows_as_a_double(digits)) {
        return Err(MysqlParserError::Unsupported(format!(
            "CAST of the DECIMAL `{digits}` AS JSON, which MySQL keeps exactly and Turso keeps as a DOUBLE, is not lowered yet"
        )));
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

fn is_json_number_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_json_number_literal(inner),
        Expr::Cast {
            kind: CastKind::Cast,
            expr,
            data_type: DataType::JSON,
            array: false,
            format: None,
        } => signed_number_literal(expr).is_some(),
        _ => false,
    }
}

fn shows_as_a_double(digits: &str) -> bool {
    let Some((whole, fraction)) = digits.split_once('.') else {
        return true;
    };
    if digits.contains(['e', 'E']) {
        return true;
    }
    let significant = format!("{whole}{fraction}");
    let significant = significant.trim_start_matches('0');
    !fraction.is_empty() && !fraction.ends_with('0') && significant.len() <= 15
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
    let value = if is_binary_literal(expr) {
        translate_bytes(expr, context)?
    } else if let Some(text) = datetime_number_text(expr) {
        string_expr(&text)
    } else {
        translate_expr(expr, context)?
    };
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

/// The digits of a DECIMAL literal a DOUBLE cannot hold exactly whose
/// integer part is a 14-digit date and time (`20000101103458.1234`), which
/// MySQL reads as a datetime or time exactly as it reads that text.
fn datetime_number_text(expr: &Expr) -> Option<String> {
    let digits = number_literal(expr).filter(|_| !is_negated(expr))?;
    let (whole, _) = digits.split_once('.')?;
    (is_decimal_literal(digits) && !is_exact_as_a_double(digits) && whole.len() == 14)
        .then(|| digits.to_string())
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
    let value = mysql_text(translate_expr(expr, context)?);
    Ok(function_call(name, vec![value, number(length)]))
}

fn translate_binary_cast(
    expr: &Expr,
    length: Option<u64>,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if !literal_context.is_value() {
        return Err(binary_cast_outside_a_value_position(expr));
    }
    let value = if is_binary_literal(expr) {
        translate_bytes(expr, context)?
    } else if json::is_json_value(expr) {
        let text = translate_cast(expr, &DataType::Char(None), literal_context, context)?;
        ast::Expr::cast(text, Some(type_name("BLOB")))
    } else {
        ast::Expr::cast(
            mysql_text(translate_in(expr, Context::Value, context)?),
            Some(type_name("BLOB")),
        )
    };
    Ok(match length {
        None => value,
        Some(length) if length <= MAX_CHAR_CAST_LENGTH => {
            function_call("__mysql_cast_as_binary", vec![value, number(length)])
        }
        Some(length) => {
            return Err(MysqlParserError::Unsupported(format!(
                "CAST to BINARY({length}) is not lowered yet"
            )))
        }
    })
}

fn is_binary_cast(expr: &Expr) -> bool {
    match expr {
        Expr::Cast {
            data_type: DataType::Binary(_),
            ..
        }
        | Expr::Convert {
            data_type: Some(DataType::Binary(_)),
            ..
        } => true,
        Expr::Nested(inner) => is_binary_cast(inner),
        _ => false,
    }
}

fn binary_cast_outside_a_value_position(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "a binary string cast of `{expr}` in a comparison or a computation, which MySQL may compare as numbers, is not lowered yet"
    ))
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
        mysql_text(translate_expr(expr, context)?),
        Some(type_name(target)),
    ))
}

fn mysql_text(value: ast::Expr) -> ast::Expr {
    function_call("CONCAT", vec![value])
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
            let name = if *op == BinaryOperator::Plus {
                "DATE_ADD"
            } else {
                "DATE_SUB"
            };
            translate_date_arithmetic(name, left, interval, context)
        }
        (Expr::Interval(interval), BinaryOperator::Plus, _) => {
            translate_date_arithmetic("DATE_ADD", right, interval, context)
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
    if let Some(error) = latin1_subject_collation_error(&upper_name, list) {
        return Err(error);
    }
    if upper_name == "FIELD" && over.is_none() {
        if let Some(folded) = folded_number_field(list)? {
            return Ok(folded);
        }
    }
    if let Some(folded) = folded_unsigned_bits(&upper_name, list)? {
        return Ok(folded);
    }
    if matches!(
        upper_name.as_str(),
        "GREATEST" | "LEAST" | "FIELD" | "NULLIF" | "INTERVAL"
    ) {
        if let Some(cast) = list.args.iter().find_map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) if is_binary_cast(expr) => Some(expr),
            _ => None,
        }) {
            return Err(binary_cast_outside_a_value_position(cast));
        }
    }
    if identifier.quote_style.is_none() {
        let variance = match upper_name.as_str() {
            "VARIANCE" | "VAR_POP" => Some("__mysql_var_pop"),
            "VAR_SAMP" => Some("__mysql_var_samp"),
            "STD" | "STDDEV" | "STDDEV_POP" => Some("__mysql_stddev_pop"),
            "STDDEV_SAMP" => Some("__mysql_stddev_samp"),
            _ => None,
        };
        if let Some(variance) = variance {
            return translate_variance(function, &upper_name, variance, list, context);
        }
    }
    if identifier.quote_style.is_none()
        && matches!(upper_name.as_str(), "JSON_ARRAYAGG" | "JSON_OBJECTAGG")
    {
        return translate_json_aggregate(function, &upper_name, context);
    }
    if identifier.quote_style.is_none() && over.is_none() {
        if let Some(expr) = translate_greatest_of_dates(&upper_name, list)? {
            return Ok(expr);
        }
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
    if let Some(rounded) = unsigned_bigint_rounding(&upper_name, list)? {
        return Ok(rounded);
    }
    let digits = literal_result_digits(&upper_name, list)?;
    if let Some(LiteralResult::Text(text)) = &digits {
        return Ok(string_expr(text));
    }
    let mut args = function_arguments(&upper_name, list, literal_context, context)?;
    round_decimal_integer_arguments(&upper_name, &mut args);
    if let Some(text) = converted_number_text(&upper_name, list)? {
        args[0] = string_expr(&text);
    }
    if matches!(
        upper_name.as_str(),
        "DATE"
            | "DATE_FORMAT"
            | "DAY"
            | "DAYNAME"
            | "DAYOFMONTH"
            | "DAYOFWEEK"
            | "DAYOFYEAR"
            | "HOUR"
            | "LAST_DAY"
            | "MICROSECOND"
            | "MINUTE"
            | "MONTH"
            | "MONTHNAME"
            | "QUARTER"
            | "SECOND"
            | "TIME_TO_SEC"
            | "TO_DAYS"
            | "TO_SECONDS"
            | "WEEK"
            | "WEEKDAY"
            | "WEEKOFYEAR"
            | "YEAR"
            | "YEARWEEK"
    ) {
        if let Some(text) = unnamed_arguments(list)?
            .first()
            .and_then(|value| datetime_number_text(value))
        {
            args[0] = string_expr(&text);
        }
    }
    if matches!(upper_name.as_str(), "ROUND" | "TRUNCATE") {
        if let Some(literal) = unnamed_arguments(list)?
            .first()
            .and_then(|value| hex_or_bit_literal(value))
        {
            args[0] = binary_literal_as_double(literal)?;
        }
    }
    if let Some(LiteralResult::TextArgument(position, text)) = &digits {
        args[*position] = string_expr(text);
    }
    let timestamp_decimals = match upper_name.as_str() {
        "UNIX_TIMESTAMP" if over.is_none() => unix_timestamp_decimals(list, &mut args)?,
        _ => None,
    };
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
    if let Some(decimals) = timestamp_decimals {
        return Ok(ast::Expr::cast(
            call,
            Some(sized_decimal_type(11 + decimals, decimals)),
        ));
    }
    let json_timestamp = upper_name == "UNIX_TIMESTAMP"
        && matches!(unnamed_arguments(list)?.as_slice(), [arg] if json::is_json_value(arg));
    Ok(match digits {
        _ if json_timestamp => ast::Expr::cast(call, Some(sized_decimal_type(65, 6))),
        Some(LiteralResult::Time(digits)) => {
            function_call("__mysql_cast_as_time", vec![call, number(digits as u64)])
        }
        Some(LiteralResult::DateTime(digits)) => function_call(
            "__mysql_cast_as_datetime",
            vec![call, number(digits as u64)],
        ),
        Some(LiteralResult::TextArgument(_, text)) => function_call(
            "__mysql_cast_as_datetime",
            vec![call, number(text_fraction_digits(&text) as u64)],
        ),
        Some(LiteralResult::Text(_)) => unreachable!("a text result returns early"),
        None => call,
    })
}

/// The decimals of `UNIX_TIMESTAMP` of a literal, which MySQL types as a
/// DECIMAL with the fractional digits its argument reads with: those of a
/// DECIMAL literal, six for a DOUBLE, and for a string those it reads as a
/// datetime with, or six when it does not read as one. A DECIMAL literal a
/// DOUBLE cannot hold exactly is passed as its text where both read alike,
/// as a 14-digit date and time.
fn unix_timestamp_decimals(
    list: &FunctionArgumentList,
    args: &mut [ast::Expr],
) -> Result<Option<u64>, MysqlParserError> {
    use crate::datetime::parse::{parse_datetime, DateFlags};
    let arguments = unnamed_arguments(list)?;
    let [arg] = arguments.as_slice() else {
        return Ok(None);
    };
    if let Some(digits) = signed_number_literal(arg) {
        if digits.contains(['e', 'E']) {
            return Ok(Some(6));
        }
        let Some((whole, fraction)) = digits.split_once('.') else {
            return Ok(None);
        };
        if fraction.len() > 6 {
            return Err(MysqlParserError::Unsupported(format!(
                "UNIX_TIMESTAMP of `{arg}`, with more than six decimals, is not lowered yet"
            )));
        }
        if !is_exact_as_a_double(digits) {
            if is_negated(arg) || whole.len() != 14 {
                return Err(MysqlParserError::Unsupported(format!(
                    "UNIX_TIMESTAMP of `{arg}`, which a DOUBLE cannot hold exactly, is not lowered yet"
                )));
            }
            args[0] = string_expr(digits);
        }
        return Ok(Some(fraction.len() as u64));
    }
    let text = match unnested(arg) {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text,
            _ => return Ok(None),
        },
        Expr::Prefixed { prefix, value } => {
            match (prefix.value.to_ascii_lowercase().as_str(), value.as_ref()) {
                ("_utf8mb4" | "_utf8mb3" | "_utf8" | "_binary", Expr::Value(value)) => {
                    match &value.value {
                        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text,
                        _ => return Ok(None),
                    }
                }
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    let decimals = parse_datetime(text.as_bytes(), DateFlags::FUZZY)
        .map_or(6, |parsed| u64::from(parsed.fraction_digits));
    Ok((decimals > 0).then_some(decimals))
}

enum LiteralResult {
    Time(usize),
    DateTime(usize),
    TextArgument(usize, String),
    Text(String),
}

fn literal_result_digits(
    upper_name: &str,
    list: &FunctionArgumentList,
) -> Result<Option<LiteralResult>, MysqlParserError> {
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Ok(None);
    }
    let Ok(args) = unnamed_arguments(list) else {
        return Ok(None);
    };
    let (position, time) = match (upper_name, args.len()) {
        ("SEC_TO_TIME" | "TIME", 1) => (0, true),
        ("MAKETIME", 3) => (2, true),
        ("FROM_UNIXTIME", 1) | ("CONVERT_TZ", 3) => (0, false),
        _ => return Ok(None),
    };
    let arg = args[position];
    if upper_name == "TIME" && dated_literal_kind(arg) == Some(TemporalKind::Date) {
        return Ok(Some(LiteralResult::Text("00:00:00".to_string())));
    }
    let Some(digits) = literal_decimals(arg) else {
        return Ok(None);
    };
    if let Some(number) = signed_number_literal(arg)
        .filter(|number| !is_exact_as_a_double(number) && !saturates(upper_name, number))
    {
        let whole = number.split_once('.').map_or(number, |(whole, _)| whole);
        if upper_name == "CONVERT_TZ" && !is_negated(arg) && whole.len() == 14 {
            return Ok(Some(LiteralResult::TextArgument(
                position,
                number.to_string(),
            )));
        }
        return Err(MysqlParserError::Unsupported(format!(
            "{upper_name} of the DECIMAL `{number}`, which MySQL reads exactly and Turso as a DOUBLE, is not lowered yet"
        )));
    }
    if upper_name == "CONVERT_TZ" && dated_literal_kind(arg).is_some() {
        return Ok(None);
    }
    Ok(Some(if time {
        LiteralResult::Time(digits)
    } else {
        LiteralResult::DateTime(digits)
    }))
}

/// A number of 12 or more integer digits, which SEC_TO_TIME saturates and
/// MAKETIME and FROM_UNIXTIME reject as NULL whatever its other digits.
fn saturates(upper_name: &str, digits: &str) -> bool {
    let whole = digits.split_once('.').map_or(digits, |(whole, _)| whole);
    matches!(upper_name, "SEC_TO_TIME" | "MAKETIME" | "FROM_UNIXTIME")
        && whole.trim_start_matches('0').len() >= 12
}

fn is_exact_as_a_double(digits: &str) -> bool {
    if digits.contains(['e', 'E']) || !digits.contains('.') {
        return true;
    }
    let significant = digits.replace('.', "");
    significant
        .trim_start_matches('0')
        .trim_end_matches('0')
        .len()
        <= 15
}

fn text_fraction_digits(text: &str) -> usize {
    text.split_once('.')
        .map_or(0, |(_, fraction)| fraction.len().min(6))
}

fn literal_decimals(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Nested(inner)
        | Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: inner,
        } => literal_decimals(inner),
        Expr::TypedString(_) => dated_literal_kind(expr).map(|_| fraction_length(expr)),
        _ => {
            let digits = number_literal(expr)?;
            if digits.contains(['e', 'E']) {
                return None;
            }
            Some(text_fraction_digits(digits))
        }
    }
}

fn round_decimal_integer_arguments(upper_name: &str, args: &mut [ast::Expr]) {
    let positions: &[usize] = match upper_name {
        "MAKEDATE" | "PERIOD_ADD" | "PERIOD_DIFF" => &[0, 1],
        "MAKETIME" => &[0, 1],
        "FROM_DAYS" => &[0],
        _ => return,
    };
    for &position in positions {
        let Some(arg) = args.get_mut(position) else {
            continue;
        };
        let (negative, digits) = match &*arg {
            ast::Expr::Literal(ast::Literal::Numeric(digits)) => (false, digits.clone()),
            ast::Expr::Unary(ast::UnaryOperator::Negative, inner) => match inner.as_ref() {
                ast::Expr::Literal(ast::Literal::Numeric(digits)) => (true, digits.clone()),
                _ => continue,
            },
            ast::Expr::Cast {
                expr: inner,
                type_name: Some(type_name),
            } if type_name.name.eq_ignore_ascii_case("DECIMAL") => match inner.as_ref() {
                ast::Expr::Literal(ast::Literal::String(quoted)) => {
                    let text = quoted.trim_matches('\'');
                    match text.strip_prefix('-') {
                        Some(digits) => (true, digits.to_string()),
                        None => (false, text.to_string()),
                    }
                }
                _ => continue,
            },
            _ => continue,
        };
        if digits.contains(['e', 'E']) {
            continue;
        }
        if matches!(upper_name, "PERIOD_ADD" | "PERIOD_DIFF") {
            if let Some(saturated) = saturated_decimal_integer(negative, &digits) {
                *arg = saturated;
                continue;
            }
        }
        let Some((whole, fraction)) = digits.split_once('.') else {
            continue;
        };
        let Ok(whole) = (if whole.is_empty() { "0" } else { whole }).parse::<u64>() else {
            continue;
        };
        let rounded = whole + u64::from(fraction.starts_with(['5', '6', '7', '8', '9']));
        let literal = ast::Expr::Literal(ast::Literal::Numeric(rounded.to_string()));
        *arg = if negative && rounded != 0 {
            ast::Expr::Unary(ast::UnaryOperator::Negative, Box::new(literal))
        } else {
            literal
        };
    }
}

/// The BIGINT a DECIMAL literal beyond the BIGINT range reads as, as MySQL's
/// integer value of a DECIMAL saturates (`PERIOD_ADD(18446744073709551616, 1)`
/// reads 9223372036854775807). Integer literals up to the BIGINT UNSIGNED
/// limit are BIGINT UNSIGNED, not DECIMAL, and are left alone.
fn saturated_decimal_integer(negative: bool, digits: &str) -> Option<ast::Expr> {
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let whole = whole.trim_start_matches('0');
    let rounds_up = fraction.starts_with(['5', '6', '7', '8', '9']);
    let magnitude = if whole.is_empty() {
        Some(0)
    } else {
        whole.parse::<u128>().ok()
    }
    .map(|whole| whole + u128::from(rounds_up));
    let limit = if negative {
        i64::MIN.unsigned_abs()
    } else {
        i64::MAX.unsigned_abs()
    };
    let decimal = !fraction.is_empty()
        || exceeds_unsigned_bigint(whole)
        || (negative && magnitude.is_none_or(|magnitude| magnitude > u128::from(limit)));
    if !decimal || magnitude.is_some_and(|magnitude| magnitude <= u128::from(limit)) {
        return None;
    }
    Some(if negative {
        smallest_bigint()
    } else {
        ast::Expr::Literal(ast::Literal::Numeric(i64::MAX.to_string()))
    })
}

/// `REPLACE`, `LPAD`, `RPAD` and string-comparing `FIELD` of a `_latin1` hex
/// subject and literals: MySQL converts the other strings to the subject's
/// `latin1` when it prepares the call and rejects a string with a character
/// `latin1` lacks, whatever the other arguments (`LPAD(_latin1 0xFF, -1,
/// '中')` fails). A `NULL` argument can spare the call, so it is left alone.
fn latin1_subject_collation_error(
    upper_name: &str,
    list: &FunctionArgumentList,
) -> Option<MysqlParserError> {
    enum Literal<'a> {
        Text(&'a str),
        Number,
        Binary,
    }
    fn quoted(value: &Value) -> Option<&str> {
        match value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Some(text),
            _ => None,
        }
    }
    fn literal(expr: &Expr) -> Option<Literal<'_>> {
        match expr {
            Expr::Value(value) => quoted(&value.value).map(Literal::Text).or_else(|| {
                hex_or_bit_literal(expr)
                    .map(|_| Literal::Binary)
                    .or_else(|| signed_number_literal(expr).map(|_| Literal::Number))
            }),
            Expr::Prefixed { prefix, value } => {
                let Expr::Value(value) = value.as_ref() else {
                    return None;
                };
                match prefix.value.to_ascii_lowercase().as_str() {
                    "_utf8mb4" | "_utf8mb3" | "_utf8" => quoted(&value.value).map(Literal::Text),
                    "_latin1" if matches!(value.value, Value::HexStringLiteral(_)) => {
                        Some(Literal::Binary)
                    }
                    _ => None,
                }
            }
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                ..
            } => signed_number_literal(expr).map(|_| Literal::Number),
            Expr::Nested(inner) => literal(inner),
            _ => None,
        }
    }
    let args = unnamed_arguments(list).ok()?;
    let (subject, rest) = args.split_first()?;
    let latin1_hex = matches!(
        subject,
        Expr::Prefixed { prefix, value }
            if prefix.quote_style.is_none()
                && prefix.value.eq_ignore_ascii_case("_latin1")
                && matches!(value.as_ref(), Expr::Value(value) if matches!(value.value, Value::HexStringLiteral(_)))
    );
    if !latin1_hex {
        return None;
    }
    let literals = rest
        .iter()
        .map(|arg| literal(arg))
        .collect::<Option<Vec<_>>>()?;
    let converted: Vec<&Literal> = match (upper_name, literals.as_slice()) {
        ("REPLACE", [_, _]) => literals.iter().collect(),
        ("LPAD" | "RPAD", [_, pad]) => vec![pad],
        ("FIELD", [_, ..])
            if !literals
                .iter()
                .any(|literal| matches!(literal, Literal::Number)) =>
        {
            literals.iter().collect()
        }
        _ => return None,
    };
    let lacking = converted.into_iter().find_map(|literal| match literal {
        Literal::Text(text) if !text.chars().all(is_latin1_char) => Some(*text),
        _ => None,
    })?;
    Some(MysqlParserError::Rejected(format!(
        "Illegal mix of collations for operation '{}': '{lacking}' has a character latin1 lacks",
        upper_name.to_ascii_lowercase()
    )))
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
    let refused = |temporal: &Expr| {
        MysqlParserError::Unsupported(format!(
            "{upper_name} of `{temporal}` and a string with a collation, which MySQL compares as temporal values, is not lowered yet"
        ))
    };
    if let Some(temporal) = args
        .iter()
        .find(|arg| temporal_type(arg).is_none() && has_a_temporal_type(arg))
    {
        return Err(refused(temporal));
    }
    if let Some(temporal) = args.iter().find(|arg| is_dated_literal(arg)) {
        match upper_name {
            "GREATEST" => {
                return greatest_of_dates(&args)?
                    .map(Some)
                    .ok_or_else(|| refused(temporal))
            }
            "LEAST"
                if args
                    .iter()
                    .any(|arg| matches!(date_read(arg), Some(DateRead::NotADate(_)))) => {}
            "STRCMP" => {
                let not_a_date = args.iter().find(|arg| {
                    !is_dated_literal(arg) && matches!(date_read(arg), Some(DateRead::NotADate(_)))
                });
                return Err(match not_a_date {
                    Some(text) => MysqlParserError::Rejected(format!(
                        "STRCMP reads `{text}` as a date next to `{temporal}`, and it is not one"
                    )),
                    None => refused(temporal),
                });
            }
            _ => return Err(refused(temporal)),
        }
    } else if let Some(temporal) = args
        .iter()
        .find(|arg| upper_name == "FIELD" && temporal_type(arg).is_some())
    {
        return Err(refused(temporal));
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

fn translate_greatest_of_dates(
    upper_name: &str,
    list: &FunctionArgumentList,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    if upper_name != "GREATEST" || list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Ok(None);
    }
    let Ok(args) = unnamed_arguments(list) else {
        return Ok(None);
    };
    if !args.iter().any(|arg| is_dated_literal(arg)) || single_explicit_collation(&args)?.is_some()
    {
        return Ok(None);
    }
    greatest_of_dates(&args)
}

fn is_dated_literal(expr: &Expr) -> bool {
    match expr {
        Expr::TypedString(typed) => {
            temporal_literal(typed).is_some_and(|literal| literal.kind != TemporalKind::Time)
        }
        Expr::Nested(inner) => is_dated_literal(inner),
        _ => false,
    }
}

enum DateRead {
    Null,
    Temporal(TemporalKind, String),
    NotADate(usize),
}

fn date_read(expr: &Expr) -> Option<DateRead> {
    const STRING_DECIMALS: usize = 6;
    match expr {
        Expr::Nested(inner) | Expr::Collate { expr: inner, .. } => date_read(inner),
        Expr::TypedString(typed) => {
            let literal = temporal_literal(typed)?;
            (literal.kind != TemporalKind::Time)
                .then_some(DateRead::Temporal(literal.kind, literal.text))
        }
        Expr::Value(value) => match &value.value {
            Value::Null => Some(DateRead::Null),
            Value::Number(digits, _) => number_is_not_a_date(digits).map(DateRead::NotADate),
            Value::SingleQuotedString(text)
            | Value::DoubleQuotedString(text)
            | Value::NationalStringLiteral(text) => {
                is_not_a_date(text.as_bytes()).then_some(DateRead::NotADate(STRING_DECIMALS))
            }
            Value::HexStringLiteral(_) | Value::SingleQuotedByteStringLiteral(_) => {
                let bytes = binary_literal_bytes(expr)?;
                (std::str::from_utf8(&bytes).is_ok() && is_not_a_date(&bytes))
                    .then_some(DateRead::NotADate(0))
            }
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: inner,
        } => match inner.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(digits, _) => number_is_not_a_date(digits).map(DateRead::NotADate),
                _ => None,
            },
            _ => None,
        },
        Expr::Prefixed { prefix, value } if prefix.quote_style.is_none() => {
            let introducer = prefix.value.to_ascii_lowercase();
            if !matches!(
                introducer.as_str(),
                "_binary" | "_utf8mb4" | "_utf8mb3" | "_utf8"
            ) {
                return None;
            }
            let text = string_literal(value)?;
            is_not_a_date(text.as_bytes()).then_some(DateRead::NotADate(STRING_DECIMALS))
        }
        Expr::Cast {
            kind: CastKind::Cast,
            expr: inner,
            data_type:
                DataType::Signed
                | DataType::SignedInteger
                | DataType::Unsigned
                | DataType::UnsignedInteger,
            array: false,
            format: None,
        } => match inner.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(digits, _) if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
                    is_not_a_date(digits.as_bytes()).then_some(DateRead::NotADate(0))
                }
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

fn number_is_not_a_date(digits: &str) -> Option<usize> {
    if digits.contains(['e', 'E']) {
        let value: f64 = digits.parse().ok()?;
        return (value < 10_000.0).then_some(6);
    }
    let scale = digits
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    is_not_a_date(digits.as_bytes()).then_some(scale)
}

fn is_not_a_date(text: &[u8]) -> bool {
    let start = text
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r'))
        .unwrap_or(text.len());
    let text = &text[start..];
    if !text.first().is_some_and(u8::is_ascii_digit) {
        return true;
    }
    let digits = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if !matches!(text.get(digits), None | Some(b'.')) {
        return false;
    }
    if digits <= 4 {
        return true;
    }
    if digits == 8 || digits >= 14 {
        let field = |range: std::ops::Range<usize>| {
            std::str::from_utf8(&text[range])
                .expect("the digits are ASCII")
                .parse::<u32>()
                .expect("the field is digits")
        };
        return field(4..6) > 12 || field(6..8) > 31;
    }
    false
}

fn greatest_of_dates(args: &[&Expr]) -> Result<Option<ast::Expr>, MysqlParserError> {
    let mut best: Option<(String, String)> = None;
    let mut datetime = false;
    let mut decimals = 0;
    let mut read_a_value_that_is_not_a_date = false;
    let mut null = false;
    for arg in args {
        let Some(read) = date_read(arg) else {
            return Err(MysqlParserError::Unsupported(format!(
                "GREATEST of a date and `{arg}`, which MySQL may read as a datetime, is not lowered yet"
            )));
        };
        match read {
            DateRead::Null => null = true,
            DateRead::NotADate(arg_decimals) => {
                read_a_value_that_is_not_a_date = true;
                decimals = decimals.max(arg_decimals);
            }
            DateRead::Temporal(kind, text) => {
                let (date, clock) = match kind {
                    TemporalKind::Date => (text.clone(), "00:00:00".to_string()),
                    TemporalKind::DateTime => {
                        datetime = true;
                        let (date, clock) = text.split_once(' ').expect("a datetime has a clock");
                        (date.to_string(), clock.to_string())
                    }
                    TemporalKind::Time => unreachable!("date_read refuses TIME literals"),
                };
                let fraction = clock.split_once('.').map_or("", |(_, fraction)| fraction);
                decimals = decimals.max(fraction.len());
                let key = format!("{date} {clock:.8}.{fraction:0<6}");
                if best.as_ref().is_none_or(|(best_key, _)| key > *best_key) {
                    best = Some((key, date));
                }
            }
        }
    }
    if !read_a_value_that_is_not_a_date {
        return Ok(None);
    }
    if null {
        return Ok(Some(ast::Expr::Literal(ast::Literal::Null)));
    }
    let (key, date) = best.expect("a dated literal is among the arguments");
    if !datetime {
        return Ok(Some(string_expr(&date)));
    }
    let decimals = decimals.min(6);
    let clock = &key[date.len() + 1..date.len() + 9];
    let fraction = &key[date.len() + 10..date.len() + 10 + decimals];
    let text = if decimals == 0 {
        format!("{date} {clock}")
    } else {
        format!("{date} {clock}.{fraction}")
    };
    Ok(Some(string_expr(&text)))
}

/// The fractional digits MySQL shows `TIMESTAMPADD(SECOND, n, d)` with when
/// `n` is a literal with decimals: those of a DECIMAL, four more than the
/// dividend's for a division of exact numbers, six for a double or a string,
/// or the date's when it has more; `None` when `n` has none or `d` is not a
/// literal.
fn added_seconds_digits(seconds: &Expr, date: &Expr) -> Option<u64> {
    fn scale(expr: &Expr) -> Option<u64> {
        let digits = signed_number_literal(expr)?;
        if digits.contains(['e', 'E']) {
            return None;
        }
        Some(
            digits
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len() as u64),
        )
    }
    let seconds_digits = match unnested(seconds) {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Divide,
            right,
        } => scale(left).zip(scale(right)).map(|(dividend, _)| dividend + 4)?,
        _ if is_double_literal(seconds) => 6,
        _ if is_text_literal(seconds)
            || matches!(unnested(seconds), Expr::Prefixed { value, .. } if is_text_literal(value)) =>
        {
            6
        }
        _ => scale(seconds)?,
    }
    .min(6);
    if seconds_digits == 0 {
        return None;
    }
    let date_digits = match unnested(date) {
        Expr::TypedString(_) => {
            dated_literal_kind(date)?;
            fraction_length(date) as u64
        }
        _ => return None,
    };
    Some(seconds_digits.max(date_digits))
}

/// `JSON_ARRAYAGG(x)` and `JSON_OBJECTAGG(k, v)`: each row becomes
/// `JSON_ARRAY(x)` or `JSON_OBJECT(k, v)`, so its value converts as in those
/// functions, `json_group_array` collects the rows, and the dialect function
/// around it unwraps them (NULL for a group without rows, the last value of
/// a repeated key).
fn translate_json_aggregate(
    function: &Function,
    upper_name: &str,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let (row_function, finish, arity) = match upper_name {
        "JSON_ARRAYAGG" => ("JSON_ARRAY", "__mysql_json_arrayagg", 1),
        _ => ("JSON_OBJECT", "__mysql_json_objectagg", 2),
    };
    let FunctionArguments::List(list) = &function.args else {
        return Err(unsupported_function(function));
    };
    if function.over.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "{upper_name} as a window function is not lowered yet"
        )));
    }
    if list.duplicate_treatment.is_some() || !list.clauses.is_empty() {
        return Err(MysqlParserError::Rejected(format!(
            "{upper_name} takes no DISTINCT, ORDER BY or other clause"
        )));
    }
    if unnamed_arguments(list)?.len() != arity {
        return Err(MysqlParserError::Rejected(format!(
            "incorrect parameter count in the call to {upper_name}"
        )));
    }
    let mut row = function.clone();
    row.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(row_function))]);
    let row = translate_in(&Expr::Function(row), Context::Value, context)?;
    Ok(function_call(
        finish,
        vec![function_call("json_group_array", vec![row])],
    ))
}

/// `VARIANCE(x)`, `STD(x)` and their sample and population variants: MySQL
/// reads `x` as a DOUBLE and runs Welford's recurrence over the rows, as the
/// dialect's aggregates do. Turso runs no extension aggregate as a window
/// function.
fn translate_variance(
    function: &Function,
    upper_name: &str,
    variance: &str,
    list: &FunctionArgumentList,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if matches!(list.duplicate_treatment, Some(DuplicateTreatment::Distinct))
        || !list.clauses.is_empty()
    {
        return Err(MysqlParserError::Rejected(format!(
            "{upper_name} takes no DISTINCT, ORDER BY or other clause"
        )));
    }
    let [FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))] = list.args.as_slice() else {
        return Err(MysqlParserError::Rejected(format!(
            "incorrect parameter count in the call to {upper_name}"
        )));
    };
    if function.over.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "{upper_name} as a window function is not lowered yet"
        )));
    }
    let mut call = function.clone();
    call.name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(variance))]);
    call.args = FunctionArguments::List(FunctionArgumentList {
        duplicate_treatment: None,
        args: vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Cast {
            kind: CastKind::Cast,
            expr: Box::new(arg.clone()),
            data_type: DataType::Double(ExactNumberInfo::None),
            array: false,
            format: None,
        }))],
        clauses: Vec::new(),
    });
    translate_in(&Expr::Function(call), Context::Value, context)
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
        ("RANDOM_BYTES", [length]) => {
            let length = translate_in(length, argument_context(upper_name, 0, &args), context)?;
            Ok(Some(function_call(
                "__mysql_random_bytes",
                vec![length, function_call("randomblob", vec![number(1024)])],
            )))
        }
        ("DATE_ADD" | "ADDDATE", [date, Expr::Interval(interval)]) => Ok(Some(
            translate_date_arithmetic("DATE_ADD", date, interval, context)?,
        )),
        ("DATE_SUB" | "SUBDATE", [date, Expr::Interval(interval)]) => Ok(Some(
            translate_date_arithmetic("DATE_SUB", date, interval, context)?,
        )),
        ("TIMESTAMPADD" | "TIMESTAMPDIFF", [Expr::Identifier(unit), rest @ ..])
            if unit.quote_style.is_none() =>
        {
            let digits = match (upper_name, rest) {
                ("TIMESTAMPADD", [seconds, date]) if unit.value.eq_ignore_ascii_case("SECOND") => {
                    added_seconds_digits(seconds, date)
                }
                _ => None,
            };
            let unit = unit_literal(&DateTimeField::Custom(unit.clone()))?;
            let mut translated = vec![unit];
            for arg in rest {
                translated.push(translate_expr(arg, context)?);
            }
            let call = function_call(upper_name, translated);
            Ok(Some(match digits {
                Some(digits) => {
                    function_call("__mysql_cast_as_datetime", vec![call, number(digits)])
                }
                None => call,
            }))
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

pub(crate) fn collation_of(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::Nested(inner) => collation_of(inner),
        Expr::Identifier(identifier)
            if identifier.quote_style.is_none()
                && crate::session::user_variable_name(&identifier.value).is_some() =>
        {
            Some(DEFAULT_COLLATION)
        }
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

fn translate_date_arithmetic(
    name: &str,
    date: &Expr,
    interval: &Interval,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    const DATE_UNITS: &[&str] = &["YEAR", "QUARTER", "MONTH", "WEEK", "DAY", "YEAR_MONTH"];
    let (amount, unit) = translate_interval(interval, context)?;
    let unit_name = match &unit {
        ast::Expr::Literal(ast::Literal::String(quoted)) => quoted.trim_matches('\'').to_string(),
        other => unreachable!("a temporal unit lowers to a string literal, not `{other}`"),
    };
    let call = function_call(name, vec![translate_expr(date, context)?, amount, unit]);
    let Some(kind) = dated_literal_kind(date) else {
        return Ok(call);
    };
    if kind == TemporalKind::Time {
        return Err(MysqlParserError::Unsupported(format!(
            "{name} of the TIME literal `{date}`, whose result MySQL types as TIME, is not lowered yet"
        )));
    }
    if kind == TemporalKind::Date && DATE_UNITS.contains(&unit_name.as_str()) {
        return Ok(call);
    }
    let Some(interval_digits) = interval_fraction_digits(&interval.value, &unit_name) else {
        return Err(MysqlParserError::Unsupported(format!(
            "{name} of `{date}` and the interval `{interval}`, whose fractional digits MySQL takes from the interval's type, is not lowered yet"
        )));
    };
    let digits = fraction_length(date).max(interval_digits);
    Ok(function_call(
        "__mysql_cast_as_datetime",
        vec![call, number(digits as u64)],
    ))
}

fn dated_literal_kind(expr: &Expr) -> Option<TemporalKind> {
    match expr {
        Expr::TypedString(typed) => temporal_literal(typed).map(|literal| literal.kind),
        Expr::Nested(inner) => dated_literal_kind(inner),
        _ => None,
    }
}

fn interval_fraction_digits(value: &Expr, unit: &str) -> Option<usize> {
    if unit.ends_with("MICROSECOND") {
        return Some(6);
    }
    if unit != "SECOND" {
        return Some(0);
    }
    if let Some(digits) = signed_number_literal(value) {
        if digits.contains(['e', 'E']) {
            return Some(6);
        }
        return Some(
            digits
                .split_once('.')
                .map_or(0, |(_, fraction)| fraction.len().min(6)),
        );
    }
    if string_literal(value).is_some() {
        return Some(6);
    }
    matches!(value, Expr::Value(literal) if literal.value == Value::Null).then_some(0)
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
    let unit = unit_literal(field)?;
    let amount = translate_expr(value, context)?;
    let compound =
        matches!(&unit, ast::Expr::Literal(ast::Literal::String(name)) if name.contains('_'));
    let amount = if compound && string_literal(value).is_none() {
        mysql_text(amount)
    } else {
        amount
    };
    Ok((amount, unit))
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

/// `ROUND(n, d)` or `TRUNCATE(n, d)` of an integer literal between the
/// BIGINT and BIGINT UNSIGNED limits and a literal `d`, which MySQL computes
/// as BIGINT UNSIGNED: `n` when `d` is not negative, 0 when it is -20 or
/// less, and otherwise `n` truncated or rounded half up to a power of ten,
/// which MySQL rejects above the BIGINT UNSIGNED range.
fn unsigned_bigint_rounding(
    upper_name: &str,
    list: &FunctionArgumentList,
) -> Result<Option<ast::Expr>, MysqlParserError> {
    if !matches!(upper_name, "ROUND" | "TRUNCATE")
        || list.duplicate_treatment.is_some()
        || !list.clauses.is_empty()
    {
        return Ok(None);
    }
    let Ok(args) = unnamed_arguments(list) else {
        return Ok(None);
    };
    let [value, places] = args.as_slice() else {
        return Ok(None);
    };
    let Some(value) = number_literal(value)
        .and_then(|digits| digits.parse::<u64>().ok())
        .filter(|value| *value > i64::MAX as u64)
    else {
        return Ok(None);
    };
    let Some(places) = integer_places(places) else {
        return Ok(None);
    };
    let unsigned = |value: u64| match i64::try_from(value) {
        Ok(value) => ast::Expr::Literal(ast::Literal::Numeric(value.to_string())),
        Err(_) => exact_decimal(&value.to_string(), false),
    };
    if places >= 0 {
        return Ok(Some(unsigned(value)));
    }
    let Some(power) = u32::try_from(-places)
        .ok()
        .and_then(|places| 10u64.checked_pow(places))
    else {
        return Ok(Some(unsigned(0)));
    };
    let floor = value / power * power;
    let result = if upper_name == "TRUNCATE" || value - floor < power / 2 {
        Some(floor)
    } else {
        floor.checked_add(power)
    };
    match result {
        Some(result) => Ok(Some(unsigned(result))),
        None => Err(MysqlParserError::Unsupported(format!(
            "{upper_name} of {value} to {places} places, which MySQL rejects above the BIGINT UNSIGNED range, is not lowered yet"
        ))),
    }
}

/// The places MySQL reads from a literal as an integer: a DECIMAL rounded
/// half away from zero and a DOUBLE rounded half to even, both saturating
/// at the BIGINT range, and 0 or more for a string that does not start
/// with a minus sign.
fn integer_places(expr: &Expr) -> Option<i128> {
    const SMALLEST: i128 = i64::MIN as i128;
    const LARGEST: i128 = i64::MAX as i128;
    if let Some(literal) = hex_or_bit_literal(expr) {
        return binary_literal_value(literal).ok().map(i128::from);
    }
    if let Some(literal) = negated_binary(expr) {
        return binary_literal_value(literal)
            .ok()
            .map(|value| -i128::from(value));
    }
    if let Some(digits) = signed_number_literal(expr) {
        let sign = if is_negated(expr) { -1 } else { 1 };
        if digits.contains(['e', 'E']) {
            let magnitude = digits.parse::<f64>().ok()?;
            let rounded = if sign < 0 { -magnitude } else { magnitude }.round_ties_even();
            return Some(if rounded <= SMALLEST as f64 {
                SMALLEST
            } else if rounded >= LARGEST as f64 {
                LARGEST
            } else {
                rounded as i128
            });
        }
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        let whole = whole.trim_start_matches('0');
        let magnitude = if whole.len() > 20 {
            LARGEST + 1
        } else {
            whole.parse::<i128>().unwrap_or(0)
                + i128::from(
                    fraction
                        .as_bytes()
                        .first()
                        .is_some_and(|digit| *digit >= b'5'),
                )
        };
        return Some((sign * magnitude).clamp(SMALLEST, LARGEST));
    }
    let text = match expr {
        Expr::Prefixed { value, .. } => value.as_ref(),
        other => other,
    };
    match text {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                (!text.trim_start().starts_with('-')).then_some(0)
            }
            _ => None,
        },
        _ => None,
    }
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
    if upper_name == "STRCMP" && args.iter().any(|arg| is_dated_literal(arg)) {
        use crate::datetime::parse as datetime;
        for text in args.iter().filter_map(|arg| string_literal(arg)) {
            if datetime::parse_datetime(text.as_bytes(), datetime::DateFlags::FUZZY).is_err() {
                return Err(MysqlParserError::Rejected(format!(
                    "STRCMP reads `{text}` as a date next to a DATE or DATETIME, and it is not one"
                )));
            }
            if datetime::parse_datetime(text.as_bytes(), datetime::DateFlags::CAST).is_err() {
                return Err(MysqlParserError::Unsupported(format!(
                    "STRCMP of the zero date `{text}` and a DATE or DATETIME is not lowered yet"
                )));
            }
        }
    }
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
    if matches!(upper_name, "CONCAT" | "CONCAT_WS" | "STRCMP")
        && args.iter().any(|arg| json::is_json_value(arg))
    {
        if let Some(arg) = args.iter().find(|arg| {
            binary_literal_bytes(arg).is_some_and(|bytes| std::str::from_utf8(&bytes).is_err())
        }) {
            return Err(MysqlParserError::Rejected(format!(
                "cannot convert the binary string `{arg}` of {upper_name} to utf8mb4 next to a JSON value"
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
            let literal_context = argument_context(upper_name, position, &args);
            if upper_name == "MOD"
                || (position == 0
                    && matches!(
                        upper_name,
                        "ABS" | "CEIL" | "CEILING" | "FLOOR" | "ROUND" | "TRUNCATE" | "CONCAT_WS"
                    ))
            {
                return unsigned_bigint_operand(arg, literal_context, context);
            }
            translate_in(arg, literal_context, context)
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
        "CONCAT_WS"
            if position == 0
                && !json::is_json_value(args[0])
                && (!is_binary_literal(args[0]) || is_utf8_binary_literal(args[0])) =>
        {
            Context::Text
        }
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
        "ELT" | "MAKE_SET" | "SPACE" | "RANDOM_BYTES" => position == 0,
        "FORMAT" | "LEFT" | "LPAD" | "REPEAT" | "RIGHT" | "RPAD" | "SHA2" => position == 1,
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

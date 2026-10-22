use crate::dialect::TEMPORAL_UNITS;
use crate::literal::{
    translate_as_bytes, translate_introduced_string, translate_temporal_text,
    translate_typed_string, translate_value, Context,
};
use crate::names::{quote_string, translate_identifier};
use crate::query::{translate_order_by_expr, translate_subquery, TranslationContext};
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, CaseWhen, CastKind, CeilFloorKind, DataType, DateTimeField, DuplicateTreatment,
    ExactNumberInfo, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentClause,
    FunctionArgumentList, FunctionArguments, Ident, Interval, ObjectName, ObjectNamePart, Query,
    TrimWhereField, UnaryOperator, Value, WindowFrame, WindowFrameBound, WindowFrameUnits,
    WindowSpec, WindowType,
};
use turso_parser::ast;

pub(crate) fn translate_expr(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    translate_in(expr, Context::Value, context)
}

pub(crate) fn translate_condition(
    expr: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    translate_in(expr, Context::Number, context)
}

fn translate_in(
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
            translate_introduced_string(prefix, value, literal_context)
        }
        Expr::TypedString(typed) => translate_typed_string(typed, literal_context),
        Expr::Nested(inner) => translate_nested(inner, literal_context, context),
        Expr::Identifier(identifier) => translate_identifier_expr(identifier),
        Expr::CompoundIdentifier(parts) => translate_compound_identifier(parts),
        Expr::UnaryOp { op, expr } => translate_unary(*op, expr, literal_context, context),
        Expr::BinaryOp { left, op, right } => translate_binary(left, op, right, context),
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
        Expr::Function(function) => translate_function(function, context),
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

fn translate_value_expr(
    value: &Value,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    match value {
        Value::Placeholder(placeholder) if placeholder == "?" => {
            Ok(ast::Expr::Variable(context.next_parameter()?))
        }
        other => translate_value(other, literal_context),
    }
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
        UnaryOperator::Minus => (ast::UnaryOperator::Negative, sign_context),
        UnaryOperator::Plus => (ast::UnaryOperator::Positive, sign_context),
        UnaryOperator::Not | UnaryOperator::BangNot => (ast::UnaryOperator::Not, Context::Number),
        UnaryOperator::BitwiseNot => (ast::UnaryOperator::BitwiseNot, Context::Arithmetic),
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

fn translate_binary(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let (operator, literal_context) = match op {
        BinaryOperator::Plus | BinaryOperator::Minus
            if matches!(left, Expr::Interval(_)) || matches!(right, Expr::Interval(_)) =>
        {
            return translate_interval_arithmetic(left, op, right, context)
        }
        BinaryOperator::Plus => (ast::Operator::Add, Context::Arithmetic),
        BinaryOperator::Minus => (ast::Operator::Subtract, Context::Arithmetic),
        BinaryOperator::Multiply => (ast::Operator::Multiply, Context::Arithmetic),
        BinaryOperator::Divide => (ast::Operator::Divide, Context::Arithmetic),
        BinaryOperator::Modulo => (ast::Operator::Modulus, Context::Arithmetic),
        BinaryOperator::PGBitwiseShiftLeft => (ast::Operator::LeftShift, Context::Arithmetic),
        BinaryOperator::BitwiseOr => (ast::Operator::BitwiseOr, Context::Bitwise),
        BinaryOperator::BitwiseAnd => (ast::Operator::BitwiseAnd, Context::Bitwise),
        BinaryOperator::PGBitwiseShiftRight => (ast::Operator::RightShift, Context::Bitwise),
        BinaryOperator::And => (ast::Operator::And, Context::Number),
        BinaryOperator::Or | BinaryOperator::StringConcat => (ast::Operator::Or, Context::Number),
        BinaryOperator::Eq => return comparison(left, ast::Operator::Equals, right, context),
        BinaryOperator::NotEq => return comparison(left, ast::Operator::NotEquals, right, context),
        BinaryOperator::Lt => return comparison(left, ast::Operator::Less, right, context),
        BinaryOperator::LtEq => return comparison(left, ast::Operator::LessEquals, right, context),
        BinaryOperator::Gt => return comparison(left, ast::Operator::Greater, right, context),
        BinaryOperator::GtEq => {
            return comparison(left, ast::Operator::GreaterEquals, right, context)
        }
        BinaryOperator::Spaceship => return comparison(left, ast::Operator::Is, right, context),
        BinaryOperator::MyIntegerDivide => return translate_integer_division(left, right, context),
        BinaryOperator::Xor => return translate_logical_xor(left, right, context),
        BinaryOperator::BitwiseXor => return translate_bitwise_xor(left, right, context),
        BinaryOperator::Arrow => return translate_json_arrow(left, right, false, context),
        BinaryOperator::LongArrow => return translate_json_arrow(left, right, true, context),
        other => return Err(unsupported_operator(other)),
    };
    operation(left, operator, right, literal_context, context)
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
    let quotient = operation(
        left,
        ast::Operator::Divide,
        right,
        Context::Arithmetic,
        context,
    )?;
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
    let lhs = translate_compared(left, right, context)?;
    let rhs = translate_compared(right, left, context)?;
    Ok(ast::Expr::binary(lhs, operator, rhs))
}

fn translate_compared(
    expr: &Expr,
    other_side: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if is_json_cast(expr) {
        return Err(json_outside_a_value_position(expr));
    }
    translate_compared_value(expr, other_side, context)
}

fn translate_compared_value(
    expr: &Expr,
    other_side: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if let Some(data_type) = temporal_type(other_side) {
        if let Some(text) = string_literal(expr) {
            return translate_temporal_text(data_type, text);
        }
        if is_literal(expr) && temporal_type(expr).is_none() {
            return Err(MysqlParserError::Unsupported(format!(
                "comparison of `{other_side}` with `{expr}`, which MySQL converts to a temporal value, is not lowered yet"
            )));
        }
    }
    translate_in(expr, comparison_context(other_side), context)
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
    let inner = Box::new(translate_in(inner, literal_context, context)?);
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
    let lhs = match list.first() {
        Some(first) => translate_compared(expr, first, context)?,
        None => translate_expr(expr, context)?,
    };
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
    let lhs = translate_compared(expr, low, context)?;
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
    let lhs = translate_in(expr, Context::Text, context)?;
    let rhs = translate_in(pattern, Context::Text, context)?;
    Ok(ast::Expr::like(
        lhs,
        negated,
        ast::LikeOperator::Regexp,
        rhs,
        None,
    ))
}

fn is_binary_literal(expr: &Expr) -> bool {
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
    let when_then_pairs = conditions
        .iter()
        .map(|case| {
            let when = match operand {
                Some(operand) => translate_compared(&case.condition, operand, context)?,
                None => translate_condition(&case.condition, context)?,
            };
            let then = translate_expr(&case.result, context)?;
            Ok((Box::new(when), Box::new(then)))
        })
        .collect::<Result<Vec<_>, MysqlParserError>>()?;
    let else_expr = else_result
        .map(|result| translate_expr(result, context).map(Box::new))
        .transpose()?;
    Ok(ast::Expr::Case {
        base,
        when_then_pairs,
        else_expr,
    })
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
    let needle = translate_expr(needle, context)?;
    let haystack = translate_expr(haystack, context)?;
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

fn translate_bitwise_xor(
    left: &Expr,
    right: &Expr,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if !has_no_side_effects(left) || !has_no_side_effects(right) {
        return Err(MysqlParserError::Unsupported(
            "`^` over function calls or subqueries is not lowered yet".to_string(),
        ));
    }
    let lhs = translate_in(left, Context::Bitwise, context)?;
    let rhs = translate_in(right, Context::Bitwise, context)?;
    let either = ast::Expr::binary(lhs.clone(), ast::Operator::BitwiseOr, rhs.clone());
    let both = ast::Expr::binary(lhs, ast::Operator::BitwiseAnd, rhs);
    Ok(parenthesized(ast::Expr::binary(
        parenthesized(either),
        ast::Operator::Subtract,
        parenthesized(both),
    )))
}

fn negation(expr: ast::Expr) -> ast::Expr {
    parenthesized(ast::Expr::unary(ast::UnaryOperator::Not, expr))
}

fn parenthesized(expr: ast::Expr) -> ast::Expr {
    ast::Expr::Parenthesized(vec![Box::new(expr)])
}

fn has_no_side_effects(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => !matches!(value.value, Value::Placeholder(_)),
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::TypedString(_)
        | Expr::Prefixed { .. } => true,
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => has_no_side_effects(inner),
        Expr::BinaryOp { left, right, .. } => {
            has_no_side_effects(left) && has_no_side_effects(right)
        }
        _ => false,
    }
}

fn translate_like(
    expr: &Expr,
    negated: bool,
    pattern: &Expr,
    escape: Option<&sqlparser::ast::ValueWithSpan>,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    let lhs = translate_in(expr, Context::Text, context)?;
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

fn string_literal(expr: &Expr) -> Option<&str> {
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
    let (target, operand_context) = match data_type {
        DataType::Signed => (type_name("SIGNED"), Context::Number),
        DataType::SignedInteger => (type_name("SIGNED INTEGER"), Context::Number),
        DataType::Unsigned => (type_name("UNSIGNED"), Context::Number),
        DataType::UnsignedInteger => (type_name("UNSIGNED INTEGER"), Context::Number),
        DataType::Double(ExactNumberInfo::None) => (type_name("DOUBLE"), Context::Number),
        DataType::Real => (type_name("REAL"), Context::Number),
        DataType::Float(ExactNumberInfo::None) => (type_name("FLOAT"), Context::Number),
        DataType::Decimal(precision) => (decimal_type(precision), Context::Number),
        DataType::Char(None) | DataType::Character(None) => (type_name("CHAR"), Context::Value),
        DataType::JSON => return translate_cast_as_json(expr, literal_context, context),
        other => {
            return Err(MysqlParserError::Unsupported(format!(
                "CAST to `{other}` is not lowered yet"
            )))
        }
    };
    Ok(ast::Expr::cast(
        translate_in(expr, operand_context, context)?,
        Some(target),
    ))
}

fn translate_cast_as_json(
    expr: &Expr,
    literal_context: Context,
    context: &mut TranslationContext,
) -> Result<ast::Expr, MysqlParserError> {
    if literal_context != Context::Value {
        return Err(json_outside_a_value_position(expr));
    }
    let value = translate_expr(expr, context)?;
    let name = if is_boolean(expr) {
        "__mysql_cast_boolean_as_json"
    } else {
        "__mysql_cast_as_json"
    };
    Ok(function_call(name, vec![value]))
}

fn json_outside_a_value_position(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "JSON value `{expr}` in a comparison, a computation or a function MySQL evaluates lazily is not lowered yet"
    ))
}

fn decimal_type(precision: &ExactNumberInfo) -> ast::Type {
    let size = match *precision {
        ExactNumberInfo::None => None,
        ExactNumberInfo::Precision(digits) => {
            Some(ast::TypeSize::MaxSize(Box::new(number(digits))))
        }
        ExactNumberInfo::PrecisionAndScale(digits, scale) => Some(ast::TypeSize::TypeSize(
            Box::new(number(digits)),
            Box::new(number(scale.unsigned_abs())),
        )),
    };
    ast::Type {
        name: "DECIMAL".to_string(),
        size,
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
    match single_name(collation)?.to_ascii_lowercase().as_str() {
        "binary" | "utf8mb4_0900_bin" => Ok(ast::Expr::collate(
            translate_expr(expr, context)?,
            ast::Name::exact("BINARY".to_string()),
        )),
        other => Err(MysqlParserError::Unsupported(format!(
            "collation `{other}` is not lowered yet"
        ))),
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
        .map(|removed| translate_expr(removed, context))
        .transpose()?;
    let text = translate_expr(expr, context)?;
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
        if let Some(expr) = translate_special_function(&upper_name, list, context)? {
            return Ok(expr);
        }
    }
    if matches!(upper_name.as_str(), "ROUND" | "TRUNCATE") && has_negative_precision(list) {
        return Err(MysqlParserError::Unsupported(format!(
            "{upper_name} to a negative number of decimals, which Turso does not round to tens, is not lowered yet"
        )));
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
    let mut args = function_arguments(&upper_name, list, context)?;
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
    Ok(ast::Expr::FunctionCall {
        name: function_name,
        distinctness,
        args: args.into_iter().map(Box::new).collect(),
        order_by,
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            over_clause,
            ..filter_over
        },
    })
}

fn has_negative_precision(list: &FunctionArgumentList) -> bool {
    matches!(
        list.args.get(1),
        Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::UnaryOp {
            op: UnaryOperator::Minus,
            ..
        })))
    )
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
        ("LTRIM" | "RTRIM", [text]) => {
            let ends = if upper_name == "LTRIM" {
                TrimEnds::Leading
            } else {
                TrimEnds::Trailing
            };
            Ok(Some(trim_call(ends, translate_expr(text, context)?, None)))
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
    context: &mut TranslationContext,
) -> Result<Vec<ast::Expr>, MysqlParserError> {
    let args = unnamed_arguments(list)?;
    args.iter()
        .enumerate()
        .map(|(position, arg)| {
            if is_json_cast(arg)
                && !upper_name.starts_with("JSON_")
                && (evaluates_arguments_lazily(upper_name)
                    || !other_arguments_are_present_values(&args, position))
            {
                return Err(json_outside_a_value_position(arg));
            }
            if is_json_value_position(upper_name, position) {
                translate_json_value(arg, context)
            } else {
                translate_in(arg, argument_context(upper_name, position, &args), context)
            }
        })
        .collect()
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
        "CONCAT_WS" if position == 0 => Context::Text,
        _ if hex_or_bit_literal(args[position]).is_some()
            && reads_number(upper_name, position, args) =>
        {
            Context::Number
        }
        _ => Context::Value,
    }
}

fn reads_number(upper_name: &str, position: usize, args: &[&Expr]) -> bool {
    match upper_name {
        "ELT" | "MAKE_SET" | "SPACE" => position == 0,
        "LEFT" | "LPAD" | "REPEAT" | "RIGHT" | "RPAD" | "SHA2" => position == 1,
        "CONV" | "INSERT" | "MID" | "SUBSTR" | "SUBSTRING" => matches!(position, 1 | 2),
        "LOCATE" | "SUBSTRING_INDEX" => position == 2,
        "EXPORT_SET" => matches!(position, 0 | 4),
        "INTERVAL" => true,
        "FIELD" => args
            .iter()
            .any(|arg| comparison_context(arg) == Context::Number),
        _ => false,
    }
}

fn hex_or_bit_literal(expr: &Expr) -> Option<&Value> {
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

fn translate_json_value(
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

fn is_boolean(expr: &Expr) -> bool {
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

fn function_call(name: &str, args: Vec<ast::Expr>) -> ast::Expr {
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

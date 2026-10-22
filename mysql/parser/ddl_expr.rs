use crate::names::{quote_string, translate_identifier};
use crate::MysqlParserError;
use sqlparser::ast::{
    BinaryOperator, Expr, Function, FunctionArg, FunctionArgExpr, FunctionArgumentList,
    FunctionArguments, ObjectNamePart, UnaryOperator, Value,
};
use turso_parser::ast;

pub(crate) enum DefaultValue {
    Null,
    Number(String),
    Text(String),
    CurrentTimestamp,
    Expression(ast::Expr),
}

pub(crate) fn translate_default(expr: &Expr) -> Result<DefaultValue, MysqlParserError> {
    if let Some(literal) = current_time_literal(expr)? {
        return match literal {
            ast::Literal::CurrentTimestamp => Ok(DefaultValue::CurrentTimestamp),
            _ => Err(unsupported_default(expr)),
        };
    }
    match expr {
        Expr::Nested(inner) => Ok(DefaultValue::Expression(ast::Expr::Parenthesized(vec![
            Box::new(translate_expr(inner)?),
        ]))),
        Expr::Value(value) => match &value.value {
            Value::Null => Ok(DefaultValue::Null),
            Value::Number(number, _) => Ok(DefaultValue::Number(number.clone())),
            Value::Boolean(true) => Ok(DefaultValue::Number("1".to_string())),
            Value::Boolean(false) => Ok(DefaultValue::Number("0".to_string())),
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
                Ok(DefaultValue::Text(text.clone()))
            }
            _ => Err(unsupported_default(expr)),
        },
        Expr::UnaryOp { op, expr: operand } => {
            let Expr::Value(value) = operand.as_ref() else {
                return Err(unsupported_default(expr));
            };
            let Value::Number(number, _) = &value.value else {
                return Err(unsupported_default(expr));
            };
            match op {
                UnaryOperator::Minus => Ok(DefaultValue::Number(format!("-{number}"))),
                UnaryOperator::Plus => Ok(DefaultValue::Number(number.clone())),
                _ => Err(unsupported_default(expr)),
            }
        }
        _ => Err(unsupported_default(expr)),
    }
}

pub(crate) fn translate_expr(expr: &Expr) -> Result<ast::Expr, MysqlParserError> {
    match expr {
        Expr::Identifier(identifier) => Ok(ast::Expr::Id(translate_identifier(identifier)?)),
        Expr::Value(value) => translate_value(&value.value),
        Expr::Nested(inner) => Ok(ast::Expr::Parenthesized(vec![Box::new(translate_expr(
            inner,
        )?)])),
        Expr::UnaryOp { op, expr: operand } => {
            let op = match op {
                UnaryOperator::Minus => ast::UnaryOperator::Negative,
                UnaryOperator::Plus => ast::UnaryOperator::Positive,
                UnaryOperator::Not => ast::UnaryOperator::Not,
                _ => return Err(unsupported_expr(expr)),
            };
            Ok(ast::Expr::Unary(op, Box::new(translate_expr(operand)?)))
        }
        Expr::BinaryOp { left, op, right } => Ok(ast::Expr::Binary(
            Box::new(translate_expr(left)?),
            translate_binary_operator(op).ok_or_else(|| unsupported_expr(expr))?,
            Box::new(translate_expr(right)?),
        )),
        Expr::IsNull(operand) => Ok(ast::Expr::IsNull(Box::new(translate_expr(operand)?))),
        Expr::IsNotNull(operand) => Ok(ast::Expr::NotNull(Box::new(translate_expr(operand)?))),
        Expr::InList {
            expr: lhs,
            list,
            negated,
        } => Ok(ast::Expr::InList {
            lhs: Box::new(translate_expr(lhs)?),
            not: *negated,
            rhs: list
                .iter()
                .map(|item| translate_expr(item).map(Box::new))
                .collect::<Result<_, _>>()?,
        }),
        Expr::Between {
            expr: lhs,
            negated,
            low,
            high,
        } => Ok(ast::Expr::Between {
            lhs: Box::new(translate_expr(lhs)?),
            not: *negated,
            start: Box::new(translate_expr(low)?),
            end: Box::new(translate_expr(high)?),
        }),
        Expr::Function(function) => match current_time_literal(expr)? {
            Some(literal) => Ok(ast::Expr::Literal(literal)),
            None => translate_function(function),
        },
        _ => Err(unsupported_expr(expr)),
    }
}

fn translate_value(value: &Value) -> Result<ast::Expr, MysqlParserError> {
    let literal = match value {
        Value::Number(number, _) => ast::Literal::Numeric(translate_number(number)?),
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
            ast::Literal::String(quote_string(text))
        }
        Value::Boolean(true) => ast::Literal::Numeric("1".to_string()),
        Value::Boolean(false) => ast::Literal::Numeric("0".to_string()),
        Value::Null => ast::Literal::Null,
        other => {
            return Err(MysqlParserError::Unsupported(format!(
                "literal `{other}` is not available in DDL expressions"
            )));
        }
    };
    Ok(ast::Expr::Literal(literal))
}

fn translate_number(number: &str) -> Result<String, MysqlParserError> {
    let is_plain_decimal = |text: &str| {
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        !(whole.is_empty() && fraction.is_empty())
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
    };
    if number.parse::<i64>().is_ok() || (number.contains('.') && is_plain_decimal(number)) {
        return Ok(number.to_string());
    }
    Err(MysqlParserError::Unsupported(format!(
        "numeric literal `{number}` needs exact MySQL decimal, unsigned or double semantics"
    )))
}

fn translate_binary_operator(op: &BinaryOperator) -> Option<ast::Operator> {
    Some(match op {
        BinaryOperator::Plus => ast::Operator::Add,
        BinaryOperator::Minus => ast::Operator::Subtract,
        BinaryOperator::Multiply => ast::Operator::Multiply,
        BinaryOperator::Gt => ast::Operator::Greater,
        BinaryOperator::Lt => ast::Operator::Less,
        BinaryOperator::GtEq => ast::Operator::GreaterEquals,
        BinaryOperator::LtEq => ast::Operator::LessEquals,
        BinaryOperator::Eq => ast::Operator::Equals,
        BinaryOperator::NotEq => ast::Operator::NotEquals,
        BinaryOperator::Spaceship => ast::Operator::Is,
        BinaryOperator::And => ast::Operator::And,
        BinaryOperator::Or => ast::Operator::Or,
        _ => return None,
    })
}

fn translate_function(function: &Function) -> Result<ast::Expr, MysqlParserError> {
    let Function {
        name,
        uses_odbc_syntax,
        parameters,
        args,
        filter,
        null_treatment,
        over,
        within_group,
    } = function;
    let [ObjectNamePart::Identifier(identifier)] = name.0.as_slice() else {
        return Err(unsupported_function(function));
    };
    if *uses_odbc_syntax
        || !matches!(parameters, FunctionArguments::None)
        || filter.is_some()
        || null_treatment.is_some()
        || over.is_some()
        || !within_group.is_empty()
        || identifier.quote_style.is_some()
    {
        return Err(unsupported_function(function));
    }
    let FunctionArguments::List(FunctionArgumentList {
        duplicate_treatment: None,
        args,
        clauses,
    }) = args
    else {
        return Err(unsupported_function(function));
    };
    if !clauses.is_empty() {
        return Err(unsupported_function(function));
    }
    let args = args
        .iter()
        .map(|arg| match arg {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(expr)) => translate_expr(expr).map(Box::new),
            _ => Err(unsupported_function(function)),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ast::Expr::FunctionCall {
        name: ast::Name::exact(identifier.value.clone()),
        distinctness: None,
        args,
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    })
}

fn current_time_literal(expr: &Expr) -> Result<Option<ast::Literal>, MysqlParserError> {
    let Expr::Function(function) = expr else {
        return Ok(None);
    };
    let [ObjectNamePart::Identifier(identifier)] = function.name.0.as_slice() else {
        return Ok(None);
    };
    let literal = match identifier.value.to_ascii_uppercase().as_str() {
        "CURRENT_TIMESTAMP" | "NOW" | "LOCALTIME" | "LOCALTIMESTAMP" => {
            ast::Literal::CurrentTimestamp
        }
        "CURRENT_DATE" | "CURDATE" => ast::Literal::CurrentDate,
        "CURRENT_TIME" | "CURTIME" => ast::Literal::CurrentTime,
        _ => return Ok(None),
    };
    let without_precision = match &function.args {
        FunctionArguments::None => true,
        FunctionArguments::List(list) => {
            list.args.is_empty()
                || matches!(
                    list.args.as_slice(),
                    [FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(value)))]
                        if matches!(&value.value, Value::Number(number, _) if number == "0")
                )
        }
        FunctionArguments::Subquery(_) => false,
    };
    if !without_precision || identifier.quote_style.is_some() {
        return Err(MysqlParserError::Unsupported(format!(
            "`{expr}` with fractional seconds is not available"
        )));
    }
    Ok(Some(literal))
}

fn unsupported_default(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "DEFAULT `{expr}` must be a literal, CURRENT_TIMESTAMP or a parenthesized expression"
    ))
}

fn unsupported_function(function: &Function) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "function call `{function}` is not available in DDL expressions"
    ))
}

fn unsupported_expr(expr: &Expr) -> MysqlParserError {
    MysqlParserError::Unsupported(format!(
        "expression `{expr}` is not available in DDL expressions"
    ))
}

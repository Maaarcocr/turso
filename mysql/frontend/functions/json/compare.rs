use std::cmp::Ordering;

use turso_core::dialect::ArgumentType;
use turso_core::{LimboError, Numeric, Result, Value};

use super::super::decimal::shown_decimal;
use super::super::string::text_to_double;
use super::document::{Json, Object};
use super::parse::parse;
use super::{boolean_result, is_json};
use crate::types::decimal_number::Decimal;

pub(super) fn execute_typed(
    name: &str,
    args: &[Value],
    arg_types: &[ArgumentType],
) -> Option<Result<Value>> {
    let function: fn(&[Operand]) -> Value = match (name, args.len()) {
        ("__MYSQL_JSON_COMPARE", 2) => compare_operands,
        ("__MYSQL_JSON_NULL_SAFE_EQUAL", 2) => return Some(null_safe_equal(args, arg_types)),
        ("__MYSQL_JSON_TRUTH", 1) => truth,
        ("__MYSQL_JSON_DOUBLE", 1) => double,
        ("__MYSQL_JSON_REMAINDER", 2) => remainder,
        _ => return None,
    };
    let mut operands = Vec::with_capacity(args.len());
    for (value, arg_type) in args.iter().zip(arg_types) {
        match operand(value, arg_type) {
            Ok(Some(operand)) => operands.push(operand),
            Ok(None) => return Some(Ok(Value::Null)),
            Err(error) => return Some(Err(error)),
        }
    }
    Some(Ok(function(&operands)))
}

pub(super) fn accepts(name: &str, arg_count: usize) -> bool {
    matches!(
        (name, arg_count),
        (
            "__MYSQL_JSON_COMPARE" | "__MYSQL_JSON_NULL_SAFE_EQUAL" | "__MYSQL_JSON_REMAINDER",
            2
        ) | ("__MYSQL_JSON_TRUTH" | "__MYSQL_JSON_DOUBLE", 1)
    )
}

enum Operand {
    Json(Json),
    Decimal(Decimal),
    Temporal(Temporal),
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TemporalKind {
    Date,
    Time,
    DateTime,
}

struct Temporal {
    kind: TemporalKind,
    micros: i128,
}

fn operand(value: &Value, arg_type: &ArgumentType) -> Result<Option<Operand>> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    if let Some(decimal) = shown_decimal(value, arg_type) {
        return Ok(Some(Operand::Decimal(decimal)));
    }
    if let ArgumentType::Named { name, .. } = arg_type {
        let kind = match name.to_ascii_lowercase().as_str() {
            "date" => TemporalKind::Date,
            "time" => TemporalKind::Time,
            "datetime" | "timestamp" => TemporalKind::DateTime,
            _ => {
                return Err(LimboError::InternalError(format!(
                    "a `{name}` value has no JSON comparison rule"
                )))
            }
        };
        let Value::Text(text) = value else {
            return Err(LimboError::InternalError(format!(
                "a `{name}` value is not text"
            )));
        };
        return temporal(kind, text.as_str())
            .map(|temporal| Some(Operand::Temporal(temporal)))
            .ok_or_else(|| {
                LimboError::InternalError(format!("`{}` is not a `{name}` value", text.as_str()))
            });
    }
    Ok(Some(Operand::Json(match value {
        Value::Null => return Ok(None),
        Value::Numeric(Numeric::Integer(integer)) => Json::Integer(*integer),
        Value::Numeric(Numeric::Float(double)) => Json::Double(f64::from(*double)),
        Value::Text(text) if is_json(text) => parse(text.as_str()).map_err(|error| {
            LimboError::InternalError(format!("a JSON value does not parse: {error:?}"))
        })?,
        Value::Text(text) => Json::String(text.as_str().to_string()),
        Value::Blob(bytes) => Json::Opaque(bytes.to_vec()),
    })))
}

fn temporal(kind: TemporalKind, text: &str) -> Option<Temporal> {
    let (date, time) = match kind {
        TemporalKind::Date => (Some(text), None),
        TemporalKind::Time => (None, Some(text)),
        TemporalKind::DateTime => {
            let (date, time) = text.split_once(' ')?;
            (Some(date), Some(time))
        }
    };
    let days = match date {
        Some(date) => {
            let mut parts = date.splitn(3, '-').map(str::parse::<i128>);
            let (Some(Ok(year)), Some(Ok(month)), Some(Ok(day))) =
                (parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            (year * 13 + month) * 32 + day
        }
        None => 0,
    };
    let micros = match time {
        Some(time) => {
            let (negative, time) = match time.strip_prefix('-') {
                Some(rest) => (true, rest),
                None => (false, time),
            };
            let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
            let mut parts = clock.splitn(3, ':').map(str::parse::<i128>);
            let (Some(Ok(hours)), Some(Ok(minutes)), Some(Ok(seconds))) =
                (parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            let fraction: i128 = format!("{fraction:0<6}").parse().ok()?;
            let magnitude = ((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + fraction;
            if negative {
                -magnitude
            } else {
                magnitude
            }
        }
        None => 0,
    };
    Some(Temporal {
        kind,
        micros: days * 86_400_000_000 + micros,
    })
}

fn compare_operands(operands: &[Operand]) -> Value {
    Value::from_i64(match compare(&operands[0], &operands[1]) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    })
}

fn null_safe_equal(args: &[Value], arg_types: &[ArgumentType]) -> Result<Value> {
    let left = operand(&args[0], &arg_types[0])?;
    let right = operand(&args[1], &arg_types[1])?;
    Ok(boolean_result(match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => compare(&left, &right) == Ordering::Equal,
        _ => false,
    }))
}

fn truth(operands: &[Operand]) -> Value {
    boolean_result(compare(&operands[0], &Operand::Json(Json::Integer(0))) != Ordering::Equal)
}

fn double(operands: &[Operand]) -> Value {
    Value::from_f64(to_double(&operands[0]))
}

fn remainder(operands: &[Operand]) -> Value {
    let divisor = to_double(&operands[1]);
    if divisor == 0.0 {
        return Value::Null;
    }
    Value::from_f64(to_double(&operands[0]) % divisor)
}

fn to_double(operand: &Operand) -> f64 {
    match operand {
        Operand::Decimal(decimal) => decimal.to_f64(),
        Operand::Temporal(_) => 0.0,
        Operand::Json(json) => match json {
            Json::Integer(integer) => *integer as f64,
            Json::Unsigned(unsigned) => *unsigned as f64,
            Json::Double(double) => *double,
            Json::String(text) => text_to_double(text.as_bytes()),
            Json::Boolean(value) => f64::from(u8::from(*value)),
            Json::Null | Json::Opaque(_) | Json::Array(_) | Json::Object(_) => 0.0,
        },
    }
}

enum Number<'a> {
    Integer(i64),
    Unsigned(u64),
    Double(f64),
    Decimal(&'a Decimal),
}

impl Number<'_> {
    fn to_f64(&self) -> f64 {
        match self {
            Number::Integer(integer) => *integer as f64,
            Number::Unsigned(unsigned) => *unsigned as f64,
            Number::Double(double) => *double,
            Number::Decimal(decimal) => decimal.to_f64(),
        }
    }

    fn exact(&self) -> Decimal {
        match self {
            Number::Integer(integer) => Decimal::from_i64(*integer),
            Number::Unsigned(unsigned) => {
                Decimal::parse(&unsigned.to_string()).expect("an unsigned integer is a decimal")
            }
            Number::Double(double) => {
                Decimal::from_double(*double).expect("JSON doubles are finite")
            }
            Number::Decimal(decimal) => (*decimal).clone(),
        }
    }
}

fn compare(left: &Operand, right: &Operand) -> Ordering {
    match (left, right) {
        (Operand::Json(left), Operand::Json(right)) => compare_json(left, right),
        (Operand::Decimal(left), Operand::Decimal(right)) => left.compare(right),
        (Operand::Decimal(decimal), Operand::Json(json)) => {
            compare_with_number(Number::Decimal(decimal), json)
        }
        (Operand::Json(json), Operand::Decimal(decimal)) => {
            compare_with_number(Number::Decimal(decimal), json).reverse()
        }
        (Operand::Temporal(left), Operand::Temporal(right)) => left
            .kind
            .cmp(&right.kind)
            .then_with(|| left.micros.cmp(&right.micros)),
        (Operand::Temporal(temporal), other) => temporal_rank(temporal).cmp(&operand_rank(other)),
        (other, Operand::Temporal(temporal)) => operand_rank(other).cmp(&temporal_rank(temporal)),
    }
}

fn temporal_rank(temporal: &Temporal) -> u8 {
    match temporal.kind {
        TemporalKind::Date => 6,
        TemporalKind::Time => 7,
        TemporalKind::DateTime => 8,
    }
}

fn operand_rank(operand: &Operand) -> u8 {
    match operand {
        Operand::Json(json) => rank(json),
        Operand::Decimal(_) => NUMBER_RANK,
        Operand::Temporal(temporal) => temporal_rank(temporal),
    }
}

fn compare_with_number(number: Number, json: &Json) -> Ordering {
    match json_number(json) {
        Some(other) => compare_numbers(&number, &other),
        None => NUMBER_RANK.cmp(&rank(json)),
    }
}

const NUMBER_RANK: u8 = 1;

fn rank(json: &Json) -> u8 {
    match json {
        Json::Null => 0,
        Json::Integer(_) | Json::Unsigned(_) | Json::Double(_) => NUMBER_RANK,
        Json::String(_) => 2,
        Json::Object(_) => 3,
        Json::Array(_) => 4,
        Json::Boolean(_) => 5,
        Json::Opaque(_) => 9,
    }
}

fn json_number(json: &Json) -> Option<Number<'static>> {
    match json {
        Json::Integer(integer) => Some(Number::Integer(*integer)),
        Json::Unsigned(unsigned) => Some(Number::Unsigned(*unsigned)),
        Json::Double(double) => Some(Number::Double(*double)),
        _ => None,
    }
}

fn compare_json(left: &Json, right: &Json) -> Ordering {
    if let (Some(left), Some(right)) = (json_number(left), json_number(right)) {
        return compare_numbers(&left, &right);
    }
    match (left, right) {
        (Json::Null, Json::Null) => Ordering::Equal,
        (Json::String(left), Json::String(right)) => left.as_bytes().cmp(right.as_bytes()),
        (Json::Boolean(left), Json::Boolean(right)) => left.cmp(right),
        (Json::Opaque(left), Json::Opaque(right)) => left.cmp(right),
        (Json::Array(left), Json::Array(right)) => left
            .iter()
            .zip(right)
            .map(|(left, right)| compare_json(left, right))
            .find(|ordering| ordering.is_ne())
            .unwrap_or_else(|| left.len().cmp(&right.len())),
        (Json::Object(left), Json::Object(right)) => compare_objects(left, right),
        _ => rank(left).cmp(&rank(right)),
    }
}

fn compare_objects(left: &Object, right: &Object) -> Ordering {
    left.len().cmp(&right.len()).then_with(|| {
        left.iter()
            .zip(right.iter())
            .map(|((left_key, left_value), (right_key, right_value))| {
                left_key
                    .as_bytes()
                    .cmp(right_key.as_bytes())
                    .then_with(|| compare_json(left_value, right_value))
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    })
}

fn compare_numbers(left: &Number, right: &Number) -> Ordering {
    match (left, right) {
        (Number::Double(left), Number::Double(right)) => {
            left.partial_cmp(right).expect("JSON doubles are not NaN")
        }
        (Number::Double(_), _) | (_, Number::Double(_)) => left
            .to_f64()
            .partial_cmp(&right.to_f64())
            .expect("JSON numbers are not NaN")
            .then_with(|| left.exact().compare(&right.exact())),
        _ => left.exact().compare(&right.exact()),
    }
}

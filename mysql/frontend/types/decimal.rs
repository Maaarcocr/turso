use super::decimal_number::{Decimal, Rounding};
use super::number_text::{self, Reading};
use super::{binary_string_in_number_column, integer_parameter, invalid_value, out_of_range};
use std::cmp::Ordering;
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

const LARGEST_PRECISION: i64 = 65;
const LARGEST_SCALE: i64 = 30;
const DIVISION_SCALE_INCREMENT: i64 = 4;
const INTEGER_PRECISION: i64 = 19;
const SUM_EXTRA_DIGITS: i64 = 22;
const LARGEST_PLACES: i64 = 1000;

macro_rules! arithmetic {
    ($left:literal, $right:literal, $slots:literal) => {
        concat!(
            "OPERATOR '+' (",
            $left,
            ", ",
            $right,
            ") RETURNS decimal __mysql_decimal_add_",
            $slots,
            " ",
            "OPERATOR '-' (",
            $left,
            ", ",
            $right,
            ") RETURNS decimal __mysql_decimal_subtract_",
            $slots,
            " ",
            "OPERATOR '*' (",
            $left,
            ", ",
            $right,
            ") RETURNS decimal __mysql_decimal_multiply_",
            $slots,
            " ",
            "OPERATOR '/' (",
            $left,
            ", ",
            $right,
            ") RETURNS decimal __mysql_decimal_divide_",
            $slots,
            " ",
            "OPERATOR '%' (",
            $left,
            ", ",
            $right,
            ") RETURNS decimal __mysql_decimal_remainder_",
            $slots,
            " ",
        )
    };
}

macro_rules! comparisons {
    ($left:literal, $right:literal, $less:literal, $equal:literal) => {
        concat!(
            "OPERATOR '<' (",
            $left,
            ", ",
            $right,
            ") ",
            $less,
            " ",
            "OPERATOR '=' (",
            $left,
            ", ",
            $right,
            ") ",
            $equal,
            " ",
        )
    };
}

macro_rules! functions {
    ($quoted:literal) => {
        concat!(
            "FUNCTION round(",
            $quoted,
            ") RETURNS decimal __mysql_decimal_round ",
            "FUNCTION round(",
            $quoted,
            ", integer) RETURNS decimal __mysql_decimal_round ",
            "FUNCTION truncate(",
            $quoted,
            ", integer) RETURNS decimal __mysql_decimal_truncate ",
            "FUNCTION abs(",
            $quoted,
            ") RETURNS decimal __mysql_decimal_abs ",
            "FUNCTION floor(",
            $quoted,
            ") RETURNS decimal __mysql_decimal_floor ",
            "FUNCTION ceil(",
            $quoted,
            ") RETURNS decimal __mysql_decimal_ceiling ",
            "FUNCTION ceiling(",
            $quoted,
            ") RETURNS decimal __mysql_decimal_ceiling ",
            "FUNCTION sign(",
            $quoted,
            ") RETURNS integer __mysql_decimal_sign ",
            "FUNCTION sum(",
            $quoted,
            ") RETURNS decimal numeric_sum ",
        )
    };
}

macro_rules! decimal_type {
    ($name:literal, $quoted:literal, $unsigned:literal, $($extra:expr),*) => {
        DialectType {
            name: $name,
            storage: StorageType::Numeric,
            parameters: "",
            behaviour: concat!(
                "CAST FROM integer AS IMPLICIT CAST TO real AS IMPLICIT ",
                "ASSIGN __mysql_assign_decimal(value, precision, scale, ", $unsigned, ") ",
                "CAST __mysql_cast_decimal(value, precision, scale) ",
                "OPERATOR '-' (", $quoted, ") RETURNS decimal __mysql_decimal_negate ",
                arithmetic!($quoted, $quoted, "dd"),
                arithmetic!($quoted, "integer", "dn"),
                arithmetic!("integer", $quoted, "nd"),
                comparisons!($quoted, $quoted, "numeric_lt", "numeric_eq"),
                comparisons!($quoted, "integer", "__mysql_decimal_less_dn", "__mysql_decimal_equal_dn"),
                comparisons!("integer", $quoted, "__mysql_decimal_less_nd", "__mysql_decimal_equal_nd"),
                functions!($quoted),
                $($extra),*
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 2] = [
    decimal_type!("decimal", "decimal", 0, ""),
    decimal_type!(
        "decimal unsigned",
        "\"decimal unsigned\"",
        1,
        arithmetic!("\"decimal unsigned\"", "decimal", "dd"),
        arithmetic!("decimal", "\"decimal unsigned\"", "dd"),
        comparisons!(
            "\"decimal unsigned\"",
            "decimal",
            "numeric_lt",
            "numeric_eq"
        ),
        comparisons!(
            "decimal",
            "\"decimal unsigned\"",
            "numeric_lt",
            "numeric_eq"
        )
    ),
];

/// The precision and scale of `+ - * / %` and SUM results, as MySQL
/// computes them from the operands'. An operand without parameters is an
/// integer.
pub(super) fn result_parameters(operator: &str, operands: [Option<&[i64]>; 2]) -> Option<Vec<i64>> {
    let [(left_precision, left_scale), (right_precision, right_scale)] =
        operands.map(|parameters| {
            let parameters = parameters.unwrap_or_default();
            (
                parameters.first().copied().unwrap_or(INTEGER_PRECISION),
                parameters.get(1).copied().unwrap_or(0),
            )
        });
    let whole = |precision: i64, scale: i64| (precision - scale).max(0);
    let (whole_digits, scale) = match operator {
        "+" | "-" => (
            whole(left_precision, left_scale).max(whole(right_precision, right_scale)) + 1,
            left_scale.max(right_scale),
        ),
        "*" => (
            whole(left_precision, left_scale) + whole(right_precision, right_scale),
            (left_scale + right_scale).min(LARGEST_SCALE),
        ),
        "/" => (
            whole(left_precision, left_scale) + right_scale,
            (left_scale + DIVISION_SCALE_INCREMENT).min(LARGEST_SCALE),
        ),
        "%" => (
            whole(left_precision, left_scale).max(whole(right_precision, right_scale)),
            left_scale.max(right_scale),
        ),
        "sum" => (
            whole(left_precision, left_scale) + SUM_EXTRA_DIGITS,
            left_scale,
        ),
        _ => return None,
    };
    let precision = (whole_digits + scale).clamp(scale.max(1), LARGEST_PRECISION);
    Some(vec![precision, scale])
}

pub(super) type Function = fn(&[Value]) -> Result<Value>;

pub(super) fn function(name: &str, arg_count: usize) -> Option<Function> {
    let function: Function = match (name, arg_count) {
        ("__MYSQL_ASSIGN_DECIMAL", 4) => assign_decimal,
        ("__MYSQL_CAST_DECIMAL", 3) => cast_decimal,
        ("__MYSQL_DECIMAL_NEGATE", 1) => negate,
        ("__MYSQL_DIVIDE_NN", 2) => |args| arithmetic(args, [Slot::Number; 2], Operation::Divide),
        ("__MYSQL_DECIMAL_ROUND", 1 | 2) => |args| rounded(args, Rounding::HalfAwayFromZero),
        ("__MYSQL_DECIMAL_TRUNCATE", 2) => |args| rounded(args, Rounding::TowardZero),
        ("__MYSQL_DECIMAL_ABS", 1) => {
            |args| unary(args, |decimal| decimal_value(&decimal.abs()), f64::abs)
        }
        ("__MYSQL_DECIMAL_FLOOR", 1) => |args| whole(args, Rounding::Floor),
        ("__MYSQL_DECIMAL_CEILING", 1) => |args| whole(args, Rounding::Ceiling),
        ("__MYSQL_DECIMAL_SIGN", 1) => |args| {
            unary(
                args,
                |decimal| Value::from_i64(decimal.signum()),
                f64::signum,
            )
        },
        _ => return operator(name.strip_prefix("__MYSQL_DECIMAL_")?, arg_count),
    };
    Some(function)
}

fn operator(name: &str, arg_count: usize) -> Option<Function> {
    if arg_count != 2 {
        return None;
    }
    let function: Function = match name {
        "ADD_DD" => |args| arithmetic(args, [Slot::Decimal; 2], Operation::Add),
        "ADD_DN" => |args| arithmetic(args, DECIMAL_NUMBER, Operation::Add),
        "ADD_ND" => |args| arithmetic(args, NUMBER_DECIMAL, Operation::Add),
        "SUBTRACT_DD" => |args| arithmetic(args, [Slot::Decimal; 2], Operation::Subtract),
        "SUBTRACT_DN" => |args| arithmetic(args, DECIMAL_NUMBER, Operation::Subtract),
        "SUBTRACT_ND" => |args| arithmetic(args, NUMBER_DECIMAL, Operation::Subtract),
        "MULTIPLY_DD" => |args| arithmetic(args, [Slot::Decimal; 2], Operation::Multiply),
        "MULTIPLY_DN" => |args| arithmetic(args, DECIMAL_NUMBER, Operation::Multiply),
        "MULTIPLY_ND" => |args| arithmetic(args, NUMBER_DECIMAL, Operation::Multiply),
        "DIVIDE_DD" => |args| arithmetic(args, [Slot::Decimal; 2], Operation::Divide),
        "DIVIDE_DN" => |args| arithmetic(args, DECIMAL_NUMBER, Operation::Divide),
        "DIVIDE_ND" => |args| arithmetic(args, NUMBER_DECIMAL, Operation::Divide),
        "REMAINDER_DD" => |args| arithmetic(args, [Slot::Decimal; 2], Operation::Remainder),
        "REMAINDER_DN" => |args| arithmetic(args, DECIMAL_NUMBER, Operation::Remainder),
        "REMAINDER_ND" => |args| arithmetic(args, NUMBER_DECIMAL, Operation::Remainder),
        "LESS_DN" => |args| comparison(args, DECIMAL_NUMBER, Ordering::is_lt),
        "LESS_ND" => |args| comparison(args, NUMBER_DECIMAL, Ordering::is_lt),
        "EQUAL_DN" => |args| comparison(args, DECIMAL_NUMBER, Ordering::is_eq),
        "EQUAL_ND" => |args| comparison(args, NUMBER_DECIMAL, Ordering::is_eq),
        _ => return None,
    };
    Some(function)
}

/// Stores a value in a `DECIMAL(precision, scale)` column as MySQL's strict
/// mode does: text must be a whole number, the value is rounded half away
/// from zero to the scale, and too many whole digits are out of range.
pub(super) fn assign_decimal(args: &[Value]) -> Result<Value> {
    let [value, precision, scale, unsigned] = args else {
        unreachable!("__mysql_assign_decimal is resolved with four arguments");
    };
    let decimal = match value {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => Decimal::from_i64(*number),
        Value::Numeric(Numeric::Float(double)) => {
            Decimal::from_double(f64::from(*double)).ok_or_else(out_of_range)?
        }
        Value::Text(text) => read_decimal(text.as_str().as_bytes())?,
        Value::Blob(_) => return Err(binary_string_in_number_column("decimal")),
    };
    if integer_parameter(unsigned) == Some(1) && decimal.is_negative() {
        return Err(out_of_range());
    }
    let Some((precision, scale)) = precision_and_scale(precision, scale) else {
        return Ok(decimal_value(&decimal));
    };
    let rounded = decimal.rounded(i64::from(scale), Rounding::HalfAwayFromZero);
    if rounded.whole_digits() > precision - scale {
        return Err(out_of_range());
    }
    Ok(decimal_value(&rounded))
}

fn read_decimal(bytes: &[u8]) -> Result<Decimal> {
    match number_text::read_as_double(bytes) {
        Reading::Number(number) => Decimal::from_text_number(&number).ok_or_else(out_of_range),
        Reading::NumberFollowedByText | Reading::NoNumber => Err(invalid_value(format!(
            "Incorrect decimal value: '{}'",
            String::from_utf8_lossy(bytes)
        ))),
    }
}

/// `CAST(value AS DECIMAL(precision, scale))`: text is read up to the first
/// character that is not part of a number, the value is rounded half away
/// from zero, and a value with too many whole digits becomes the largest
/// value of the type.
pub(super) fn cast_decimal(args: &[Value]) -> Result<Value> {
    let [value, precision, scale] = args else {
        unreachable!("__mysql_cast_decimal is resolved with three arguments");
    };
    let read = match value {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => Ok(Decimal::from_i64(*number)),
        Value::Numeric(Numeric::Float(double)) => {
            Ok(Decimal::from_double(f64::from(*double)).expect("stored doubles are finite"))
        }
        Value::Text(text) => leading_decimal(text.as_str().as_bytes()),
        Value::Blob(bytes) => leading_decimal(bytes),
    };
    let Some((precision, scale)) = precision_and_scale(precision, scale) else {
        let largest = Decimal::largest(LARGEST_PRECISION as u32, 0);
        return Ok(decimal_value(&match read {
            Ok(decimal) => decimal,
            Err(true) => largest.negated(),
            Err(false) => largest,
        }));
    };
    let largest = Decimal::largest(precision, scale);
    let clamped = match read {
        Ok(decimal) => {
            let rounded = decimal.rounded(i64::from(scale), Rounding::HalfAwayFromZero);
            if rounded.whole_digits() <= precision - scale {
                rounded
            } else if rounded.is_negative() {
                largest.negated()
            } else {
                largest
            }
        }
        Err(true) => largest.negated(),
        Err(false) => largest,
    };
    Ok(decimal_value(&clamped))
}

/// The number at the start of `bytes`; zero when there is none. An error
/// tells whether a number too large to read is negative.
fn leading_decimal(bytes: &[u8]) -> std::result::Result<Decimal, bool> {
    match number_text::leading_number(bytes) {
        Some(number) => Decimal::from_text_number(&number).ok_or_else(|| number.is_negative()),
        None => Ok(Decimal::zero()),
    }
}

fn precision_and_scale(precision: &Value, scale: &Value) -> Option<(u32, u32)> {
    let precision = u32::try_from(integer_parameter(precision)?).ok()?;
    let scale = u32::try_from(integer_parameter(scale)?).ok()?;
    Some((precision, scale.min(precision)))
}

fn negate(args: &[Value]) -> Result<Value> {
    unary(args, negated, |double| -double)
}

/// `-x` of a DECIMAL. The lowering writes `-x` as `x * -1`, so this runs
/// only in statements it does not lower, such as INSERT and UPDATE, where a
/// decimal literal was a double before DECIMAL had a type. A negated value
/// that a double holds exactly stays a double there, so a negative literal
/// is stored as before (`-0.4` into an UNSIGNED column is out of range).
fn negated(decimal: &Decimal) -> Value {
    let negated = decimal.negated();
    let double = negated.to_f64();
    match Decimal::from_double(double) {
        Some(read_back) if read_back.compare(&negated).is_eq() => Value::from_f64(double),
        _ => decimal_value(&negated),
    }
}

fn unary(args: &[Value], exact: fn(&Decimal) -> Value, double: fn(f64) -> f64) -> Result<Value> {
    Ok(match operand(&args[0], Slot::Decimal) {
        None => Value::Null,
        Some(Operand::Exact(decimal)) => exact(&decimal),
        Some(Operand::Double(value)) => Value::from_f64(double(value)),
    })
}

/// `ROUND(x, places)` and `TRUNCATE(x, places)` of a DECIMAL. The result
/// keeps at most the scale of `x`, so `ROUND(1.5, 5)` is `1.5`.
fn rounded(args: &[Value], rounding: Rounding) -> Result<Value> {
    let places = match args.get(1) {
        None => 0,
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Numeric(Numeric::Integer(places))) => *places,
        Some(Value::Numeric(Numeric::Float(places))) => f64::from(*places).round_ties_even() as i64,
        Some(Value::Text(text)) => leading_integer(text.as_str().as_bytes()),
        Some(Value::Blob(bytes)) => leading_integer(bytes),
    }
    .clamp(-LARGEST_PLACES, LARGEST_PLACES);
    Ok(match operand(&args[0], Slot::Decimal) {
        None => Value::Null,
        Some(Operand::Exact(decimal)) => {
            decimal_value(&decimal.rounded_within_scale(places, rounding))
        }
        Some(Operand::Double(value)) => {
            let factor = 10f64.powi(places.clamp(-400, 400) as i32);
            let scaled = value * factor;
            let rounded = match rounding {
                Rounding::TowardZero => scaled.trunc(),
                _ => scaled.round_ties_even(),
            };
            Value::from_f64(if factor.is_finite() && scaled.is_finite() {
                rounded / factor
            } else {
                value
            })
        }
    })
}

/// The integer at the start of text, as MySQL reads a string where it needs
/// an integer: `'1.5'` is 1.
fn leading_integer(bytes: &[u8]) -> i64 {
    let Some(number) = number_text::leading_number(bytes) else {
        return 0;
    };
    let (negative, whole, _, _) = number.parts();
    let magnitude = whole.iter().fold(0_i64, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'))
    });
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

/// `FLOOR` and `CEILING` of a DECIMAL: a BIGINT when the result fits, else
/// a DECIMAL without decimals.
fn whole(args: &[Value], rounding: Rounding) -> Result<Value> {
    Ok(match operand(&args[0], Slot::Decimal) {
        None => Value::Null,
        Some(Operand::Exact(decimal)) => {
            let whole = decimal.rounded(0, rounding);
            whole
                .to_i64()
                .map_or_else(|| decimal_value(&whole), Value::from_i64)
        }
        Some(Operand::Double(value)) => Value::from_f64(match rounding {
            Rounding::Ceiling => value.ceil(),
            _ => value.floor(),
        }),
    })
}

/// Where an operator receives a value: a `Decimal` slot holds a typed
/// decimal, a `Number` slot an integer or a value of unknown type.
#[derive(Clone, Copy)]
enum Slot {
    Decimal,
    Number,
}

const DECIMAL_NUMBER: [Slot; 2] = [Slot::Decimal, Slot::Number];
const NUMBER_DECIMAL: [Slot; 2] = [Slot::Number, Slot::Decimal];

enum Operand {
    Exact(Decimal),
    Double(f64),
}

impl Operand {
    fn to_f64(&self) -> f64 {
        match self {
            Operand::Exact(decimal) => decimal.to_f64(),
            Operand::Double(double) => *double,
        }
    }
}

/// Reads an operand as MySQL computes it: integers and decimals exactly,
/// and doubles and strings (text of unknown type) as DOUBLE.
fn operand(value: &Value, slot: Slot) -> Option<Operand> {
    Some(match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(number)) => Operand::Exact(Decimal::from_i64(*number)),
        Value::Numeric(Numeric::Float(double)) => Operand::Double(f64::from(*double)),
        Value::Text(text) => match slot {
            Slot::Decimal => match Decimal::parse(text.as_str()) {
                Some(decimal) => Operand::Exact(decimal),
                None => Operand::Double(text_to_double(text.as_str().as_bytes())),
            },
            Slot::Number => Operand::Double(text_to_double(text.as_str().as_bytes())),
        },
        Value::Blob(bytes) => Operand::Double(text_to_double(bytes)),
    })
}

fn text_to_double(bytes: &[u8]) -> f64 {
    number_text::leading_number(bytes)
        .map_or(0.0, |number| number.to_f64())
        .clamp(f64::MIN, f64::MAX)
}

#[derive(Clone, Copy)]
enum Operation {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}

fn arithmetic(args: &[Value], slots: [Slot; 2], operation: Operation) -> Result<Value> {
    let (Some(left), Some(right)) = (operand(&args[0], slots[0]), operand(&args[1], slots[1]))
    else {
        return Ok(Value::Null);
    };
    match (left, right) {
        (Operand::Exact(left), Operand::Exact(right)) => {
            let result = match operation {
                Operation::Add => Some(left.add(&right)),
                Operation::Subtract => Some(left.subtract(&right)),
                Operation::Multiply => Some(left.multiply(&right)),
                Operation::Divide => left.divide(&right),
                Operation::Remainder => left.remainder(&right),
            };
            Ok(result.map_or(Value::Null, |result| decimal_value(&result)))
        }
        (left, right) => {
            let (left, right) = (left.to_f64(), right.to_f64());
            let result = match operation {
                Operation::Add => left + right,
                Operation::Subtract => left - right,
                Operation::Multiply => left * right,
                Operation::Divide | Operation::Remainder if right == 0.0 => return Ok(Value::Null),
                Operation::Divide => left / right,
                Operation::Remainder => left % right,
            };
            if result.is_finite() {
                Ok(Value::from_f64(result))
            } else {
                Err(LimboError::InvalidArgument(
                    "DOUBLE value is out of range".to_string(),
                ))
            }
        }
    }
}

fn comparison(args: &[Value], slots: [Slot; 2], holds: fn(Ordering) -> bool) -> Result<Value> {
    let (Some(left), Some(right)) = (operand(&args[0], slots[0]), operand(&args[1], slots[1]))
    else {
        return Ok(Value::Null);
    };
    let ordering = match (&left, &right) {
        (Operand::Exact(left), Operand::Exact(right)) => Some(left.compare(right)),
        _ => left.to_f64().partial_cmp(&right.to_f64()),
    };
    Ok(match ordering {
        Some(ordering) => Value::from_i64(i64::from(holds(ordering))),
        None => Value::Null,
    })
}

fn decimal_value(decimal: &Decimal) -> Value {
    Value::build_text(decimal.to_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Value {
        Value::build_text(value.to_string())
    }

    fn shown(result: Result<Value>) -> String {
        match result.expect("the conversion succeeds") {
            Value::Text(text) => text.as_str().to_string(),
            other => format!("{other:?}"),
        }
    }

    fn assign(value: Value) -> Result<Value> {
        assign_decimal(&[
            value,
            Value::from_i64(5),
            Value::from_i64(2),
            Value::from_i64(0),
        ])
    }

    fn cast(value: Value, precision: i64, scale: i64) -> String {
        shown(cast_decimal(&[
            value,
            Value::from_i64(precision),
            Value::from_i64(scale),
        ]))
    }

    #[test]
    fn assignment_rounds_to_the_scale_and_checks_whole_digits() {
        assert_eq!(shown(assign(text("1.005"))), "1.01");
        assert_eq!(shown(assign(text(" 1e2 "))), "100.00");
        assert_eq!(shown(assign(Value::from_f64(0.125))), "0.13");
        assert_eq!(shown(assign(text("-0.001"))), "0.00");
        assert!(assign(text("999.995")).is_err());
        assert!(assign(text("1.5abc")).is_err());
        assert!(assign(text("")).is_err());
    }

    #[test]
    fn cast_rounds_and_clamps_to_the_largest_value() {
        assert_eq!(cast(Value::from_i64(1000), 3, 1), "99.9");
        assert_eq!(cast(Value::from_i64(-1000), 3, 1), "-99.9");
        assert_eq!(cast(text("1e400"), 5, 2), "999.99");
        assert_eq!(cast(text("-1e400"), 5, 2), "-999.99");
        assert_eq!(cast(text("1.5abc"), 5, 2), "1.50");
        assert_eq!(cast(text("abc"), 5, 2), "0.00");
        assert_eq!(
            cast(Value::from_f64(0.1), 30, 25),
            "0.1000000000000000000000000"
        );
    }
}

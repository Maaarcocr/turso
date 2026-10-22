use turso_core::{Numeric, Result, Value};

use super::convert::{
    double, double_value, integer, is_space, out_of_range, parse_digits, split_sign, string,
    string_to_double, Integer,
};

#[derive(Clone, Copy)]
enum Rounding {
    Truncate,
    Nearest,
}

#[derive(Clone, Copy)]
enum Number {
    Integer(i64),
    Double(f64),
}

impl Number {
    fn to_double(self) -> f64 {
        match self {
            Number::Integer(value) => value as f64,
            Number::Double(value) => value,
        }
    }
}

pub(super) fn truncate(args: &[Value]) -> Result<Value> {
    round_with(args, Rounding::Truncate)
}

pub(super) fn round(args: &[Value]) -> Result<Value> {
    round_with(args, Rounding::Nearest)
}

fn round_with(args: &[Value], rounding: Rounding) -> Result<Value> {
    let places = match args.get(1) {
        Some(places) => match integer(places) {
            Some(places) => places,
            None => return Ok(Value::Null),
        },
        None => Integer { value: 0 },
    };
    Ok(match number(&args[0]) {
        None => Value::Null,
        Some(Number::Integer(value)) => Value::from_i64(round_signed(value, places, rounding)),
        Some(Number::Double(value)) => Value::from_f64(round_double(value, places, rounding)),
    })
}

fn round_signed(value: i64, places: Integer, rounding: Rounding) -> i64 {
    if !places.is_negative() {
        return value;
    }
    let digits = places.value.unsigned_abs();
    if digits >= 20 {
        return 0;
    }
    let factor = 10u64.pow(digits as u32);
    match rounding {
        Rounding::Truncate if digits >= 19 => 0,
        Rounding::Truncate => value / factor as i64 * factor as i64,
        Rounding::Nearest => {
            let magnitude = value.unsigned_abs();
            let lower = magnitude / factor * factor;
            let rounded = if magnitude - lower < factor / 2 {
                lower
            } else {
                lower.wrapping_add(factor)
            };
            if value >= 0 {
                rounded as i64
            } else {
                (rounded as i64).wrapping_neg()
            }
        }
    }
}

fn round_double(value: f64, places: Integer, rounding: Rounding) -> f64 {
    let negative_places = places.is_negative();
    let digits = if negative_places {
        places.value.unsigned_abs()
    } else {
        places.as_unsigned()
    };
    let factor = power_of_ten(digits);
    let divided = value / factor;
    let multiplied = value * factor;
    if negative_places && factor.is_infinite() {
        return 0.0;
    }
    if !negative_places && !multiplied.is_finite() {
        return value;
    }
    let scale_down = places.value < 0;
    match (rounding, value >= 0.0) {
        (Rounding::Nearest, _) if scale_down => divided.round_ties_even() * factor,
        (Rounding::Nearest, _) => multiplied.round_ties_even() / factor,
        (Rounding::Truncate, true) if scale_down => divided.floor() * factor,
        (Rounding::Truncate, true) => multiplied.floor() / factor,
        (Rounding::Truncate, false) if scale_down => divided.ceil() * factor,
        (Rounding::Truncate, false) => multiplied.ceil() / factor,
    }
}

fn power_of_ten(exponent: u64) -> f64 {
    if exponent > 308 {
        return f64::INFINITY;
    }
    format!("1e{exponent}")
        .parse()
        .expect("a power of ten is valid float syntax")
}

type ReadNumber = fn(&[u8], i32) -> u64;

pub(super) fn conv(args: &[Value]) -> Result<Value> {
    conv_with(args, parse_in_base)
}

pub(super) fn conv_binary_literal(args: &[Value]) -> Result<Value> {
    conv_with(args, |bytes, _| binary_literal_number(bytes))
}

fn conv_with(args: &[Value], read_number: ReadNumber) -> Result<Value> {
    let (Some(from), Some(to)) = (integer(&args[1]), integer(&args[2])) else {
        return Ok(Value::Null);
    };
    Ok(convert_base(
        &args[0],
        from.value as i32,
        to.value as i32,
        read_number,
    ))
}

pub(super) fn bin(args: &[Value]) -> Result<Value> {
    Ok(convert_base(&args[0], 10, 2, parse_in_base))
}

pub(super) fn oct(args: &[Value]) -> Result<Value> {
    Ok(convert_base(&args[0], 10, 8, parse_in_base))
}

fn convert_base(number: &Value, from: i32, to: i32, read_number: ReadNumber) -> Value {
    let is_valid_base = |base: i32| base != i32::MIN && (2..=36).contains(&base.abs());
    let Some(digits) = string(number) else {
        return Value::Null;
    };
    if !is_valid_base(from) || !is_valid_base(to) || digits.len() == 0 {
        return Value::Null;
    }
    Value::build_text(format_in_base(read_number(digits.as_bytes(), from), to))
}

fn parse_in_base(bytes: &[u8], base: i32) -> u64 {
    if base < 0 {
        parse_signed_in_base(bytes, base.unsigned_abs()) as u64
    } else {
        parse_unsigned_in_base(bytes, base.unsigned_abs())
    }
}

fn binary_literal_number(bytes: &[u8]) -> u64 {
    let start = bytes
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(bytes.len());
    let significant = &bytes[start..];
    if significant.len() > 8 {
        return 0;
    }
    significant
        .iter()
        .fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

fn parse_unsigned_in_base(bytes: &[u8], base: u32) -> u64 {
    let (negative, digits) = split_sign(skip_spaces(bytes));
    match parse_digits(digits, base) {
        (_, true) => u64::MAX,
        (magnitude, false) if negative => magnitude.wrapping_neg(),
        (magnitude, false) => magnitude,
    }
}

fn parse_signed_in_base(bytes: &[u8], base: u32) -> i64 {
    let (negative, digits) = split_sign(skip_spaces(bytes));
    let (magnitude, overflow) = parse_digits(digits, base);
    if negative {
        if overflow || magnitude > 1 << 63 {
            i64::MIN
        } else {
            (magnitude as i64).wrapping_neg()
        }
    } else if overflow || magnitude > i64::MAX as u64 {
        i64::MAX
    } else {
        magnitude as i64
    }
}

fn skip_spaces(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|byte| !is_space(*byte))
        .unwrap_or(bytes.len());
    &bytes[start..]
}

fn format_in_base(value: u64, base: i32) -> String {
    let negative = base < 0 && (value as i64) < 0;
    let mut magnitude = if negative {
        (value as i64).unsigned_abs()
    } else {
        value
    };
    let radix = base.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        let digit = (magnitude % u64::from(radix)) as u32;
        digits.push(
            char::from_digit(digit, radix)
                .expect("a remainder is a digit of its radix")
                .to_ascii_uppercase(),
        );
        magnitude /= u64::from(radix);
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

pub(super) fn bit_count(args: &[Value]) -> Result<Value> {
    let bits = match &args[0] {
        Value::Blob(bytes) => bytes.iter().map(|byte| byte.count_ones()).sum(),
        other => match integer(other) {
            Some(value) => value.as_unsigned().count_ones(),
            None => return Ok(Value::Null),
        },
    };
    Ok(Value::from_i64(i64::from(bits)))
}

pub(super) fn abs(args: &[Value]) -> Result<Value> {
    Ok(match number(&args[0]) {
        None => Value::Null,
        Some(Number::Integer(value)) => {
            Value::from_i64(value.checked_abs().ok_or_else(|| out_of_range("BIGINT"))?)
        }
        Some(Number::Double(value)) => Value::from_f64(value.abs()),
    })
}

pub(super) fn sign(args: &[Value]) -> Result<Value> {
    Ok(double(&args[0]).map_or(Value::Null, |value| {
        Value::from_i64(if value > 0.0 {
            1
        } else if value < 0.0 {
            -1
        } else {
            0
        })
    }))
}

pub(super) fn ceiling(args: &[Value]) -> Result<Value> {
    Ok(round_to_integer(&args[0], f64::ceil))
}

pub(super) fn floor(args: &[Value]) -> Result<Value> {
    Ok(round_to_integer(&args[0], f64::floor))
}

fn round_to_integer(value: &Value, round: fn(f64) -> f64) -> Value {
    match number(value) {
        None => Value::Null,
        Some(Number::Integer(value)) => Value::from_i64(value),
        Some(Number::Double(value)) => Value::from_f64(round(value)),
    }
}

pub(super) fn modulo(args: &[Value]) -> Result<Value> {
    let (Some(dividend), Some(divisor)) = (number(&args[0]), number(&args[1])) else {
        return Ok(Value::Null);
    };
    Ok(match (dividend, divisor) {
        (_, Number::Integer(0)) => Value::Null,
        (Number::Integer(dividend), Number::Integer(divisor)) => {
            let remainder = dividend.unsigned_abs() % divisor.unsigned_abs();
            let remainder = remainder as i64;
            Value::from_i64(if dividend < 0 { -remainder } else { remainder })
        }
        _ if divisor.to_double() == 0.0 => Value::Null,
        _ => Value::from_f64(dividend.to_double() % divisor.to_double()),
    })
}

fn number(value: &Value) -> Option<Number> {
    Some(match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(value)) => Number::Integer(*value),
        Value::Numeric(Numeric::Float(value)) => Number::Double(f64::from(*value)),
        Value::Text(text) => Number::Double(string_to_double(text.as_str().as_bytes())),
        Value::Blob(bytes) => Number::Double(string_to_double(bytes)),
    })
}

pub(super) fn pow(args: &[Value]) -> Result<Value> {
    binary_double(args, |base, exponent| double_value(base.powf(exponent)))
}

pub(super) fn atan(args: &[Value]) -> Result<Value> {
    match args {
        [_] => unary_double(args, |value| Ok(Value::from_f64(value.atan()))),
        _ => atan2(args),
    }
}

pub(super) fn atan2(args: &[Value]) -> Result<Value> {
    binary_double(args, |y, x| double_value(y.atan2(x)))
}

pub(super) fn cot(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| double_value(1.0 / value.tan()))
}

pub(super) fn sin(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| double_value(value.sin()))
}

pub(super) fn cos(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| double_value(value.cos()))
}

pub(super) fn tan(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| double_value(value.tan()))
}

pub(super) fn asin(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| Ok(inverse_trigonometric(value, f64::asin)))
}

pub(super) fn acos(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| Ok(inverse_trigonometric(value, f64::acos)))
}

fn inverse_trigonometric(value: f64, function: fn(f64) -> f64) -> Value {
    if (-1.0..=1.0).contains(&value) {
        Value::from_f64(function(value))
    } else {
        Value::Null
    }
}

pub(super) fn degrees(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| {
        double_value(value * (180.0 / std::f64::consts::PI))
    })
}

pub(super) fn radians(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| {
        double_value(value * (std::f64::consts::PI / 180.0))
    })
}

pub(super) fn exp(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| double_value(value.exp()))
}

pub(super) fn sqrt(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| {
        Ok(if value < 0.0 {
            Value::Null
        } else {
            Value::from_f64(value.sqrt())
        })
    })
}

pub(super) fn log(args: &[Value]) -> Result<Value> {
    match args {
        [_] => ln(args),
        _ => {
            let Some(base) = double(&args[0]) else {
                return Ok(Value::Null);
            };
            if base <= 0.0 {
                return Ok(Value::Null);
            }
            let Some(value) = double(&args[1]) else {
                return Ok(Value::Null);
            };
            if value <= 0.0 || base == 1.0 {
                return Ok(Value::Null);
            }
            Ok(Value::from_f64(value.ln() / base.ln()))
        }
    }
}

pub(super) fn ln(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| Ok(logarithm(value, f64::ln)))
}

pub(super) fn log2(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| Ok(logarithm(value, f64::log2)))
}

pub(super) fn log10(args: &[Value]) -> Result<Value> {
    unary_double(args, |value| Ok(logarithm(value, f64::log10)))
}

fn logarithm(value: f64, function: fn(f64) -> f64) -> Value {
    if value <= 0.0 {
        Value::Null
    } else {
        Value::from_f64(function(value))
    }
}

fn unary_double(args: &[Value], function: impl Fn(f64) -> Result<Value>) -> Result<Value> {
    match double(&args[0]) {
        Some(value) => function(value),
        None => Ok(Value::Null),
    }
}

fn binary_double(args: &[Value], function: impl Fn(f64, f64) -> Result<Value>) -> Result<Value> {
    match (double(&args[0]), double(&args[1])) {
        (Some(left), Some(right)) => function(left, right),
        _ => Ok(Value::Null),
    }
}

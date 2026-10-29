//! `FORMAT(X, D[, locale])`, which rounds X to D decimal places and groups
//! the digits of its integer part as the locale writes numbers.
use super::convert;
use crate::types::decimal_number::{Decimal, Rounding};
use crate::types::unsupported_value;
use turso_core::{Numeric, Result, Value};

/// MySQL rounds to at most 30 decimal places.
const LARGEST_PLACES: i64 = 30;

/// `FORMAT` of an integer, a DOUBLE or a string. MySQL rounds an integer
/// exactly, half away from zero, and a DOUBLE (or a string read as one) as
/// `rint` does, half to even, after scaling it by the power of ten.
pub(super) fn format(args: &[Value]) -> Result<Value> {
    let Some(places) = places(&args[1]) else {
        return Ok(Value::Null);
    };
    let separators = separators(args.get(2))?;
    let double = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => {
            let rounded = Decimal::from_i64(*number).rounded(places, Rounding::HalfAwayFromZero);
            return Ok(Value::build_text(group(&rounded.to_text(), separators)));
        }
        Value::Numeric(Numeric::Float(double)) => f64::from(*double),
        Value::Text(text) => convert::string_to_double(text.as_str().as_bytes()),
        Value::Blob(bytes) => convert::string_to_double(bytes),
    };
    let power = 10f64.powi(places as i32);
    let scaled = double * power;
    let rounded = if scaled.is_finite() {
        scaled.round_ties_even() / power
    } else {
        double
    };
    if !rounded.is_finite() || (rounded == 0.0 && rounded.is_sign_negative()) {
        return Err(unsupported_value(format!(
            "FORMAT of {double}, which rounds to {rounded}"
        )));
    }
    Ok(Value::build_text(group(
        &format!("{rounded:.0$}", places as usize),
        separators,
    )))
}

/// `FORMAT` of a DECIMAL, rounded exactly, half away from zero.
pub(crate) fn format_decimal(decimal: &Decimal, args: &[Value]) -> Result<Value> {
    let Some(places) = places(&args[1]) else {
        return Ok(Value::Null);
    };
    let separators = separators(args.get(2))?;
    let rounded = decimal.rounded(places, Rounding::HalfAwayFromZero);
    if rounded.is_zero() && decimal.is_negative() {
        return Err(unsupported_value(format!(
            "FORMAT of {}, which rounds to zero",
            decimal.to_text()
        )));
    }
    Ok(Value::build_text(group(&rounded.to_text(), separators)))
}

/// The number of decimal places, read as MySQL reads an integer argument;
/// `None` for NULL.
fn places(value: &Value) -> Option<i64> {
    let places = match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(places)) => *places,
        Value::Numeric(Numeric::Float(places)) => f64::from(*places).round_ties_even() as i64,
        Value::Text(text) => convert::string_to_integer(text.as_str().as_bytes()),
        Value::Blob(bytes) => convert::string_to_integer(bytes),
    };
    Some(places.clamp(0, LARGEST_PLACES))
}

#[derive(Clone, Copy)]
struct Separators {
    thousands: char,
    decimal_point: char,
}

/// The separators of the locales whose number format is modeled; others
/// are refused. A NULL locale is `en_US`, as in MySQL.
fn separators(locale: Option<&Value>) -> Result<Separators> {
    let name = match locale {
        None | Some(Value::Null) => "en_US".to_string(),
        Some(Value::Text(text)) => text.as_str().to_string(),
        Some(other) => return Err(unsupported_value(format!("FORMAT with the locale {other}"))),
    };
    let (thousands, decimal_point) = match name.to_ascii_lowercase().as_str() {
        "en_us" => (',', '.'),
        "da_dk" | "de_de" => ('.', ','),
        _ => {
            return Err(unsupported_value(format!(
                "FORMAT with the locale `{name}`"
            )))
        }
    };
    Ok(Separators {
        thousands,
        decimal_point,
    })
}

/// Groups the integer digits of `text` (an optional `-`, digits and an
/// optional `.` fraction) by three and writes the locale's separators.
fn group(text: &str, separators: Separators) -> String {
    let (sign, unsigned) = match text.strip_prefix('-') {
        Some(unsigned) => ("-", unsigned),
        None => ("", text),
    };
    let (whole, fraction) = match unsigned.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (unsigned, None),
    };
    let mut grouped = String::with_capacity(text.len() + whole.len() / 3 + 1);
    grouped.push_str(sign);
    for (position, digit) in whole.chars().enumerate() {
        if position > 0 && (whole.len() - position) % 3 == 0 {
            grouped.push(separators.thousands);
        }
        grouped.push(digit);
    }
    if let Some(fraction) = fraction {
        grouped.push(separators.decimal_point);
        grouped.push_str(fraction);
    }
    grouped
}

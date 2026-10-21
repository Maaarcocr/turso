use std::borrow::Cow;

use turso_core::{LimboError, Numeric, Result, Value};

use super::decimal::Decimal;
use super::parse::{self, DateFlags};
use super::temporal::{Kind, MyTime, MAX_FSP};

const BIGINT_LIMIT: f64 = 9_223_372_036_854_775_808.0;
const UNSIGNED_BIGINT_MAX_AS_REAL: f64 = 18_446_744_073_709_551_616.0;

#[derive(Clone, Copy, Debug)]
pub(super) enum Arg<'a> {
    Null,
    Int(i64),
    Unsigned(u64),
    Double(f64),
    Text(&'a [u8]),
    Binary(&'a [u8]),
}

impl<'a> Arg<'a> {
    pub fn new(value: &'a Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Numeric(Numeric::Integer(i)) => Self::Int(*i),
            Value::Numeric(Numeric::Float(f)) => Self::from_real(f64::from(*f)),
            Value::Text(text) => Self::Text(text.as_str().as_bytes()),
            Value::Blob(blob) => Self::Binary(blob),
        }
    }

    fn from_real(number: f64) -> Self {
        if number == BIGINT_LIMIT {
            Self::Unsigned(1 << 63)
        } else if number == UNSIGNED_BIGINT_MAX_AS_REAL {
            Self::Unsigned(u64::MAX)
        } else {
            Self::Double(number)
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    pub fn is_unsigned(&self) -> bool {
        matches!(self, Self::Unsigned(_))
    }

    pub fn datetime(&self, flags: DateFlags) -> Option<MyTime> {
        match *self {
            Self::Null => None,
            Self::Int(number) => parse::datetime_from_number(number, flags),
            Self::Unsigned(number) => parse::datetime_from_number(number as i64, flags),
            Self::Double(number) => datetime_from_double(number, flags),
            Self::Text(text) | Self::Binary(text) => {
                let flags = DateFlags {
                    no_zero_date: true,
                    ..flags
                };
                let parsed = parse::parse_datetime(text, flags).ok()?;
                let mut time = parsed.time;
                time.add_nanos_to_datetime(parsed.rounding_nanos)
                    .then_some(time)
            }
        }
    }

    pub fn time(&self) -> Option<MyTime> {
        match *self {
            Self::Null => None,
            Self::Int(number) => parse::time_from_number(number),
            Self::Unsigned(number) => parse::time_from_number(number as i64),
            Self::Double(number) => time_from_double(number),
            Self::Text(text) | Self::Binary(text) => {
                let parsed = parse::parse_time(text)?;
                let mut time = parsed.time;
                if time.kind == Kind::Time {
                    time.add_nanos_to_time(parsed.rounding_nanos);
                    Some(time)
                } else {
                    time.add_nanos_to_datetime(parsed.rounding_nanos)
                        .then_some(time)
                }
            }
        }
    }

    pub fn datetime_fsp(&self) -> u8 {
        match *self {
            Self::Null | Self::Int(_) | Self::Unsigned(_) => 0,
            Self::Double(_) => MAX_FSP,
            Self::Text(text) => parse::parse_datetime(text, DateFlags::FUZZY)
                .map_or(MAX_FSP, |parsed| parsed.fraction_digits),
            Self::Binary(bytes) => parse::parse_datetime(bytes, DateFlags::FUZZY)
                .map_or(0, |parsed| parsed.fraction_digits),
        }
    }

    pub fn time_fsp(&self) -> u8 {
        match *self {
            Self::Null | Self::Int(_) | Self::Unsigned(_) => 0,
            Self::Double(_) => MAX_FSP,
            Self::Text(text) => {
                parse::parse_time(text).map_or(MAX_FSP, |parsed| parsed.fraction_digits)
            }
            Self::Binary(bytes) => {
                parse::parse_time(bytes).map_or(0, |parsed| parsed.fraction_digits)
            }
        }
    }

    pub fn number_fsp(&self) -> u8 {
        match *self {
            Self::Null | Self::Int(_) | Self::Unsigned(_) | Self::Binary(_) => 0,
            Self::Double(_) | Self::Text(_) => MAX_FSP,
        }
    }

    pub fn int(&self) -> Result<Option<i64>> {
        Ok(match *self {
            Self::Null => None,
            Self::Int(number) => Some(number),
            Self::Unsigned(number) => Some(number as i64),
            Self::Double(number) => Some(int_from_double(number)?),
            Self::Text(text) => Some(int_from_text(text)),
            Self::Binary(bytes) => Some(int_from_binary(bytes) as i64),
        })
    }

    pub fn decimal(&self) -> Option<Decimal> {
        match *self {
            Self::Null => None,
            Self::Int(number) => Some(Decimal::from_int(number)),
            Self::Unsigned(number) => Some(Decimal::from_unsigned(number)),
            Self::Double(number) => Some(Decimal::from_double(number)),
            Self::Text(text) => Some(Decimal::parse(text)),
            Self::Binary(bytes) => Some(Decimal::from_unsigned(int_from_binary(bytes))),
        }
    }

    pub fn text(&self) -> Option<Cow<'a, str>> {
        match *self {
            Self::Null => None,
            Self::Int(number) => Some(Cow::Owned(number.to_string())),
            Self::Unsigned(number) => Some(Cow::Owned(number.to_string())),
            Self::Double(number) => Some(Cow::Owned(format_double(number))),
            Self::Text(text) | Self::Binary(text) => Some(String::from_utf8_lossy(text)),
        }
    }
}

fn datetime_from_double(number: f64, flags: DateFlags) -> Option<MyTime> {
    let (seconds, nanos) = split_double(number);
    if nanos < 0 {
        return None;
    }
    let mut time = parse::datetime_from_number(seconds, flags)?;
    if time.kind == Kind::DateTime {
        time.micro = (nanos / 1000) as u32;
        if !time.add_nanos_to_datetime((nanos % 1000) as u32) {
            return None;
        }
    }
    Some(time)
}

fn time_from_double(number: f64) -> Option<MyTime> {
    let (seconds, nanos) = split_double(number);
    let mut time = parse::time_from_number(seconds)?;
    time.negative |= nanos < 0;
    let nanos = nanos.unsigned_abs();
    time.micro = (nanos / 1000) as u32;
    time.add_nanos_to_time((nanos % 1000) as u32);
    Some(time)
}

fn split_double(number: f64) -> (i64, i64) {
    if number >= BIGINT_LIMIT {
        return (i64::MAX, 0);
    }
    if number <= -BIGINT_LIMIT {
        return (i64::MIN, 0);
    }
    let seconds = number.trunc();
    let nanos = ((number - seconds) * 1_000_000_000.0).round();
    (seconds as i64, nanos as i64)
}

fn int_from_double(number: f64) -> Result<i64> {
    if number < -BIGINT_LIMIT {
        return Err(LimboError::InvalidArgument(format!(
            "BIGINT value is out of range in '{}'",
            format_double(number)
        )));
    }
    if number >= BIGINT_LIMIT {
        return Ok(i64::MAX);
    }
    Ok(number.round_ties_even() as i64)
}

pub(super) fn int_from_text(text: &[u8]) -> i64 {
    let mut pos = 0;
    while pos < text.len() && parse::is_space(text[pos]) {
        pos += 1;
    }
    let negative = pos < text.len() && text[pos] == b'-';
    if pos < text.len() && (text[pos] == b'-' || text[pos] == b'+') {
        pos += 1;
    }
    let mut value: u64 = 0;
    let mut overflow = false;
    while pos < text.len() && text[pos].is_ascii_digit() {
        match value
            .checked_mul(10)
            .and_then(|v| v.checked_add(u64::from(text[pos] - b'0')))
        {
            Some(next) => value = next,
            None => overflow = true,
        }
        pos += 1;
    }
    if negative {
        if overflow || value > 1 << 63 {
            return i64::MIN;
        }
        return (value as i64).wrapping_neg();
    }
    if overflow {
        return u64::MAX as i64;
    }
    value as i64
}

fn int_from_binary(bytes: &[u8]) -> u64 {
    let start = bytes.len().saturating_sub(8);
    bytes[start..]
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte))
}

pub(super) fn format_double(number: f64) -> String {
    if number == 0.0 {
        return if number.is_sign_negative() { "-0" } else { "0" }.to_string();
    }
    let scientific = format!("{:e}", number.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("scientific notation has an exponent");
    let exponent: i32 = exponent.parse().expect("exponent is an integer");
    let digits = mantissa.replace('.', "");
    let sign = if number < 0.0 { "-" } else { "" };
    let integer_digits = exponent + 1;
    let is_integer = digits.len() as i32 <= integer_digits;
    if exponent < -15 || (exponent >= 15 && is_integer) {
        format!("{sign}{mantissa}e{exponent}")
    } else if exponent < 0 {
        let zeros = "0".repeat((-integer_digits) as usize);
        format!("{sign}0.{zeros}{digits}")
    } else if is_integer {
        let zeros = "0".repeat(integer_digits as usize - digits.len());
        format!("{sign}{digits}{zeros}")
    } else {
        let (integer, fraction) = digits.split_at(integer_digits as usize);
        format!("{sign}{integer}.{fraction}")
    }
}

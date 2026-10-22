use std::borrow::Cow;

use turso_core::{LimboError, Numeric, Result, Value};

pub(super) const MAX_ALLOWED_PACKET: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub(super) struct Integer {
    pub(super) value: i64,
}

impl Integer {
    pub(super) fn is_negative(self) -> bool {
        self.value < 0
    }

    pub(super) fn is_positive(self) -> bool {
        self.value > 0
    }

    pub(super) fn as_unsigned(self) -> u64 {
        self.value as u64
    }

    pub(super) fn count_up_to_int32_max(self) -> u64 {
        self.as_unsigned().min(i32::MAX as u64)
    }
}

#[derive(Clone, Debug)]
pub(super) enum Str<'a> {
    Text(Cow<'a, str>),
    Binary(Cow<'a, [u8]>),
}

impl Str<'_> {
    pub(super) fn as_bytes(&self) -> &[u8] {
        match self {
            Str::Text(text) => text.as_bytes(),
            Str::Binary(bytes) => bytes,
        }
    }

    pub(super) fn is_binary(&self) -> bool {
        matches!(self, Str::Binary(_))
    }

    pub(super) fn len(&self) -> usize {
        self.as_bytes().len()
    }

    pub(super) fn char_count(&self) -> usize {
        match self {
            Str::Text(text) => text.chars().count(),
            Str::Binary(bytes) => bytes.len(),
        }
    }

    pub(super) fn char_length(&self, start: usize, count: usize) -> usize {
        match self {
            Str::Text(text) => {
                let rest = &text[start..];
                let mut seen = 0;
                for (offset, _) in rest.char_indices() {
                    if seen == count {
                        return offset;
                    }
                    seen += 1;
                }
                if seen == count {
                    rest.len()
                } else {
                    rest.len() + 2
                }
            }
            Str::Binary(bytes) => {
                let rest = bytes.len() - start;
                if count > rest {
                    rest + 2
                } else {
                    count
                }
            }
        }
    }

    pub(super) fn slice(&self, start: usize, end: usize) -> Str<'_> {
        match self {
            Str::Text(text) => Str::Text(Cow::Borrowed(&text[start..end])),
            Str::Binary(bytes) => Str::Binary(Cow::Borrowed(&bytes[start..end])),
        }
    }

    pub(super) fn empty(&self) -> Str<'static> {
        match self {
            Str::Text(_) => Str::Text(Cow::Borrowed("")),
            Str::Binary(_) => Str::Binary(Cow::Borrowed(b"")),
        }
    }

    pub(super) fn into_binary(self) -> Str<'static> {
        Str::Binary(Cow::Owned(self.as_bytes().to_vec()))
    }

    pub(super) fn into_owned(self) -> Str<'static> {
        match self {
            Str::Text(text) => Str::Text(Cow::Owned(text.into_owned())),
            Str::Binary(bytes) => Str::Binary(Cow::Owned(bytes.into_owned())),
        }
    }

    pub(super) fn into_value(self) -> Result<Value> {
        match self {
            Str::Text(text) => Ok(Value::build_text(text.into_owned())),
            Str::Binary(bytes) => Ok(Value::from_slice(&bytes)?),
        }
    }
}

pub(super) fn integer(value: &Value) -> Option<Integer> {
    let value = match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(integer)) => *integer,
        Value::Numeric(Numeric::Float(double)) => double_to_integer(f64::from(*double)),
        Value::Text(text) => string_to_integer(text.as_str().as_bytes()),
        Value::Blob(bytes) => string_to_integer(bytes),
    };
    Some(Integer { value })
}

pub(super) fn double(value: &Value) -> Option<f64> {
    Some(match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(integer)) => *integer as f64,
        Value::Numeric(Numeric::Float(double)) => f64::from(*double),
        Value::Text(text) => string_to_double(text.as_str().as_bytes()),
        Value::Blob(bytes) => string_to_double(bytes),
    })
}

pub(super) fn string_converted_to_text(
    value: &Value,
    convert_binary: bool,
) -> Result<Option<Str<'_>>> {
    match value {
        Value::Blob(bytes) if convert_binary => match std::str::from_utf8(bytes) {
            Ok(text) => Ok(Some(Str::Text(Cow::Borrowed(text)))),
            Err(_) => Err(LimboError::InvalidArgument(format!(
                "Cannot convert string '{}' from binary to utf8mb4",
                bytes
                    .iter()
                    .map(|byte| format!("\\x{byte:02X}"))
                    .collect::<String>()
            ))),
        },
        other => Ok(string(other)),
    }
}

pub(super) fn string(value: &Value) -> Option<Str<'_>> {
    Some(match value {
        Value::Null => return None,
        Value::Numeric(Numeric::Integer(integer)) => Str::Text(Cow::Owned(integer.to_string())),
        Value::Numeric(Numeric::Float(double)) => {
            Str::Text(Cow::Owned(format_double(f64::from(*double))))
        }
        Value::Text(text) => Str::Text(Cow::Borrowed(text.as_str())),
        Value::Blob(bytes) => Str::Binary(Cow::Borrowed(bytes)),
    })
}

pub(super) fn double_to_integer(value: f64) -> i64 {
    if value <= i64::MIN as f64 {
        i64::MIN
    } else if value >= i64::MAX as f64 {
        i64::MAX
    } else {
        value.round_ties_even() as i64
    }
}

pub(super) fn string_to_integer(bytes: &[u8]) -> i64 {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    let (negative, digits) = split_sign(&bytes[start..]);
    let (magnitude, overflow) = parse_digits(digits, 10);
    match (negative, overflow) {
        (true, false) if magnitude <= 1 << 63 => (magnitude as i64).wrapping_neg(),
        (true, _) => i64::MIN,
        (false, true) => u64::MAX as i64,
        (false, false) => magnitude as i64,
    }
}

pub(super) fn string_to_double(bytes: &[u8]) -> f64 {
    let start = bytes
        .iter()
        .position(|byte| !is_space(*byte))
        .unwrap_or(bytes.len());
    let rest = &bytes[start..];
    let length = numeric_prefix_length(rest);
    if length == 0 {
        return 0.0;
    }
    let text = std::str::from_utf8(&rest[..length]).expect("a numeric prefix is ASCII");
    let value: f64 = text
        .parse()
        .expect("a numeric prefix is valid float syntax");
    value.clamp(f64::MIN, f64::MAX)
}

pub(super) fn is_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

fn numeric_prefix_length(bytes: &[u8]) -> usize {
    let digits_from = |start: usize| {
        start
            + bytes[start..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count()
    };
    let integer_start = usize::from(matches!(bytes.first(), Some(b'+' | b'-')));
    let mut end = digits_from(integer_start);
    let mut digit_count = end - integer_start;
    if bytes.get(end) == Some(&b'.') {
        let fraction_end = digits_from(end + 1);
        digit_count += fraction_end - (end + 1);
        end = fraction_end;
    }
    if digit_count == 0 {
        return 0;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent_start = end + 1 + usize::from(matches!(bytes.get(end + 1), Some(b'+' | b'-')));
        let exponent_end = digits_from(exponent_start);
        if exponent_end > exponent_start {
            end = exponent_end;
        }
    }
    end
}

pub(super) fn split_sign(bytes: &[u8]) -> (bool, &[u8]) {
    match bytes.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, bytes),
    }
}

pub(super) fn parse_digits(bytes: &[u8], base: u32) -> (u64, bool) {
    let mut value: u64 = 0;
    let mut overflow = false;
    for digit in bytes
        .iter()
        .map_while(|byte| (*byte as char).to_digit(base))
    {
        match value
            .checked_mul(u64::from(base))
            .and_then(|value| value.checked_add(u64::from(digit)))
        {
            Some(next) => value = next,
            None => overflow = true,
        }
    }
    (value, overflow)
}

pub(super) fn double_value(value: f64) -> Result<Value> {
    if value.is_finite() {
        Ok(Value::from_f64(value))
    } else {
        Err(out_of_range("DOUBLE"))
    }
}

pub(super) fn out_of_range(kind: &str) -> LimboError {
    LimboError::InvalidArgument(format!("{kind} value is out of range"))
}

pub(super) fn bytes_value(bytes: Vec<u8>, binary: bool) -> Result<Value> {
    if binary {
        return Ok(Value::from_slice(&bytes)?);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(Value::build_text(text)),
        Err(error) => Ok(Value::from_slice(error.as_bytes())?),
    }
}

pub(super) fn format_double(value: f64) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0");
    }
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("`{:e}` output has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let digits = mantissa.replace('.', "");
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

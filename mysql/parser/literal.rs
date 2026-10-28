use crate::names::quote_string;
use crate::MysqlParserError;
use sqlparser::ast::{DataType, Expr, Ident, TimezoneInfo, TypedString, Value};
use turso_parser::ast;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Context {
    Value,
    Bytes,
    DateTimeArgument,
    Number,
    Bitwise,
    Arithmetic,
    Text,
}

impl Context {
    pub(crate) fn is_value(self) -> bool {
        matches!(
            self,
            Context::Value | Context::Bytes | Context::DateTimeArgument
        )
    }
}

pub(crate) fn translate_value(
    value: &Value,
    context: Context,
) -> Result<ast::Expr, MysqlParserError> {
    let literal = match value {
        Value::Number(number, false) if context == Context::Bitwise => {
            ast::Literal::Numeric(bitwise_operand(number)?)
        }
        Value::Number(number, false) => ast::Literal::Numeric(number.clone()),
        Value::SingleQuotedString(text)
        | Value::DoubleQuotedString(text)
        | Value::NationalStringLiteral(text) => ast::Literal::String(quote_string(text)),
        Value::HexStringLiteral(digits) => return binary_literal(hex_bytes(digits)?, context),
        Value::SingleQuotedByteStringLiteral(bits) => {
            return binary_literal(bit_bytes(bits)?, context)
        }
        Value::Boolean(true) => ast::Literal::True,
        Value::Boolean(false) => ast::Literal::False,
        Value::Null => ast::Literal::Null,
        other => return Err(unsupported(format!("literal `{other}`"))),
    };
    Ok(ast::Expr::Literal(literal))
}

fn bitwise_operand(number: &str) -> Result<String, MysqlParserError> {
    match number.parse::<i64>() {
        Ok(_) => Ok(number.to_string()),
        Err(_) => Err(unsupported(format!(
            "bit operation on `{number}`, which MySQL rounds to BIGINT UNSIGNED,"
        ))),
    }
}

pub(crate) fn translate_introduced_string(
    introducer: &Ident,
    value: &Expr,
    context: Context,
) -> Result<ast::Expr, MysqlParserError> {
    if introducer.quote_style.is_some() {
        return Err(unsupported(format!("introducer `{introducer}`")));
    }
    let Expr::Value(value) = value else {
        return Err(unsupported(format!("introduced value `{value}`")));
    };
    let bytes = match &value.value {
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
            text.as_bytes().to_vec()
        }
        Value::HexStringLiteral(digits) => hex_bytes(digits)?,
        other => return Err(unsupported(format!("introduced value `{other}`"))),
    };
    let charset = introducer.value.to_ascii_lowercase();
    match charset.as_str() {
        "_binary" => binary_string(bytes, context),
        "_utf8mb4" | "_utf8mb3" | "_utf8" => text_literal(bytes),
        "_ascii" | "_latin1" if bytes.is_ascii() => text_literal(bytes),
        _ => Err(unsupported(format!(
            "character set introducer `{introducer}` with this value"
        ))),
    }
}

pub(crate) fn translate_as_bytes(value: &Value) -> Result<ast::Expr, MysqlParserError> {
    let bytes = match value {
        Value::HexStringLiteral(digits) => hex_bytes(digits)?,
        Value::SingleQuotedByteStringLiteral(bits) => bit_bytes(bits)?,
        other => return Err(unsupported(format!("literal `{other}` as bytes"))),
    };
    Ok(blob_literal(&bytes))
}

fn binary_literal(bytes: Vec<u8>, context: Context) -> Result<ast::Expr, MysqlParserError> {
    match context {
        Context::Arithmetic => Err(unsupported(
            "a binary literal in arithmetic, which MySQL computes as BIGINT UNSIGNED,".to_string(),
        )),
        Context::Number | Context::Bitwise => {
            let number = unsigned_number(&bytes)?;
            Ok(ast::Expr::Literal(ast::Literal::Numeric(
                number.to_string(),
            )))
        }
        Context::Bytes | Context::DateTimeArgument => Ok(blob_literal(&bytes)),
        Context::Text | Context::Value if !is_plain_text(&bytes) => Err(not_plain_text()),
        Context::Text => text_literal(bytes),
        Context::Value => Ok(blob_literal(&bytes)),
    }
}

fn binary_string(bytes: Vec<u8>, context: Context) -> Result<ast::Expr, MysqlParserError> {
    match context {
        Context::Value | Context::Bytes | Context::Text => binary_literal(bytes, context),
        Context::DateTimeArgument if is_plain_text(&bytes) => text_literal(bytes),
        Context::DateTimeArgument => Err(not_plain_text()),
        Context::Arithmetic => double_of_text(&bytes),
        Context::Number => {
            let end = bytes
                .iter()
                .position(|byte| *byte == 0 || !byte.is_ascii())
                .unwrap_or(bytes.len());
            text_literal(bytes[..end].to_vec())
        }
        Context::Bitwise => Err(unsupported(
            "a binary string in a bit operation, which MySQL computes on its bytes or its text,"
                .to_string(),
        )),
    }
}

fn double_of_text(bytes: &[u8]) -> Result<ast::Expr, MysqlParserError> {
    let start = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .unwrap_or(bytes.len());
    let text = &bytes[start..];
    let mut end = usize::from(matches!(text.first(), Some(b'+' | b'-')));
    let digits = |from: usize| {
        from + text[from..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count()
    };
    let whole_end = digits(end);
    let mut mantissa_end = whole_end;
    if text.get(whole_end) == Some(&b'.') {
        mantissa_end = digits(whole_end + 1);
    }
    let has_digits = mantissa_end > end + usize::from(mantissa_end > whole_end);
    if has_digits {
        end = mantissa_end;
        if matches!(text.get(end), Some(b'e' | b'E')) {
            let sign = usize::from(matches!(text.get(end + 1), Some(b'+' | b'-')));
            let exponent_end = digits(end + 1 + sign);
            if exponent_end > end + 1 + sign {
                end = exponent_end;
            }
        }
    }
    let value = if has_digits {
        std::str::from_utf8(&text[..end])
            .ok()
            .and_then(|number| number.parse::<f64>().ok())
            .unwrap_or(0.0)
    } else {
        0.0
    };
    if !value.is_finite() {
        return Err(unsupported(
            "a binary string beyond the DOUBLE range in arithmetic".to_string(),
        ));
    }
    Ok(ast::Expr::Literal(ast::Literal::Numeric(format!(
        "{value:e}"
    ))))
}

pub(crate) fn negated_binary_literal(value: &Value) -> Result<ast::Expr, MysqlParserError> {
    let number = binary_literal_value(value)?;
    if number >= FIXED_DOUBLE_DIGITS_LIMIT {
        return Err(unsupported(format!(
            "a negated binary literal of {number}, which MySQL shows as a DOUBLE with fixed digits,"
        )));
    }
    Ok(ast::Expr::Literal(ast::Literal::Numeric(format!(
        "-{number}e0"
    ))))
}

pub(crate) fn binary_literal_as_double(value: &Value) -> Result<ast::Expr, MysqlParserError> {
    let number = binary_literal_value(value)?;
    Ok(ast::Expr::Literal(ast::Literal::Numeric(format!(
        "{number}e0"
    ))))
}

pub(crate) fn binary_literal_value(value: &Value) -> Result<u64, MysqlParserError> {
    let bytes = match value {
        Value::HexStringLiteral(digits) => hex_bytes(digits)?,
        Value::SingleQuotedByteStringLiteral(bits) => bit_bytes(bits)?,
        other => return Err(unsupported(format!("binary literal `{other}`"))),
    };
    if bytes.len() > 8 {
        return Err(unsupported(
            "a binary literal longer than 8 bytes in arithmetic".to_string(),
        ));
    }
    Ok(bytes
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

const FIXED_DOUBLE_DIGITS_LIMIT: u64 = 1_000_000_000_000_000;

fn is_plain_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn not_plain_text() -> MysqlParserError {
    unsupported("a binary string that is not NUL-free UTF-8 outside a numeric context".to_string())
}

fn blob_literal(bytes: &[u8]) -> ast::Expr {
    let digits: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
    ast::Expr::Literal(ast::Literal::Blob(format!("x'{digits}'")))
}

fn text_literal(bytes: Vec<u8>) -> Result<ast::Expr, MysqlParserError> {
    let text = String::from_utf8(bytes)
        .map_err(|_| unsupported("a string that is not valid UTF-8".to_string()))?;
    Ok(ast::Expr::Literal(ast::Literal::String(quote_string(
        &text,
    ))))
}

fn unsigned_number(bytes: &[u8]) -> Result<i64, MysqlParserError> {
    if bytes.len() > 8 {
        return Err(unsupported(
            "a binary literal longer than 8 bytes in a numeric context".to_string(),
        ));
    }
    let value = bytes
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
    i64::try_from(value).map_err(|_| {
        unsupported(
            "a binary literal above the signed 64-bit range in a numeric context".to_string(),
        )
    })
}

fn hex_bytes(digits: &str) -> Result<Vec<u8>, MysqlParserError> {
    if digits.len() % 2 == 1 {
        return Err(unsupported(format!(
            "hexadecimal literal X'{digits}' with an odd number of digits"
        )));
    }
    (0..digits.len())
        .step_by(2)
        .map(|start| {
            u8::from_str_radix(&digits[start..start + 2], 16)
                .map_err(|_| unsupported(format!("hexadecimal literal X'{digits}'")))
        })
        .collect()
}

fn bit_bytes(bits: &str) -> Result<Vec<u8>, MysqlParserError> {
    if !bits.bytes().all(|bit| matches!(bit, b'0' | b'1')) {
        return Err(unsupported(format!("bit-value literal b'{bits}'")));
    }
    let padded = format!("{}{bits}", "0".repeat((8 - bits.len() % 8) % 8));
    Ok(padded
        .as_bytes()
        .chunks(8)
        .map(|chunk| {
            chunk
                .iter()
                .fold(0u8, |byte, bit| (byte << 1) | u8::from(*bit == b'1'))
        })
        .collect())
}

pub(crate) fn translate_typed_string(
    typed: &TypedString,
    context: Context,
) -> Result<ast::Expr, MysqlParserError> {
    let (temporal, text) = typed_temporal(typed)?;
    match context {
        Context::Arithmetic | Context::Bitwise => Err(unsupported(format!(
            "{} literal '{text}' in arithmetic",
            typed.data_type
        ))),
        Context::Number => Ok(temporal.number()),
        Context::Value | Context::Bytes | Context::DateTimeArgument | Context::Text => Ok(
            ast::Expr::Literal(ast::Literal::String(quote_string(&temporal.text()))),
        ),
    }
}

pub(crate) fn translate_typed_string_as_bytes(
    typed: &TypedString,
) -> Result<ast::Expr, MysqlParserError> {
    let (temporal, _) = typed_temporal(typed)?;
    Ok(blob_literal(temporal.text().as_bytes()))
}

fn typed_temporal(typed: &TypedString) -> Result<(Temporal, &str), MysqlParserError> {
    let text = match &typed.value.value {
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => text,
        other => return Err(unsupported(format!("typed literal value `{other}`"))),
    };
    let temporal = match &typed.data_type {
        DataType::Date => Temporal::date(text),
        DataType::Time(None, TimezoneInfo::None) => Temporal::time(text),
        DataType::Timestamp(None, TimezoneInfo::None) => Temporal::timestamp(text),
        other => return Err(unsupported(format!("typed literal `{other} '{text}'`"))),
    }
    .ok_or_else(|| unsupported(format!("invalid {} literal '{text}'", typed.data_type)))?;
    Ok((temporal, text))
}

pub(crate) fn translate_temporal_text(
    data_type: &DataType,
    text: &str,
) -> Result<ast::Expr, MysqlParserError> {
    let temporal = match data_type {
        DataType::Date => Temporal::date(text),
        DataType::Time(None, TimezoneInfo::None) => Temporal::time(text),
        DataType::Timestamp(None, TimezoneInfo::None) => Temporal::timestamp(text),
        _ => None,
    }
    .ok_or_else(|| unsupported(format!("comparison of a {data_type} with '{text}'")))?;
    Ok(ast::Expr::Literal(ast::Literal::String(quote_string(
        &temporal.text(),
    ))))
}

struct Date {
    year: u32,
    month: u32,
    day: u32,
}

struct Time {
    negative: bool,
    hours: u32,
    minutes: u32,
    seconds: u32,
    fraction: String,
}

enum Temporal {
    Date(Date),
    Time(Time),
    Timestamp(Date, Time),
}

impl Temporal {
    fn date(text: &str) -> Option<Self> {
        parse_date(text.trim()).map(Temporal::Date)
    }

    fn time(text: &str) -> Option<Self> {
        parse_time(text.trim()).map(Temporal::Time)
    }

    fn timestamp(text: &str) -> Option<Self> {
        let text = text.trim();
        let split = text.find([' ', 'T'])?;
        let date = parse_date(&text[..split])?;
        let time = parse_clock(text[split + 1..].trim_start())?;
        (time.hours < 24).then_some(Temporal::Timestamp(date, time))
    }

    fn text(&self) -> String {
        match self {
            Temporal::Date(date) => date_text(date),
            Temporal::Time(time) => time_text(time),
            Temporal::Timestamp(date, time) => format!("{} {}", date_text(date), time_text(time)),
        }
    }

    fn number(&self) -> ast::Expr {
        let digits = match self {
            Temporal::Date(date) => date_number(date),
            Temporal::Time(time) => time_number(time),
            Temporal::Timestamp(date, time) => {
                format!("{}{}", date_number(date), time_number(time))
            }
        };
        let number = ast::Expr::Literal(ast::Literal::Numeric(digits));
        match self {
            Temporal::Time(Time { negative: true, .. }) => {
                ast::Expr::unary(ast::UnaryOperator::Negative, number)
            }
            _ => number,
        }
    }
}

fn date_text(date: &Date) -> String {
    format!("{:04}-{:02}-{:02}", date.year, date.month, date.day)
}

fn date_number(date: &Date) -> String {
    format!("{:04}{:02}{:02}", date.year, date.month, date.day)
}

fn time_text(time: &Time) -> String {
    let sign = if time.negative { "-" } else { "" };
    let fraction = fraction_suffix(&time.fraction);
    format!(
        "{sign}{:02}:{:02}:{:02}{fraction}",
        time.hours, time.minutes, time.seconds
    )
}

fn time_number(time: &Time) -> String {
    let fraction = fraction_suffix(&time.fraction);
    format!(
        "{:02}{:02}{:02}{fraction}",
        time.hours, time.minutes, time.seconds
    )
}

fn fraction_suffix(fraction: &str) -> String {
    if fraction.is_empty() {
        String::new()
    } else {
        format!(".{fraction}")
    }
}

fn parse_date(text: &str) -> Option<Date> {
    let (year, month, day) = if text.bytes().all(|byte| byte.is_ascii_digit()) {
        match text.len() {
            8 => (&text[..4], &text[4..6], &text[6..]),
            6 => (&text[..2], &text[2..4], &text[4..]),
            _ => return None,
        }
    } else {
        let mut parts = text.split(['-', '/']);
        let parts = (parts.next()?, parts.next()?, parts.next()?, parts.next());
        let (year, month, day, None) = parts else {
            return None;
        };
        (year, month, day)
    };
    let year = match year.len() {
        4 => digits(year)?,
        2 => match digits(year)? {
            year @ 0..=69 => 2000 + year,
            year => 1900 + year,
        },
        _ => return None,
    };
    let month = small_number(month)?;
    let day = small_number(day)?;
    ((1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day) && year >= 1)
        .then_some(Date { year, month, day })
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn parse_time(text: &str) -> Option<Time> {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let mut time = match text.split_once(' ') {
        Some((days, clock)) => {
            let days = small_number(days)?;
            let mut time = parse_clock(clock)?;
            time.hours = days.checked_mul(24)?.checked_add(time.hours)?;
            time
        }
        None if text.contains(':') => parse_clock(text)?,
        None => parse_compact_time(text)?,
    };
    time.negative = negative;
    (time.hours <= 838).then_some(time)
}

fn parse_compact_time(text: &str) -> Option<Time> {
    let (whole, fraction) = split_fraction(text)?;
    if whole.is_empty() || whole.len() > 6 || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let padded = format!("{whole:0>6}");
    let time = Time {
        negative: false,
        hours: digits(&padded[..2])?,
        minutes: digits(&padded[2..4])?,
        seconds: digits(&padded[4..])?,
        fraction,
    };
    (time.minutes < 60 && time.seconds < 60).then_some(time)
}

fn parse_clock(text: &str) -> Option<Time> {
    let (whole, fraction) = split_fraction(text)?;
    let mut parts = whole.split(':');
    let hours = number_up_to(parts.next()?, 3)?;
    let minutes = small_number(parts.next()?)?;
    let seconds = match parts.next() {
        Some(seconds) => small_number(seconds)?,
        None if fraction.is_empty() => 0,
        None => return None,
    };
    if parts.next().is_some() || minutes >= 60 || seconds >= 60 {
        return None;
    }
    Some(Time {
        negative: false,
        hours,
        minutes,
        seconds,
        fraction,
    })
}

fn split_fraction(text: &str) -> Option<(&str, String)> {
    match text.split_once('.') {
        Some((whole, fraction))
            if !fraction.is_empty()
                && fraction.len() <= 6
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            Some((whole, fraction.to_string()))
        }
        Some(_) => None,
        None => Some((text, String::new())),
    }
}

fn small_number(text: &str) -> Option<u32> {
    number_up_to(text, 2)
}

fn number_up_to(text: &str, max_digits: usize) -> Option<u32> {
    (!text.is_empty() && text.len() <= max_digits)
        .then(|| digits(text))
        .flatten()
}

fn digits(text: &str) -> Option<u32> {
    text.bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| text.parse().ok())
        .flatten()
}

fn unsupported(what: String) -> MysqlParserError {
    MysqlParserError::Unsupported(format!("{what} is not lowered yet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporal_text(data_type: DataType, text: &str) -> Option<String> {
        let temporal = match data_type {
            DataType::Date => Temporal::date(text),
            DataType::Time(..) => Temporal::time(text),
            _ => Temporal::timestamp(text),
        };
        temporal.map(|temporal| temporal.text())
    }

    #[test]
    fn normalizes_temporal_literals_like_mysql() {
        let date = || DataType::Date;
        let time = || DataType::Time(None, TimezoneInfo::None);
        let timestamp = || DataType::Timestamp(None, TimezoneInfo::None);
        let cases = [
            (date(), "2018-5-1", Some("2018-05-01")),
            (date(), "20180501", Some("2018-05-01")),
            (date(), "18-05-01", Some("2018-05-01")),
            (date(), "2018/05/01", Some("2018-05-01")),
            (date(), "2018-02-30", None),
            (date(), "0000-00-00", None),
            (date(), "2018-05-01 10:00:00", None),
            (time(), "1:2:3", Some("01:02:03")),
            (time(), "10:00", Some("10:00:00")),
            (time(), "10", Some("00:00:10")),
            (time(), "-1:00:00", Some("-01:00:00")),
            (time(), "1 02:00:00", Some("26:00:00")),
            (time(), "10:00:00.123", Some("10:00:00.123")),
            (time(), "838:59:59", Some("838:59:59")),
            (time(), "839:00:00", None),
            (time(), " 10:00:00", Some("10:00:00")),
            (timestamp(), "2020-1-2 3:4:5", Some("2020-01-02 03:04:05")),
            (
                timestamp(),
                "2020-01-02T03:04:05",
                Some("2020-01-02 03:04:05"),
            ),
            (timestamp(), "2020-01-02 03:04", Some("2020-01-02 03:04:00")),
            (
                timestamp(),
                "2020-01-02 03:04:05.5",
                Some("2020-01-02 03:04:05.5"),
            ),
            (timestamp(), "2020-01-02", None),
        ];
        for (data_type, text, expected) in cases {
            assert_eq!(
                temporal_text(data_type, text).as_deref(),
                expected,
                "{text}"
            );
        }
    }

    #[test]
    fn decodes_bit_literals_into_bytes() {
        assert_eq!(bit_bytes("1000001").unwrap(), vec![0x41]);
        assert_eq!(bit_bytes("").unwrap(), Vec::<u8>::new());
        assert_eq!(bit_bytes("100000001").unwrap(), vec![0x01, 0x01]);
    }
}

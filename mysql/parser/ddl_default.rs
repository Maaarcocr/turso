use crate::ddl_expr::DefaultValue;
use crate::ddl_type::{number, set_values, ColumnType, Family};
use crate::names::quote_string;
use crate::MysqlParserError;
use turso_parser::ast;

const TIMESTAMP_MIN: &str = "1970-01-01 00:00:01";
const TIMESTAMP_MAX: &str = "2038-01-19 03:14:07.999999";
const MAX_TIME_HOURS: u32 = 838;

/// Lowers a column DEFAULT the way MySQL stores it: the value is checked
/// against the column type and written in the form MySQL would return, so
/// rows that take the default read back like MySQL rows.
pub(crate) fn resolve_default(
    column_type: &ColumnType,
    value: DefaultValue,
    nullable: bool,
) -> Result<ast::Expr, MysqlParserError> {
    let resolved = match value {
        DefaultValue::Null if nullable => Some(ast::Expr::Literal(ast::Literal::Null)),
        DefaultValue::Null => None,
        DefaultValue::Expression(expr) => Some(expr),
        DefaultValue::CurrentTimestamp => match column_type.family {
            Family::Datetime { precision: 0 } | Family::Timestamp { precision: 0 } => {
                Some(ast::Expr::Literal(ast::Literal::CurrentTimestamp))
            }
            _ => None,
        },
        DefaultValue::Number(text) => number_default(&column_type.family, &text),
        DefaultValue::Text(text) => text_default(&column_type.family, &text),
    };
    resolved.ok_or_else(|| {
        MysqlParserError::Unsupported(
            "MySQL rejects this DEFAULT value for the column type".to_string(),
        )
    })
}

fn number_default(family: &Family, text: &str) -> Option<ast::Expr> {
    let decimal = Decimal::parse(text)?;
    match family {
        Family::Integer { min, max } => integer_in_range(&decimal, *min, *max),
        Family::Bit { width } => {
            let value = decimal.exact_integer()?;
            (0..1_i128 << width).contains(&value).then(|| number(value))
        }
        Family::Decimal {
            precision,
            scale,
            unsigned,
        } => decimal_in_range(&decimal, *precision, *scale, *unsigned),
        Family::Float {
            unsigned,
            digits: Some((precision, scale)),
        } => decimal_in_range(&decimal, *precision, *scale, *unsigned),
        Family::Float {
            unsigned,
            digits: None,
        } => (!*unsigned || !decimal.negative).then(|| numeric(decimal.to_text())),
        Family::Char { length } | Family::Varchar { length } => {
            fit_characters(&decimal.to_text(), *length, family).map(|text| string(&text))
        }
        Family::Varbinary { length } => {
            let text = decimal.to_text();
            (text.len() as u64 <= *length).then(|| string(&text))
        }
        Family::Year => year(decimal.exact_integer()?),
        _ => None,
    }
}

fn text_default(family: &Family, text: &str) -> Option<ast::Expr> {
    match family {
        Family::Integer { min, max } => integer_in_range(&Decimal::parse(text)?, *min, *max),
        Family::Decimal {
            precision,
            scale,
            unsigned,
        } => decimal_in_range(&Decimal::parse(text)?, *precision, *scale, *unsigned),
        Family::Float { .. } => number_default(family, text),
        Family::Char { length } | Family::Varchar { length } => {
            fit_characters(text, *length, family).map(|text| string(&text))
        }
        Family::Varbinary { length } => (text.len() as u64 <= *length).then(|| string(text)),
        Family::Enum(members) => members
            .iter()
            .find(|member| member.to_lowercase() == text.to_lowercase())
            .map(|member| string(member)),
        Family::Set(members) => set_default(members, text).map(|value| string(&value)),
        Family::Date => {
            let (date, time) = parse_datetime(text)?;
            time.is_none().then(|| string(&date.to_text()))
        }
        Family::Datetime { precision } => {
            let (date, time) = parse_datetime(text)?;
            let time = time.unwrap_or_default();
            Some(string(&format!(
                "{} {}",
                date.to_text(),
                time.to_text(*precision)?
            )))
        }
        Family::Timestamp { precision } => {
            let (date, time) = parse_datetime(text)?;
            let value = format!(
                "{} {}",
                date.to_text(),
                time.unwrap_or_default().to_text(*precision)?
            );
            (TIMESTAMP_MIN..=TIMESTAMP_MAX)
                .contains(&value.as_str())
                .then(|| string(&value))
        }
        Family::Time { precision } => {
            let time = parse_time(text, MAX_TIME_HOURS)?;
            Some(string(&time.to_text(*precision)?))
        }
        Family::Year => year(text.parse::<i128>().ok().filter(|_| text.len() == 4)?),
        Family::Bit { .. } | Family::Text { .. } | Family::Blob { .. } | Family::Json => None,
    }
}

fn integer_in_range(decimal: &Decimal, min: i128, max: i128) -> Option<ast::Expr> {
    let value = decimal.rounded_integer()?;
    (min..=max).contains(&value).then(|| number(value))
}

fn decimal_in_range(
    decimal: &Decimal,
    precision: u64,
    scale: u64,
    unsigned: bool,
) -> Option<ast::Expr> {
    let rounded = decimal.rounded(scale)?;
    let whole_digits = rounded.whole.trim_start_matches('0').len() as u64;
    if whole_digits > precision - scale || (unsigned && rounded.negative && !rounded.is_zero()) {
        return None;
    }
    Some(numeric(rounded.to_text()))
}

fn year(value: i128) -> Option<ast::Expr> {
    (1901..=2155).contains(&value).then(|| number(value))
}

fn set_default(members: &[String], text: &str) -> Option<String> {
    let chosen = if text.is_empty() {
        Vec::new()
    } else {
        text.split(',')
            .map(|part| {
                members
                    .iter()
                    .position(|member| member.to_lowercase() == part.to_lowercase())
            })
            .collect::<Option<Vec<_>>>()?
    };
    set_values(members)
        .zip(0_u32..)
        .find(|(_, mask)| {
            chosen.iter().all(|position| mask & (1 << position) != 0)
                && (0..members.len())
                    .all(|position| mask & (1 << position) == 0 || chosen.contains(&position))
        })
        .map(|(value, _)| value)
}

fn fit_characters(text: &str, length: u64, family: &Family) -> Option<String> {
    let trimmed = text.trim_end_matches(' ');
    let length = usize::try_from(length).ok()?;
    if trimmed.chars().count() > length {
        return None;
    }
    if matches!(family, Family::Char { .. }) {
        return Some(trimmed.to_string());
    }
    Some(text.chars().take(length).collect())
}

fn string(text: &str) -> ast::Expr {
    ast::Expr::Literal(ast::Literal::String(quote_string(text)))
}

fn numeric(text: String) -> ast::Expr {
    match text.strip_prefix('-') {
        Some(magnitude) => ast::Expr::Unary(
            ast::UnaryOperator::Negative,
            Box::new(ast::Expr::Literal(ast::Literal::Numeric(
                magnitude.to_string(),
            ))),
        ),
        None => ast::Expr::Literal(ast::Literal::Numeric(text)),
    }
}

/// A plain decimal number as MySQL writes it: an optional sign, whole digits
/// and fraction digits, without an exponent.
struct Decimal {
    negative: bool,
    whole: String,
    fraction: String,
}

impl Decimal {
    fn parse(text: &str) -> Option<Self> {
        let (negative, unsigned) = match text.as_bytes().first()? {
            b'-' => (true, &text[1..]),
            b'+' => (false, &text[1..]),
            _ => (false, text),
        };
        let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
        let digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if (whole.is_empty() && fraction.is_empty()) || !digits(whole) || !digits(fraction) {
            return None;
        }
        let whole = whole.trim_start_matches('0');
        Some(Decimal {
            negative,
            whole: if whole.is_empty() { "0" } else { whole }.to_string(),
            fraction: fraction.to_string(),
        })
    }

    fn is_zero(&self) -> bool {
        self.whole == "0" && self.fraction.bytes().all(|byte| byte == b'0')
    }

    fn exact_integer(&self) -> Option<i128> {
        if !self.fraction.bytes().all(|byte| byte == b'0') {
            return None;
        }
        self.rounded_integer()
    }

    fn rounded_integer(&self) -> Option<i128> {
        let rounded = self.rounded(0)?;
        let magnitude = rounded.whole.parse::<i128>().ok()?;
        Some(if rounded.negative {
            -magnitude
        } else {
            magnitude
        })
    }

    fn rounded(&self, scale: u64) -> Option<Decimal> {
        let scale = usize::try_from(scale).ok()?;
        let mut digits = format!("{}{}", self.whole, self.fraction);
        let kept_fraction = self.fraction.len().min(scale);
        let kept = self.whole.len() + kept_fraction;
        let round_up = digits
            .as_bytes()
            .get(kept)
            .is_some_and(|digit| *digit >= b'5');
        digits.truncate(kept);
        if round_up {
            digits = increment(&digits);
        }
        let whole_len = digits.len() - kept_fraction;
        let mut fraction = digits[whole_len..].to_string();
        fraction.extend(std::iter::repeat_n('0', scale - kept_fraction));
        let whole = digits[..whole_len].trim_start_matches('0');
        Some(Decimal {
            negative: self.negative,
            whole: if whole.is_empty() { "0" } else { whole }.to_string(),
            fraction,
        })
    }

    fn to_text(&self) -> String {
        let sign = if self.negative && !self.is_zero() {
            "-"
        } else {
            ""
        };
        if self.fraction.is_empty() {
            format!("{sign}{}", self.whole)
        } else {
            format!("{sign}{}.{}", self.whole, self.fraction)
        }
    }
}

fn increment(digits: &str) -> String {
    let mut bytes = digits.as_bytes().to_vec();
    for byte in bytes.iter_mut().rev() {
        if *byte == b'9' {
            *byte = b'0';
        } else {
            *byte += 1;
            return String::from_utf8(bytes).expect("digits are ASCII");
        }
    }
    format!("1{}", String::from_utf8(bytes).expect("digits are ASCII"))
}

struct Date {
    year: u32,
    month: u32,
    day: u32,
}

#[derive(Default)]
struct Time {
    negative: bool,
    hours: u32,
    minutes: u32,
    seconds: u32,
    fraction: String,
}

impl Date {
    fn to_text(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Time {
    fn to_text(&self, precision: u64) -> Option<String> {
        let precision = usize::try_from(precision).ok()?;
        if self.fraction.len() > precision {
            return None;
        }
        let sign = if self.negative { "-" } else { "" };
        let mut text = format!(
            "{sign}{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds
        );
        if precision > 0 {
            text.push('.');
            text.push_str(&self.fraction);
            text.extend(std::iter::repeat_n('0', precision - self.fraction.len()));
        }
        Some(text)
    }
}

fn parse_datetime(text: &str) -> Option<(Date, Option<Time>)> {
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(parse_time(time, 23)?)),
        None => (text, None),
    };
    if time.as_ref().is_some_and(|time| time.negative) {
        return None;
    }
    let mut parts = date.split('-');
    let year = parts.next().filter(|part| part.len() == 4)?.parse().ok()?;
    let month = small_number(parts.next()?)?;
    let day = small_number(parts.next()?)?;
    if parts.next().is_some() || year == 0 || !(1..=12).contains(&month) {
        return None;
    }
    (1..=days_in_month(year, month))
        .contains(&day)
        .then_some((Date { year, month, day }, time))
}

fn parse_time(text: &str, max_hours: u32) -> Option<Time> {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (clock, fraction) = text.split_once('.').unwrap_or((text, ""));
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut parts = clock.split(':');
    let hours = parts
        .next()
        .filter(|part| (1..=3).contains(&part.len()))?
        .parse::<u32>()
        .ok()?;
    let minutes = small_number(parts.next()?)?;
    let seconds = match parts.next() {
        Some(part) => small_number(part)?,
        None => 0,
    };
    if parts.next().is_some() || hours > max_hours || minutes > 59 || seconds > 59 {
        return None;
    }
    Some(Time {
        negative,
        hours,
        minutes,
        seconds,
        fraction: fraction.to_string(),
    })
}

fn small_number(part: &str) -> Option<u32> {
    if !(1..=2).contains(&part.len()) || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    part.parse().ok()
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

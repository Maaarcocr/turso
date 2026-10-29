use crate::ddl_expr::DefaultValue;
use crate::ddl_type::{number, ColumnType, Family};
use crate::names::quote_string;
use crate::MysqlParserError;
use turso_parser::ast;

const TIMESTAMP_MIN: &str = "1970-01-01 00:00:01";
const TIMESTAMP_MAX: &str = "2038-01-19 03:14:07.999999";
const MAX_TIME_HOURS: u32 = 838;

pub(crate) fn resolve_default(
    column_type: &ColumnType,
    value: DefaultValue,
    nullable: bool,
) -> Result<ast::Expr, MysqlParserError> {
    match value {
        DefaultValue::Null if nullable => Ok(ast::Expr::Literal(ast::Literal::Null)),
        DefaultValue::Null => Err(rejected()),
        DefaultValue::Expression(expr) => Ok(expr),
        DefaultValue::CurrentTimestamp(asked) => match column_type.family {
            Family::Datetime { precision } | Family::Timestamp { precision }
                if precision == asked =>
            {
                Ok(current_timestamp(precision))
            }
            _ => Err(rejected()),
        },
        DefaultValue::Number(text) => number_default(&column_type.family, &text),
        DefaultValue::Text(text) => text_default(&column_type.family, &text),
    }
}

/// The current time with `precision` fractional digits, as MySQL stores it
/// in a `DATETIME(precision)` or `TIMESTAMP(precision)` column.
fn current_timestamp(precision: u64) -> ast::Expr {
    if precision == 0 {
        return ast::Expr::Literal(ast::Literal::CurrentTimestamp);
    }
    ast::Expr::Parenthesized(vec![Box::new(ast::Expr::FunctionCall {
        name: ast::Name::exact("NOW".to_string()),
        distinctness: None,
        args: vec![Box::new(number(i128::from(precision)))],
        order_by: Vec::new(),
        within_group: Vec::new(),
        filter_over: ast::FunctionTail {
            filter_clause: None,
            over_clause: None,
        },
    })])
}

fn number_default(family: &Family, text: &str) -> Result<ast::Expr, MysqlParserError> {
    let decimal = Decimal::parse(text).ok_or_else(unsupported)?;
    match family {
        Family::Integer { min, max } => integer_in_range(&decimal, *min, *max),
        Family::Bit { width } => {
            let value = decimal.exact_integer().ok_or_else(unsupported)?;
            accept_if((0..1_i128 << width).contains(&value), || number(value))
        }
        Family::Decimal {
            precision,
            scale,
            unsigned,
        }
        | Family::Float {
            unsigned,
            digits: Some((precision, scale)),
        } => decimal_in_range(&decimal, *precision, *scale, *unsigned),
        Family::Float {
            unsigned,
            digits: None,
        } => accept_if(!*unsigned || !decimal.is_negative(), || {
            numeric(decimal.to_text())
        }),
        Family::Char { .. }
        | Family::Varchar { .. }
        | Family::Binary { .. }
        | Family::Varbinary { .. } => string_default(family, &decimal.to_text()),
        Family::Year => year(decimal.exact_integer().ok_or_else(unsupported)?),
        Family::Enum(_) | Family::Text | Family::Blob | Family::Json | Family::Spatial => {
            Err(rejected())
        }
        Family::Set(_)
        | Family::Date
        | Family::Datetime { .. }
        | Family::Timestamp { .. }
        | Family::Time { .. } => Err(unsupported()),
    }
}

fn text_default(family: &Family, text: &str) -> Result<ast::Expr, MysqlParserError> {
    match family {
        Family::Integer { .. } | Family::Decimal { .. } | Family::Float { .. } => {
            match Decimal::parse(text) {
                Some(_) => number_default(family, text),
                None if looks_like_a_number(text) => Err(unsupported()),
                None => Err(rejected()),
            }
        }
        Family::Char { .. }
        | Family::Varchar { .. }
        | Family::Binary { .. }
        | Family::Varbinary { .. } => string_default(family, text),
        Family::Enum(members) => members
            .iter()
            .find(|member| member.to_lowercase() == text.to_lowercase())
            .map(|member| string(member))
            .ok_or_else(rejected),
        Family::Set(members) => set_default(members, text)
            .map(|value| string(&value))
            .ok_or_else(rejected),
        Family::Date => temporal_text(TemporalColumn::Date, text).map(|text| string(&text)),
        Family::Datetime { precision } => {
            temporal_text(TemporalColumn::Datetime(*precision), text).map(|text| string(&text))
        }
        Family::Timestamp { precision } => {
            temporal_text(TemporalColumn::Timestamp(*precision), text).map(|text| string(&text))
        }
        Family::Time { precision } => {
            temporal_text(TemporalColumn::Time(*precision), text).map(|text| string(&text))
        }
        Family::Year => match text.parse::<i128>() {
            Ok(value) if text.len() == 4 => year(value),
            _ => Err(unsupported()),
        },
        Family::Bit { .. } => Err(unsupported()),
        Family::Text | Family::Blob | Family::Json | Family::Spatial => Err(rejected()),
    }
}

/// A column type whose values MySQL stores as canonical date and time text,
/// with its fractional digits.
#[derive(Clone, Copy, Debug)]
pub enum TemporalColumn {
    Date,
    Datetime(u64),
    Timestamp(u64),
    Time(u64),
}

/// The text MySQL stores for the string `text` in a column of type `column`
/// under the target profile's strict SQL mode: `YYYY-MM-DD`,
/// `YYYY-MM-DD HH:MM:SS[.fraction]` or `[-]HH:MM:SS[.fraction]`, with the
/// column's fractional digits, rounded as MySQL rounds them. A value MySQL
/// rejects (an invalid or zero date, a time out of range, a `TIMESTAMP`
/// outside 1970-2038) is [`MysqlParserError::Rejected`]; one whose
/// conversion is not available (a time in a `DATE`, a negative zero time, a
/// rounding past year 9999 or `838:59:59`, a form other than numbers
/// separated by `-`, `:`, a space or `T`) is [`MysqlParserError::Unsupported`].
pub fn temporal_text(column: TemporalColumn, text: &str) -> Result<String, MysqlParserError> {
    let datetime = |precision| -> Result<String, MysqlParserError> {
        let (date, time) = parse_datetime(text)?;
        let mut time = time.unwrap_or_default().rounded(precision)?;
        let date = if time.hours == 24 {
            time.hours = 0;
            date.next_day()?
        } else {
            date
        };
        Ok(format!("{} {}", date.to_text(), time.to_text()))
    };
    match column {
        TemporalColumn::Date => {
            let (date, time) = parse_datetime(text)?;
            if time.is_some_and(|time| !time.is_zero()) {
                return Err(unsupported());
            }
            Ok(date.to_text())
        }
        TemporalColumn::Datetime(precision) => datetime(precision),
        TemporalColumn::Timestamp(precision) => {
            let value = datetime(precision)?;
            if !(TIMESTAMP_MIN..=TIMESTAMP_MAX).contains(&value.as_str()) {
                return Err(rejected());
            }
            Ok(value)
        }
        TemporalColumn::Time(precision) => {
            let time = parse_time(text, MAX_TIME_HOURS)?.rounded(precision)?;
            if time.hours > MAX_TIME_HOURS || (time.negative && time.is_zero()) {
                return Err(unsupported());
            }
            Ok(time.to_text())
        }
    }
}

fn integer_in_range(
    decimal: &Decimal,
    min: i128,
    max: i128,
) -> Result<ast::Expr, MysqlParserError> {
    let value = decimal.rounded_integer().ok_or_else(rejected)?;
    accept_if((min..=max).contains(&value), || number(value))
}

fn decimal_in_range(
    decimal: &Decimal,
    precision: u64,
    scale: u64,
    unsigned: bool,
) -> Result<ast::Expr, MysqlParserError> {
    let rounded = decimal.rounded(scale).ok_or_else(unsupported)?;
    let whole_digits = rounded.whole.trim_start_matches('0').len() as u64;
    let fits = whole_digits <= precision - scale && !(unsigned && rounded.is_negative());
    accept_if(fits, || numeric(rounded.to_text()))
}

fn string_default(family: &Family, text: &str) -> Result<ast::Expr, MysqlParserError> {
    let fitted = match family {
        Family::Char { length } | Family::Varchar { length } => {
            fit_characters(text, *length, family)
        }
        Family::Binary { length } | Family::Varbinary { length } => {
            (text.len() as u64 <= *length).then(|| text.to_string())
        }
        _ => None,
    };
    fitted.map(|text| string(&text)).ok_or_else(rejected)
}

fn year(value: i128) -> Result<ast::Expr, MysqlParserError> {
    if (1901..=2155).contains(&value) {
        return Ok(number(value));
    }
    Err(unsupported())
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
    Some(
        members
            .iter()
            .enumerate()
            .filter(|(position, _)| chosen.contains(position))
            .map(|(_, member)| member.as_str())
            .collect::<Vec<_>>()
            .join(","),
    )
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

fn looks_like_a_number(text: &str) -> bool {
    text.trim().parse::<f64>().is_ok()
}

fn accept_if(
    valid: bool,
    value: impl FnOnce() -> ast::Expr,
) -> Result<ast::Expr, MysqlParserError> {
    if valid {
        Ok(value())
    } else {
        Err(rejected())
    }
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

fn rejected() -> MysqlParserError {
    MysqlParserError::Rejected("invalid default value for the column type".to_string())
}

fn unsupported() -> MysqlParserError {
    MysqlParserError::Unsupported(
        "the DEFAULT value needs a MySQL conversion that is not available".to_string(),
    )
}

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

    fn is_negative(&self) -> bool {
        self.negative && !self.is_zero()
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
        let sign = if self.is_negative() { "-" } else { "" };
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

#[derive(Clone, Copy)]
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

    fn next_day(self) -> Result<Date, MysqlParserError> {
        Ok(if self.day < days_in_month(self.year, self.month) {
            Date {
                day: self.day + 1,
                ..self
            }
        } else if self.month < 12 {
            Date {
                month: self.month + 1,
                day: 1,
                ..self
            }
        } else if self.year < 9999 {
            Date {
                year: self.year + 1,
                month: 1,
                day: 1,
            }
        } else {
            return Err(unsupported());
        })
    }
}

impl Time {
    fn is_zero(&self) -> bool {
        self.hours == 0
            && self.minutes == 0
            && self.seconds == 0
            && self.fraction.bytes().all(|digit| digit == b'0')
    }

    /// The time with `precision` fractional digits, rounded as MySQL rounds
    /// it: to microseconds by the seventh digit, then half up. The hours of
    /// a time of day can reach 24.
    fn rounded(&self, precision: u64) -> Result<Time, MysqlParserError> {
        let precision = u32::try_from(precision)
            .ok()
            .filter(|precision| *precision <= 6)
            .ok_or_else(unsupported)?;
        let microseconds = format!("{:0<6}", &self.fraction[..self.fraction.len().min(6)]);
        let mut total = (u64::from(self.hours) * 3600
            + u64::from(self.minutes) * 60
            + u64::from(self.seconds))
            * 1_000_000
            + microseconds.parse::<u64>().expect("the fraction is digits");
        if self.fraction.as_bytes().get(6).is_some_and(|digit| *digit >= b'5') {
            total += 1;
        }
        let unit = 10u64.pow(6 - precision);
        total = (total + unit / 2) / unit * unit;
        let seconds = total / 1_000_000;
        let part = |value: u64| u32::try_from(value).map_err(|_| unsupported());
        Ok(Time {
            negative: self.negative,
            hours: part(seconds / 3600)?,
            minutes: part(seconds / 60 % 60)?,
            seconds: part(seconds % 60)?,
            fraction: format!("{:06}", total % 1_000_000)[..precision as usize].to_string(),
        })
    }

    fn to_text(&self) -> String {
        let sign = if self.negative { "-" } else { "" };
        let mut text = format!(
            "{sign}{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds
        );
        if !self.fraction.is_empty() {
            text.push('.');
            text.push_str(&self.fraction);
        }
        text
    }
}

fn parse_datetime(text: &str) -> Result<(Date, Option<Time>), MysqlParserError> {
    let (date, time) = match text.split_once([' ', 'T']) {
        Some((date, time)) => (date, Some(parse_time(time, 23)?)),
        None => (text, None),
    };
    if time.as_ref().is_some_and(|time| time.negative) {
        return Err(unsupported());
    }
    let [year, month, day] = date.split('-').collect::<Vec<_>>()[..] else {
        return Err(unsupported());
    };
    if year.len() != 4 || !year.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported());
    }
    let year = year.parse().map_err(|_| unsupported())?;
    let month = small_number(month)?;
    let day = small_number(day)?;
    let valid =
        year != 0 && (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day);
    if !valid {
        return Err(rejected());
    }
    Ok((Date { year, month, day }, time))
}

fn parse_time(text: &str, max_hours: u32) -> Result<Time, MysqlParserError> {
    let (negative, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (clock, fraction) = text.split_once('.').unwrap_or((text, ""));
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported());
    }
    let (hours, minutes, seconds) = match clock.split(':').collect::<Vec<_>>()[..] {
        [hours, minutes] => (hours, minutes, "0"),
        [hours, minutes, seconds] => (hours, minutes, seconds),
        _ => return Err(unsupported()),
    };
    if !(1..=3).contains(&hours.len()) || !hours.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported());
    }
    let hours = hours.parse::<u32>().map_err(|_| unsupported())?;
    let minutes = small_number(minutes)?;
    let seconds = small_number(seconds)?;
    if hours > max_hours || minutes > 59 || seconds > 59 {
        return Err(rejected());
    }
    Ok(Time {
        negative,
        hours,
        minutes,
        seconds,
        fraction: fraction.to_string(),
    })
}

fn small_number(part: &str) -> Result<u32, MysqlParserError> {
    if !(1..=2).contains(&part.len()) || !part.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(unsupported());
    }
    part.parse().map_err(|_| unsupported())
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

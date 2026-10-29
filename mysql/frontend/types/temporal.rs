use super::unsupported_value;
use turso_core::dialect::{DialectType, StorageType};
use turso_core::types::TextSubtype;
use turso_core::{LimboError, Numeric, Result, Value};
use turso_mysql_parser::datetime::parse::{parse_datetime, DateFlags};
use turso_mysql_parser::{temporal_text, MysqlParserError, TemporalColumn};

macro_rules! compared {
    ($left:expr, $right:expr, $function:literal) => {
        concat!(
            "OPERATOR '<' (",
            $left,
            ", ",
            $right,
            ") ",
            $function,
            "less ",
            "OPERATOR '=' (",
            $left,
            ", ",
            $right,
            ") ",
            $function,
            "equal ",
        )
    };
}

macro_rules! temporal {
    ($name:literal, $parameters:literal, $assign:literal, $operators:expr) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: $parameters,
            behaviour: concat!(
                "ASSIGN ",
                $assign,
                " ",
                "CAST __mysql_sqlite_cast(value, 'TEXT') ",
                $operators,
                "FUNCTION __mysql_cast_as_signed(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported ",
                "FUNCTION __mysql_cast_as_unsigned(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported"
            ),
        }
    };
}

/// The comparisons of a `DATE`, `DATETIME` or `TIMESTAMP` value: with its own
/// type, its canonical text; with the other two, a string or anything else,
/// the datetimes MySQL reads them as; with a `TIME`, refused.
macro_rules! dated {
    ($quoted:literal, $first:literal, $second:literal) => {
        concat!(
            "OPERATOR '<' (",
            $quoted,
            ", ",
            $quoted,
            ") OPERATOR '=' (",
            $quoted,
            ", ",
            $quoted,
            ") ",
            compared!($quoted, $first, "__mysql_dated_"),
            compared!($quoted, $second, "__mysql_dated_"),
            compared!($quoted, "\"time\"", "__mysql_dated_time_"),
            compared!("\"time\"", $quoted, "__mysql_dated_time_"),
            compared!($quoted, "any", "__mysql_dated_"),
            compared!("any", $quoted, "__mysql_dated_"),
        )
    };
}

pub(super) const TYPES: [DialectType; 4] = [
    temporal!(
        "date",
        "",
        "__mysql_assign_date(value)",
        dated!("\"date\"", "\"datetime\"", "\"timestamp\"")
    ),
    temporal!(
        "datetime",
        "precision integer DEFAULT 0",
        "__mysql_assign_datetime(value, precision)",
        dated!("\"datetime\"", "\"date\"", "\"timestamp\"")
    ),
    temporal!(
        "timestamp",
        "precision integer DEFAULT 0",
        "__mysql_assign_timestamp(value, precision)",
        dated!("\"timestamp\"", "\"date\"", "\"datetime\"")
    ),
    temporal!(
        "time",
        "precision integer DEFAULT 0",
        "__mysql_assign_time(value, precision)",
        ""
    ),
];

/// The text MySQL stores for a value assigned to a `DATE` column; see
/// [`assign`].
pub(super) fn assign_date(args: &[Value]) -> Result<Value> {
    assign(&args[0], TemporalColumn::Date, "date")
}

pub(super) fn assign_datetime(args: &[Value]) -> Result<Value> {
    assign(
        &args[0],
        TemporalColumn::Datetime(precision(&args[1])?),
        "datetime",
    )
}

pub(super) fn assign_timestamp(args: &[Value]) -> Result<Value> {
    assign(
        &args[0],
        TemporalColumn::Timestamp(precision(&args[1])?),
        "datetime",
    )
}

pub(super) fn assign_time(args: &[Value]) -> Result<Value> {
    assign(&args[0], TemporalColumn::Time(precision(&args[1])?), "time")
}

/// A string assigned to a temporal column becomes the canonical text MySQL
/// stores (`2024-3-5 9:4:5` is `2024-03-05 09:04:05`, padded to the column's
/// fractional digits). One MySQL rejects fails as its error 1292 does; a
/// number, a binary string, a JSON value, and a string whose conversion is not
/// available yet are refused.
fn assign(value: &Value, column: TemporalColumn, type_name: &str) -> Result<Value> {
    let text = match value {
        Value::Null => return Ok(Value::Null),
        Value::Text(text) if text.subtype != TextSubtype::Json => text.as_str(),
        _ => {
            return Err(unsupported_value(format!(
                "`{value}` assigned to a {} column",
                type_name.to_ascii_uppercase()
            )))
        }
    };
    match temporal_text(column, text) {
        Ok(canonical) => Ok(Value::build_text(canonical)),
        Err(MysqlParserError::Rejected(_)) => Err(LimboError::InvalidArgument(format!(
            "Incorrect {type_name} value: '{text}'"
        ))),
        Err(_) => Err(unsupported_value(format!(
            "`{text}` assigned to a {} column, which needs a conversion that",
            type_name.to_ascii_uppercase()
        ))),
    }
}

fn precision(value: &Value) -> Result<u64> {
    match value {
        Value::Numeric(Numeric::Integer(precision @ 0..=6)) => Ok(precision.unsigned_abs()),
        other => Err(LimboError::InternalError(format!(
            "a temporal column has 0 to 6 fractional digits, not {other}"
        ))),
    }
}

/// Whether the left operand is the earlier datetime, reading both as MySQL
/// reads the operands of a comparison with a `DATE`, `DATETIME` or
/// `TIMESTAMP` value.
pub(super) fn dated_less(args: &[Value]) -> Result<Value> {
    dated_comparison(args, |left, right| left < right)
}

pub(super) fn dated_equal(args: &[Value]) -> Result<Value> {
    dated_comparison(args, |left, right| left == right)
}

pub(super) fn dated_time_comparison(_args: &[Value]) -> Result<Value> {
    Err(unsupported_value(
        "a TIME compared with a DATE, DATETIME or TIMESTAMP, which MySQL dates with the current day,"
            .to_string(),
    ))
}

fn dated_comparison(args: &[Value], holds: fn(i128, i128) -> bool) -> Result<Value> {
    let [left, right] = args else {
        unreachable!("a comparison has two operands");
    };
    Ok(match (micros(left)?, micros(right)?) {
        (Some(left), Some(right)) => Value::from_i64(i64::from(holds(left, right))),
        _ => Value::Null,
    })
}

/// The microseconds since year 0 of the datetime MySQL reads a string as
/// next to a `DATE`, `DATETIME` or `TIMESTAMP` value: one its datetime
/// parser rejects fails (1525); a zero date, or a date with a zero month or
/// day, and numbers, binary strings and JSON values are refused.
fn micros(value: &Value) -> Result<Option<i128>> {
    let text = match value {
        Value::Null => return Ok(None),
        Value::Text(text) if text.subtype != TextSubtype::Json => text.as_str(),
        _ => {
            return Err(unsupported_value(format!(
                "`{value}` compared with a DATE, DATETIME or TIMESTAMP"
            )))
        }
    };
    let parsed = parse_datetime(text.as_bytes(), DateFlags::FUZZY)
        .map_err(|_| LimboError::InvalidArgument(format!("Incorrect DATETIME value: '{text}'")))?;
    let mut time = parsed.time;
    if time.month == 0 || time.day == 0 || !time.add_nanos_to_datetime(parsed.rounding_nanos) {
        return Err(unsupported_value(format!(
            "the zero date `{text}` compared with a DATE, DATETIME or TIMESTAMP"
        )));
    }
    Ok(Some(time.datetime_in_micros()))
}

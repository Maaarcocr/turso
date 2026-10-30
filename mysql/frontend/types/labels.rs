use super::{data_truncated, unsupported_value};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! numbers {
    ($quoted:expr) => {
        concat!(
            "OPERATOR '-' (",
            $quoted,
            ") RETURNS integer __mysql_labels_number_unsupported ",
            arithmetic!($quoted, $quoted, "+"),
            arithmetic!($quoted, "any", "+"),
            arithmetic!("any", $quoted, "+"),
            arithmetic!($quoted, $quoted, "-"),
            arithmetic!($quoted, "any", "-"),
            arithmetic!("any", $quoted, "-"),
            arithmetic!($quoted, $quoted, "*"),
            arithmetic!($quoted, "any", "*"),
            arithmetic!("any", $quoted, "*"),
            arithmetic!($quoted, $quoted, "/"),
            arithmetic!($quoted, "any", "/"),
            arithmetic!("any", $quoted, "/"),
            arithmetic!($quoted, $quoted, "%"),
            arithmetic!($quoted, "any", "%"),
            arithmetic!("any", $quoted, "%"),
            compared!($quoted, $quoted),
            compared!($quoted, "any"),
            compared!("any", $quoted),
        )
    };
}

macro_rules! arithmetic {
    ($left:expr, $right:expr, $operator:literal) => {
        concat!(
            "OPERATOR '",
            $operator,
            "' (",
            $left,
            ", ",
            $right,
            ") RETURNS integer __mysql_labels_number_unsupported "
        )
    };
}

macro_rules! compared {
    ($left:expr, $right:expr) => {
        concat!(
            "OPERATOR '<' (",
            $left,
            ", ",
            $right,
            ") __mysql_labels_less ",
            "OPERATOR '=' (",
            $left,
            ", ",
            $right,
            ") __mysql_labels_equal ",
        )
    };
}

macro_rules! labels {
    ($name:literal, $storage:expr, $assign:literal) => {
        DialectType {
            name: $name,
            storage: $storage,
            parameters: "",
            behaviour: concat!(
                "ASSIGN ",
                $assign,
                "(value, labels) ",
                numbers!(concat!("\"", $name, "\"")),
                "CAST __mysql_sqlite_cast(value, 'TEXT') ",
                "FUNCTION __mysql_cast_as_signed(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported ",
                "FUNCTION __mysql_cast_as_unsigned(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported ",
                "FUNCTION __mysql_cast_as_unsigned_bits(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_integer_unsupported ",
                "FUNCTION __mysql_numeric_cast_operand(\"",
                $name,
                "\") RETURNS integer __mysql_cast_as_number_unsupported"
            ),
        }
    };
}

/// `ENUM` and `SET`, stored as the member position and the bitmask so that
/// they sort as in MySQL, and their variants that match member names with
/// their letter case, as under a case-sensitive collation.
pub(super) const TYPES: [DialectType; 4] = [
    labels!("enum", StorageType::Enumeration, "__mysql_assign_enum"),
    labels!(
        "enum_exact",
        StorageType::Enumeration,
        "__mysql_assign_enum_exact"
    ),
    labels!("set", StorageType::LabelSet, "__mysql_assign_set"),
    labels!(
        "set_exact",
        StorageType::LabelSet,
        "__mysql_assign_set_exact"
    ),
];

/// A number made of an `ENUM` or `SET` value, which MySQL reads as its member
/// position or bitmask where Turso has its text.
pub(super) fn number_unsupported(_args: &[Value]) -> Result<Value> {
    Err(unsupported_value(
        "an ENUM or SET value in arithmetic or compared with a number".to_string(),
    ))
}

/// Whether `ENUM` or `SET` text compares below the other operand, byte by
/// byte; a number is refused, as MySQL compares it with the member position
/// or bitmask.
pub(super) fn less(args: &[Value]) -> Result<Value> {
    compared(args, |left, right| left < right)
}

pub(super) fn equal(args: &[Value]) -> Result<Value> {
    compared(args, |left, right| left == right)
}

fn compared(args: &[Value], holds: fn(&[u8], &[u8]) -> bool) -> Result<Value> {
    fn bytes(value: &Value) -> Result<Option<&[u8]>> {
        match value {
            Value::Null => Ok(None),
            Value::Text(text) => Ok(Some(text.as_str().as_bytes())),
            Value::Blob(bytes) => Ok(Some(bytes)),
            Value::Numeric(_) => number_unsupported(&[]).map(|_| None),
        }
    }
    let [left, right] = args else {
        unreachable!("a comparison has two operands");
    };
    Ok(match (bytes(left)?, bytes(right)?) {
        (Some(left), Some(right)) => Value::from_i64(i64::from(holds(left, right))),
        _ => Value::Null,
    })
}

/// The member MySQL stores for a value assigned to an `ENUM` column: a
/// member named by the text, compared without ASCII case and trailing
/// spaces (or with letter case under a case-sensitive collation, `exact`),
/// or the member at an integer position, also written as a string of
/// at most five digits that names no member.
pub(super) fn assign_enum(args: &[Value]) -> Result<Value> {
    member_of_enum(args, false)
}

pub(super) fn assign_enum_exact(args: &[Value]) -> Result<Value> {
    member_of_enum(args, true)
}

fn member_of_enum(args: &[Value], exact: bool) -> Result<Value> {
    let labels = labels(&args[1])?;
    let position = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(position)) => *position,
        Value::Numeric(Numeric::Float(_)) => {
            return Err(unsupported_value(
                "a DOUBLE or DECIMAL assigned to an ENUM column".to_string(),
            ))
        }
        Value::Text(_) | Value::Blob(_) => {
            let text = text_of(&args[0])?;
            if let Some(member) = member(&labels, &text, exact)? {
                return Ok(Value::build_text(member.to_string()));
            }
            match short_number(&text) {
                Some(position) => position,
                None => return Err(data_truncated()),
            }
        }
    };
    usize::try_from(position)
        .ok()
        .and_then(|position| position.checked_sub(1))
        .and_then(|index| labels.get(index))
        .map(|member| Value::build_text(member.clone()))
        .ok_or_else(data_truncated)
}

/// The members MySQL stores for a value assigned to a `SET` column, listed
/// in definition order: those named by the comma-separated text, compared
/// like `ENUM` members, or those of the bits of an integer, also written as
/// a string of digits when the text names no members. Turso passes numbers
/// as text, so a DOUBLE or DECIMAL reads as text naming no member.
pub(super) fn assign_set(args: &[Value]) -> Result<Value> {
    members_of_set(args, false)
}

pub(super) fn assign_set_exact(args: &[Value]) -> Result<Value> {
    members_of_set(args, true)
}

fn members_of_set(args: &[Value], exact: bool) -> Result<Value> {
    let labels = labels(&args[1])?;
    let mask = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(mask)) => {
            u64::try_from(*mask).map_err(|_| data_truncated())?
        }
        Value::Numeric(Numeric::Float(_)) => {
            return Err(unsupported_value(
                "a DOUBLE or DECIMAL assigned to a SET column".to_string(),
            ))
        }
        Value::Text(_) | Value::Blob(_) => {
            let text = text_of(&args[0])?;
            match named_mask(&labels, &text, exact)? {
                Some(mask) => mask,
                None if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) => {
                    text.parse().map_err(|_| data_truncated())?
                }
                None => return Err(data_truncated()),
            }
        }
    };
    if labels.len() < 64 && mask >> labels.len() != 0 {
        return Err(data_truncated());
    }
    let members: Vec<&str> = labels
        .iter()
        .enumerate()
        .filter(|(index, _)| mask & (1 << index) != 0)
        .map(|(_, member)| member.as_str())
        .collect();
    Ok(Value::build_text(members.join(",")))
}

fn named_mask(labels: &[String], text: &str, exact: bool) -> Result<Option<u64>> {
    if text.is_empty() {
        return Ok(Some(0));
    }
    let mut mask = 0;
    for name in text.split(',') {
        let Some(member) = member(labels, name, exact)? else {
            return Ok(None);
        };
        let index = labels
            .iter()
            .position(|label| label == member)
            .expect("the member is a label");
        mask |= 1 << index;
    }
    Ok(Some(mask))
}

/// The member `text` names. Names other than ASCII ones that differ from
/// every member are refused, as MySQL compares them under the column's
/// collation, which may match accents and letter cases beyond ASCII.
fn member<'a>(labels: &'a [String], text: &str, exact: bool) -> Result<Option<&'a str>> {
    let name = text.trim_end_matches(' ');
    if let Some(member) = labels.iter().find(|label| label.as_str() == name) {
        return Ok(Some(member));
    }
    if exact {
        return Ok(None);
    }
    if !name.is_ascii() || labels.iter().any(|label| !label.is_ascii()) {
        return Err(unsupported_value(format!(
            "`{text}` as a member name where a member or the name is not ASCII"
        )));
    }
    Ok(labels
        .iter()
        .find(|label| label.eq_ignore_ascii_case(name))
        .map(String::as_str))
}

fn short_number(text: &str) -> Option<i64> {
    (!text.is_empty() && text.len() < 6 && text.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| text.parse().expect("at most five digits"))
}

fn text_of(value: &Value) -> Result<String> {
    match value {
        Value::Text(text) => Ok(text.as_str().to_string()),
        Value::Blob(bytes) => String::from_utf8(bytes.clone()).map_err(|_| data_truncated()),
        other => unreachable!("only text and blobs are read as names, got {other}"),
    }
}

/// The labels of the column, as the quoted, comma-separated list of its type parameter.
fn labels(list: &Value) -> Result<Vec<String>> {
    let bad = || LimboError::InternalError(format!("bad label list {list}"));
    let Value::Text(list) = list else {
        return Err(bad());
    };
    let mut labels = Vec::new();
    let mut chars = list.as_str().chars().peekable();
    loop {
        if chars.next() != Some('\'') {
            return Err(bad());
        }
        let mut label = String::new();
        loop {
            match chars.next() {
                Some('\'') if chars.peek() == Some(&'\'') => {
                    chars.next();
                    label.push('\'');
                }
                Some('\'') => break,
                Some(character) => label.push(character),
                None => return Err(bad()),
            }
        }
        labels.push(label);
        match chars.next() {
            None => return Ok(labels),
            Some(',') => {}
            Some(_) => return Err(bad()),
        }
    }
}

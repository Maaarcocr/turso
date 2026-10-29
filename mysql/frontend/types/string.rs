use super::{data_too_long, integer_parameter, invalid_value};
use std::borrow::Cow;
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! text {
    ($name:literal, $max_bytes:tt) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: "",
            behaviour: concat!(
                "ASSIGN __mysql_assign_text(value, ",
                stringify!($max_bytes),
                ") CAST __mysql_cast_text(value)"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 6] = [
    DialectType {
        name: "char",
        storage: StorageType::Text,
        parameters: "length integer",
        behaviour: "ASSIGN __mysql_assign_char(value, length) \
            CAST __mysql_cast_text(value)",
    },
    DialectType {
        name: "varchar",
        storage: StorageType::Text,
        parameters: "length integer",
        behaviour: "ASSIGN __mysql_assign_varchar(value, length) \
            CAST __mysql_cast_text(value)",
    },
    text!("tinytext", 255),
    text!("text", 65535),
    text!("mediumtext", 16777215),
    text!("longtext", NULL),
];

pub(super) fn assign_char(args: &[Value]) -> Result<Value> {
    let Some(text) = text_of(&args[0])? else {
        return Ok(Value::Null);
    };
    let length = length_parameter(&args[1])?;
    let fitted = fit_characters(&text, length)?;
    Ok(Value::build_text(fitted.trim_end_matches(' ').to_string()))
}

pub(super) fn assign_varchar(args: &[Value]) -> Result<Value> {
    let Some(text) = text_of(&args[0])? else {
        return Ok(Value::Null);
    };
    let length = length_parameter(&args[1])?;
    Ok(Value::build_text(
        fit_characters(&text, length)?.to_string(),
    ))
}

pub(super) fn assign_text(args: &[Value]) -> Result<Value> {
    let Some(text) = text_of(&args[0])? else {
        return Ok(Value::Null);
    };
    let fitted = match integer_parameter(&args[1]) {
        Some(max_bytes) => fit_bytes(&text, max_bytes)?,
        None => &text,
    };
    Ok(Value::build_text(fitted.to_string()))
}

fn text_of(value: &Value) -> Result<Option<Cow<'_, str>>> {
    Ok(Some(match value {
        Value::Null => return Ok(None),
        Value::Text(text) => Cow::Borrowed(text.as_str()),
        Value::Numeric(Numeric::Integer(number)) => Cow::Owned(number.to_string()),
        Value::Numeric(Numeric::Float(double)) => {
            Cow::Owned(crate::functions::format_double(f64::from(*double)))
        }
        Value::Blob(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => return Err(incorrect_string_value(bytes)),
        },
    }))
}

fn length_parameter(value: &Value) -> Result<u64> {
    integer_parameter(value).ok_or_else(|| {
        LimboError::InternalError(format!("a string length must be an integer, got {value}"))
    })
}

fn fit_characters(text: &str, length: u64) -> Result<&str> {
    let Ok(length) = usize::try_from(length) else {
        return Ok(text);
    };
    match text.char_indices().nth(length) {
        None => Ok(text),
        Some((end, _)) => keep_if_only_spaces_follow(text, end),
    }
}

fn fit_bytes(text: &str, max_bytes: u64) -> Result<&str> {
    let Ok(max_bytes) = usize::try_from(max_bytes) else {
        return Ok(text);
    };
    if text.len() <= max_bytes {
        return Ok(text);
    }
    let end = (0..=max_bytes)
        .rev()
        .find(|end| text.is_char_boundary(*end))
        .unwrap_or(0);
    keep_if_only_spaces_follow(text, end)
}

fn keep_if_only_spaces_follow(text: &str, end: usize) -> Result<&str> {
    let (kept, cut) = text.split_at(end);
    if cut.bytes().all(|byte| byte == b' ') {
        Ok(kept)
    } else {
        Err(data_too_long())
    }
}

fn incorrect_string_value(bytes: &[u8]) -> LimboError {
    let shown: String = bytes
        .iter()
        .take(6)
        .map(|byte| match byte {
            0x20..=0x7E => (*byte as char).to_string(),
            _ => format!("\\x{byte:02X}"),
        })
        .collect();
    invalid_value(format!("Incorrect string value: '{shown}'"))
}

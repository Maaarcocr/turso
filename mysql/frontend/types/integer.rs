use super::number_text::{self, Reading};
use super::{binary_string_in_number_column, data_truncated, invalid_value, out_of_range};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! ranged {
    ($name:literal, $min:expr, $max:expr, $assign:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::RangedInteger {
                min: $min,
                max: $max,
            },
            parameters: "width integer DEFAULT NULL",
            behaviour: concat!(
                "CAST TO integer AS IMPLICIT ",
                "ASSIGN ",
                $assign,
                "(value) ",
                "CAST __mysql_sqlite_cast(value, 'INTEGER') ",
                "OPERATOR '<' (\"",
                $name,
                "\", integer) ",
                "OPERATOR '=' (\"",
                $name,
                "\", integer) ",
                "OPERATOR '<' (any, \"",
                $name,
                "\") ",
                "OPERATOR '=' (any, \"",
                $name,
                "\") ",
                "FUNCTION sum(\"",
                $name,
                "\") RETURNS decimal numeric_sum"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 10] = [
    ranged!("tinyint", -128, 127, "__mysql_assign_integer"),
    ranged!(
        "tinyint unsigned",
        0,
        255,
        "__mysql_assign_unsigned_integer"
    ),
    ranged!("smallint", -32_768, 32_767, "__mysql_assign_integer"),
    ranged!(
        "smallint unsigned",
        0,
        65_535,
        "__mysql_assign_unsigned_integer"
    ),
    ranged!("mediumint", -8_388_608, 8_388_607, "__mysql_assign_integer"),
    ranged!(
        "mediumint unsigned",
        0,
        16_777_215,
        "__mysql_assign_unsigned_integer"
    ),
    ranged!(
        "int",
        i32::MIN as i64,
        i32::MAX as i64,
        "__mysql_assign_integer"
    ),
    ranged!(
        "int unsigned",
        0,
        u32::MAX as i64,
        "__mysql_assign_unsigned_integer"
    ),
    ranged!("bigint", i64::MIN, i64::MAX, "__mysql_assign_integer"),
    DialectType {
        name: "bigint unsigned",
        storage: StorageType::Uint64,
        parameters: "width integer DEFAULT NULL",
        behaviour: "ASSIGN __mysql_assign_unsigned_bigint(value) \
            CAST __mysql_sqlite_cast(value, 'INTEGER') \
            OPERATOR '<' (\"bigint unsigned\", \"bigint unsigned\") uint64_lt \
            OPERATOR '<' (\"bigint unsigned\", integer) uint64_lt \
            OPERATOR '=' (\"bigint unsigned\", \"bigint unsigned\") uint64_eq \
            OPERATOR '=' (\"bigint unsigned\", real) __mysql_unsigned_equals_value \
            OPERATOR '=' (\"bigint unsigned\", char) __mysql_unsigned_equals_value \
            OPERATOR '=' (\"bigint unsigned\", varchar) __mysql_unsigned_equals_value \
            OPERATOR '=' (\"bigint unsigned\", tinytext) __mysql_unsigned_equals_value \
            OPERATOR '=' (\"bigint unsigned\", mediumtext) __mysql_unsigned_equals_value \
            OPERATOR '=' (\"bigint unsigned\", longtext) __mysql_unsigned_equals_value \
            OPERATOR '=' (any, \"bigint unsigned\") __mysql_value_equals_unsigned \
            FUNCTION sum(\"bigint unsigned\") RETURNS decimal numeric_sum",
    },
];

pub(super) fn assign_integer(args: &[Value]) -> Result<Value> {
    let magnitude = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(_)) => return Ok(args[0].clone()),
        Value::Numeric(Numeric::Float(double)) => {
            return rounded_double(f64::from(*double)).map(Value::from_i64);
        }
        Value::Text(text) => read_integer(text.as_str().as_bytes())?,
        Value::Blob(_) => return Err(binary_string_in_number_column("integer")),
    };
    i64::try_from(magnitude)
        .map(Value::from_i64)
        .map_err(|_| out_of_range())
}

pub(super) fn assign_unsigned_integer(args: &[Value]) -> Result<Value> {
    if let Value::Numeric(Numeric::Float(double)) = &args[0] {
        if f64::from(*double) < 0.0 {
            return Err(out_of_range());
        }
    }
    assign_integer(args)
}

pub(super) fn assign_unsigned_bigint(args: &[Value]) -> Result<Value> {
    let magnitude = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => {
            return u64::try_from(*number)
                .map(|_| args[0].clone())
                .map_err(|_| out_of_range());
        }
        Value::Numeric(Numeric::Float(double)) => {
            let rounded = f64::from(*double).round();
            if f64::from(*double) < 0.0 || !(0.0..18_446_744_073_709_551_616.0).contains(&rounded) {
                return Err(out_of_range());
            }
            if rounded >= 9_223_372_036_854_775_808.0 {
                return Err(invalid_value(format!(
                    "Incorrect integer value: {} is above 9223372036854775807 and arrives \
                    as a double, so it cannot be stored exactly; write it as text",
                    crate::functions::format_double(f64::from(*double))
                )));
            }
            i128::from(rounded as i64)
        }
        Value::Text(text) => read_integer(text.as_str().as_bytes())?,
        Value::Blob(_) => return Err(binary_string_in_number_column("integer")),
    };
    let unsigned = u64::try_from(magnitude).map_err(|_| out_of_range())?;
    Ok(match i64::try_from(unsigned) {
        Ok(signed) => Value::from_i64(signed),
        Err(_) => Value::build_text(unsigned.to_string()),
    })
}

pub(super) fn unsigned_equals_value(args: &[Value]) -> Result<Value> {
    let [unsigned, value] = args else {
        unreachable!("__mysql_unsigned_equals_value is resolved with two arguments");
    };
    unsigned_equals(unsigned, value)
}

pub(super) fn value_equals_unsigned(args: &[Value]) -> Result<Value> {
    let [value, unsigned] = args else {
        unreachable!("__mysql_value_equals_unsigned is resolved with two arguments");
    };
    unsigned_equals(unsigned, value)
}

fn unsigned_equals(unsigned: &Value, value: &Value) -> Result<Value> {
    let unsigned = match unsigned {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => u64::try_from(*number).ok(),
        Value::Text(text) => text.as_str().parse::<u64>().ok(),
        Value::Numeric(Numeric::Float(_)) | Value::Blob(_) => None,
    }
    .ok_or_else(|| {
        LimboError::InternalError(format!("a BIGINT UNSIGNED value read back as {unsigned}"))
    })?;
    let equal = match value {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => i128::from(unsigned) == i128::from(*number),
        Value::Numeric(Numeric::Float(double)) => unsigned as f64 == f64::from(*double),
        Value::Text(_) | Value::Blob(_) => match value.exec_cast("REAL")? {
            Value::Numeric(Numeric::Float(double)) => unsigned as f64 == f64::from(double),
            other => {
                return Err(LimboError::InternalError(format!(
                    "a REAL cast of {value} returned {other}, which is not a double"
                )));
            }
        },
    };
    Ok(Value::from_i64(i64::from(equal)))
}

pub(super) fn cast_as_signed(args: &[Value]) -> Result<Value> {
    signed_result(integer_bits(&args[0]))
}

pub(super) fn cast_as_unsigned(args: &[Value]) -> Result<Value> {
    unsigned_result(integer_bits(&args[0]))
}

pub(super) fn cast_as_integer_unsupported(_args: &[Value]) -> Result<Value> {
    Err(LimboError::InvalidArgument(
        "CAST of a DATE, DATETIME, TIMESTAMP, TIME, ENUM or SET value to SIGNED or UNSIGNED \
        is not supported yet"
            .to_string(),
    ))
}

pub(super) fn signed_result(bits: Option<u64>) -> Result<Value> {
    Ok(bits.map_or(Value::Null, |bits| Value::from_i64(bits as i64)))
}

pub(super) fn unsigned_result(bits: Option<u64>) -> Result<Value> {
    match bits {
        None => Ok(Value::Null),
        Some(bits) => i64::try_from(bits).map(Value::from_i64).map_err(|_| {
            LimboError::InvalidArgument(format!(
                "CAST to UNSIGNED of {bits}, which is above 9223372036854775807, \
                is not supported yet"
            ))
        }),
    }
}

fn integer_bits(value: &Value) -> Option<u64> {
    match value {
        Value::Null => None,
        Value::Numeric(Numeric::Integer(number)) => Some(*number as u64),
        Value::Numeric(Numeric::Float(double)) => Some(double_bits(f64::from(*double))),
        Value::Text(text) => Some(text_bits(text.as_str().as_bytes())),
        Value::Blob(bytes) => Some(text_bits(bytes)),
    }
}

pub(super) fn double_bits(value: f64) -> u64 {
    if value <= -9_223_372_036_854_775_808.0 {
        i64::MIN as u64
    } else if value >= 9_223_372_036_854_775_807.0 {
        i64::MAX as u64
    } else {
        value.round_ties_even() as i64 as u64
    }
}

pub(super) fn text_bits(bytes: &[u8]) -> u64 {
    let Some(number) = number_text::leading_number(bytes) else {
        return 0;
    };
    let (negative, whole, _, _) = number.parts();
    let magnitude = whole.iter().fold(0_u128, |value, digit| {
        (value * 10 + u128::from(digit - b'0')).min(u128::from(u64::MAX) + 1)
    });
    if negative {
        match i64::try_from(magnitude) {
            Ok(magnitude) => (-magnitude) as u64,
            Err(_) => i64::MIN as u64,
        }
    } else {
        u64::try_from(magnitude).unwrap_or(u64::MAX)
    }
}

fn rounded_double(value: f64) -> Result<i64> {
    let rounded = value.round();
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&rounded) {
        return Err(out_of_range());
    }
    Ok(rounded as i64)
}

fn read_integer(bytes: &[u8]) -> Result<i128> {
    let number = match number_text::read_as_integer(bytes) {
        Reading::Number(number) => number,
        Reading::NumberFollowedByText => return Err(data_truncated()),
        Reading::NoNumber => {
            return Err(invalid_value(format!(
                "Incorrect integer value: '{}'",
                String::from_utf8_lossy(bytes)
            )));
        }
    };
    let magnitude = number.rounded_magnitude().ok_or_else(out_of_range)?;
    let magnitude = i128::try_from(magnitude).map_err(|_| out_of_range())?;
    Ok(if number.is_negative() {
        -magnitude
    } else {
        magnitude
    })
}

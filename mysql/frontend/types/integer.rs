use super::number_text::{self, Reading};
use super::{binary_string_in_number_column, data_truncated, invalid_value, out_of_range};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{Numeric, Result, Value};

macro_rules! ranged {
    ($name:literal, $min:expr, $max:expr) => {
        DialectType {
            name: $name,
            storage: StorageType::RangedInteger {
                min: $min,
                max: $max,
            },
            parameters: "width integer DEFAULT NULL",
            behaviour: concat!(
                "CAST TO integer AS IMPLICIT ",
                "ASSIGN __mysql_assign_integer(value) ",
                "CAST __mysql_sqlite_cast(value, 'INTEGER') ",
                "OPERATOR '<' (\"",
                $name,
                "\", integer) ",
                "OPERATOR '=' (\"",
                $name,
                "\", integer)"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 10] = [
    ranged!("tinyint", -128, 127),
    ranged!("tinyint unsigned", 0, 255),
    ranged!("smallint", -32_768, 32_767),
    ranged!("smallint unsigned", 0, 65_535),
    ranged!("mediumint", -8_388_608, 8_388_607),
    ranged!("mediumint unsigned", 0, 16_777_215),
    ranged!("int", i32::MIN as i64, i32::MAX as i64),
    ranged!("int unsigned", 0, u32::MAX as i64),
    ranged!("bigint", i64::MIN, i64::MAX),
    DialectType {
        name: "bigint unsigned",
        storage: StorageType::Uint64,
        parameters: "width integer DEFAULT NULL",
        behaviour: "ASSIGN __mysql_assign_unsigned_bigint(value) \
            CAST __mysql_sqlite_cast(value, 'INTEGER') \
            OPERATOR '<' (\"bigint unsigned\", \"bigint unsigned\") uint64_lt \
            OPERATOR '<' (\"bigint unsigned\", integer) uint64_lt \
            OPERATOR '=' (\"bigint unsigned\", \"bigint unsigned\") uint64_eq \
            OPERATOR '=' (\"bigint unsigned\", integer) uint64_eq",
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
            if !(0.0..18_446_744_073_709_551_616.0).contains(&rounded) {
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

fn rounded_double(value: f64) -> Result<i64> {
    let rounded = value.round();
    if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&rounded) {
        return Err(out_of_range());
    }
    Ok(rounded as i64)
}

fn read_integer(bytes: &[u8]) -> Result<i128> {
    let number = match number_text::read(bytes) {
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

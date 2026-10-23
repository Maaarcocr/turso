use super::{float, integer_parameter, out_of_range};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! decimal {
    ($name:literal, $unsigned:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Text,
            parameters: "precision integer DEFAULT 10, scale integer DEFAULT 0",
            behaviour: concat!(
                "CAST FROM integer, real, char, varchar, tinytext, mediumtext, longtext AS IMPLICIT \
                ASSIGN __mysql_assign_decimal(value, ",
                $unsigned,
                ") ENCODE __mysql_decimal_text(value) \
                DECODE __mysql_sqlite_cast(value, 'NUMERIC') \
                CAST __mysql_decimal_text(value) \
                OPERATOR '<' numeric_lt OPERATOR '=' numeric_eq"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 2] =
    [decimal!("decimal", 0), decimal!("decimal unsigned", 1)];

pub(super) fn assign_decimal(args: &[Value]) -> Result<Value> {
    let [value, unsigned] = args else {
        unreachable!("__mysql_assign_decimal is resolved with two arguments");
    };
    match value {
        Value::Null => Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number))
            if *number < 0 && integer_parameter(unsigned) == Some(1) =>
        {
            Err(out_of_range())
        }
        Value::Numeric(Numeric::Integer(_)) => decimal_text(value),
        _ => {
            float::assign_double(&[value.clone(), Value::Null, Value::Null, unsigned.clone()])?;
            decimal_text(&value.exec_cast("NUMERIC")?)
        }
    }
}

pub(super) fn decimal_text_of(args: &[Value]) -> Result<Value> {
    let [value] = args else {
        unreachable!("__mysql_decimal_text is resolved with one argument");
    };
    decimal_text(&value.exec_cast("NUMERIC")?)
}

fn decimal_text(number: &Value) -> Result<Value> {
    match number {
        Value::Null => Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => Ok(Value::build_text(number.to_string())),
        Value::Numeric(Numeric::Float(number)) => {
            let number = f64::from(*number);
            if !number.is_finite() {
                return Err(out_of_range());
            }
            if number.fract() == 0.0 && number.abs() < 9_223_372_036_854_775_808.0 {
                return Ok(Value::build_text((number as i64).to_string()));
            }
            Ok(Value::build_text(number.to_string()))
        }
        Value::Text(_) | Value::Blob(_) => Err(LimboError::InternalError(format!(
            "a NUMERIC cast returned {number:?}, which is not a number"
        ))),
    }
}

use turso_core::{LimboError, Numeric, Result, Value};

use super::convert::format_double;
use crate::types::unsupported_value;

pub(super) fn bit_or(args: &[Value]) -> Result<Value> {
    binary(args, |left, right| left | right)
}

pub(super) fn bit_and(args: &[Value]) -> Result<Value> {
    binary(args, |left, right| left & right)
}

pub(super) fn bit_xor(args: &[Value]) -> Result<Value> {
    binary(args, |left, right| left ^ right)
}

pub(super) fn shift_left(args: &[Value]) -> Result<Value> {
    binary(
        args,
        |bits, count| if count < 64 { bits << count } else { 0 },
    )
}

pub(super) fn shift_right(args: &[Value]) -> Result<Value> {
    binary(
        args,
        |bits, count| if count < 64 { bits >> count } else { 0 },
    )
}

pub(super) fn bit_not(args: &[Value]) -> Result<Value> {
    Ok(bits(&args[0])?.map_or(Value::Null, |bits| from_bits(!bits)))
}

fn binary(args: &[Value], operation: fn(u64, u64) -> u64) -> Result<Value> {
    Ok(match (bits(&args[0])?, bits(&args[1])?) {
        (Some(left), Some(right)) => from_bits(operation(left, right)),
        _ => Value::Null,
    })
}

fn bits(value: &Value) -> Result<Option<u64>> {
    match value {
        Value::Null => Ok(None),
        Value::Numeric(Numeric::Integer(integer)) => Ok(Some(*integer as u64)),
        other => Err(LimboError::InternalError(format!(
            "a bit operation read {other}, which is not the bits of a BIGINT UNSIGNED"
        ))),
    }
}

fn from_bits(bits: u64) -> Value {
    Value::from_i64(bits as i64)
}

pub(super) fn bit_operand(args: &[Value]) -> Result<Value> {
    const BIGINT_LIMIT: f64 = 9_223_372_036_854_775_808.0;
    match &args[0] {
        Value::Null => Ok(Value::Null),
        Value::Numeric(Numeric::Integer(integer)) => Ok(Value::from_i64(*integer)),
        Value::Numeric(Numeric::Float(double)) => {
            let double = f64::from(*double);
            if double.fract() == 0.0 && (-BIGINT_LIMIT..BIGINT_LIMIT).contains(&double) {
                Ok(Value::from_i64(double as i64))
            } else {
                Err(unsupported_value(format!(
                    "a bit operation on the DOUBLE {}, which MySQL rounds to BIGINT UNSIGNED,",
                    format_double(double)
                )))
            }
        }
        Value::Text(_) | Value::Blob(_) => Err(unsupported_value(
            "a bit operation on a string, which MySQL reads as a number or computes on its bytes,"
                .to_string(),
        )),
    }
}

pub(super) fn unsigned_value(args: &[Value]) -> Result<Value> {
    match bits(&args[0])? {
        None => Ok(Value::Null),
        Some(bits) => i64::try_from(bits).map(Value::from_i64).map_err(|_| {
            unsupported_value(format!(
                "the BIGINT UNSIGNED value {bits}, which is above 9223372036854775807, outside a selected column"
            ))
        }),
    }
}

pub(super) fn unsigned_shown(args: &[Value]) -> Result<Value> {
    Ok(match bits(&args[0])? {
        None => Value::Null,
        Some(bits) => match i64::try_from(bits) {
            Ok(integer) => Value::from_i64(integer),
            Err(_) => Value::build_text(bits.to_string()),
        },
    })
}

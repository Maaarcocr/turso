use super::{data_too_long, integer, integer_parameter};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

pub(super) const TYPES: [DialectType; 1] = [DialectType {
    name: "bit",
    storage: StorageType::Integer,
    parameters: "width integer",
    behaviour: "CAST TO integer AS IMPLICIT \
        ASSIGN __mysql_assign_bit(value, width) \
        CAST __mysql_sqlite_cast(value, 'INTEGER')",
}];

pub(super) fn assign_bit(args: &[Value]) -> Result<Value> {
    let [value, width] = args else {
        unreachable!("__mysql_assign_bit is resolved with two arguments");
    };
    let width = integer_parameter(width)
        .filter(|width| (1..=64).contains(width))
        .ok_or_else(|| {
            LimboError::InvalidArgument(format!(
                "a BIT width must be an integer from 1 to 64, got {width}"
            ))
        })?;
    let assigned = integer::assign_integer(std::slice::from_ref(value))?;
    let Value::Numeric(Numeric::Integer(number)) = assigned else {
        return Ok(assigned);
    };
    let largest = match width {
        64 => i64::MAX,
        width => (1_i64 << width) - 1,
    };
    if !(0..=largest).contains(&number) {
        return Err(data_too_long());
    }
    Ok(assigned)
}

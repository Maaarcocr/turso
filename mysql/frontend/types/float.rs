use super::number_text::{self, Reading};
use super::{binary_string_in_number_column, data_truncated, integer_parameter, out_of_range};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! double {
    ($name:literal, $unsigned:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Real,
            parameters: "precision integer DEFAULT NULL, scale integer DEFAULT NULL",
            behaviour: concat!(
                "ASSIGN __mysql_assign_double(value, precision, scale, ",
                $unsigned,
                ") CAST __mysql_sqlite_cast(value, 'REAL')"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 4] = [
    double!("float", 0),
    double!("float unsigned", 1),
    double!("double", 0),
    double!("double unsigned", 1),
];

pub(super) fn assign_double(args: &[Value]) -> Result<Value> {
    let [value, precision, scale, unsigned] = args else {
        unreachable!("__mysql_assign_double is resolved with four arguments");
    };
    let mut number = match value {
        Value::Null => return Ok(Value::Null),
        Value::Numeric(Numeric::Integer(number)) => *number as f64,
        Value::Numeric(Numeric::Float(double)) => f64::from(*double),
        Value::Text(text) => read_double(text.as_str().as_bytes())?,
        Value::Blob(_) => return Err(binary_string_in_number_column("double")),
    };
    if !number.is_finite() {
        return Err(out_of_range());
    }
    if integer_parameter(unsigned) == Some(1) {
        if number < 0.0 {
            return Err(out_of_range());
        }
        number = number.abs();
    }
    if let (Some(precision), Some(scale)) = (integer_parameter(precision), integer_parameter(scale))
    {
        number = rounded_to_scale(number, precision, scale)?;
    }
    Ok(Value::from_f64(number))
}

fn read_double(bytes: &[u8]) -> Result<f64> {
    match number_text::read(bytes) {
        Reading::Number(number) => Ok(number.to_f64()),
        Reading::NumberFollowedByText | Reading::NoNumber => Err(data_truncated()),
    }
}

fn rounded_to_scale(number: f64, precision: u64, scale: u64) -> Result<f64> {
    if precision > 255 || scale > 30 || scale > precision {
        return Err(LimboError::InvalidArgument(format!(
            "DOUBLE({precision}, {scale}) needs at most 255 digits and at most 30 decimals, \
            and no more decimals than digits"
        )));
    }
    let factor = 10_f64.powi(scale as i32);
    let whole = number.floor();
    let rounded = whole + ((number - whole) * factor).round_ties_even() / factor;
    let largest = 10_f64.powi((precision - scale) as i32) - 1.0 / factor;
    if rounded.abs() > largest {
        return Err(out_of_range());
    }
    Ok(rounded)
}

use super::{label_storage, time_storage};
use crate::numeric::Numeric;
use crate::{LimboError, Result, Value};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, strum::EnumIter)]
pub enum StorageFunc {
    #[default]
    Uint64Encode,
    Uint64Decode,
    Uint64Lt,
    Uint64Eq,
    Uint64Add,
    Uint64Sub,
    Uint64Mul,
    DateDaysEncode,
    DateDaysDecode,
    DatetimeMicrosEncode,
    DatetimeMicrosDecode,
    TimeMicrosEncode,
    TimeMicrosDecode,
    TimeMicrosLt,
    EnumerationEncode,
    EnumerationDecode,
    LabelSetEncode,
    LabelSetDecode,
    Int64Add,
    Int64Sub,
    Int64Mul,
    Int64Neg,
}

impl StorageFunc {
    pub fn from_name(name: &str) -> Option<Self> {
        <Self as strum::IntoEnumIterator>::iter()
            .find(|func| func.name().eq_ignore_ascii_case(name))
    }

    pub fn name(self) -> &'static str {
        match self {
            StorageFunc::Uint64Encode => "uint64_encode",
            StorageFunc::Uint64Decode => "uint64_decode",
            StorageFunc::Uint64Lt => "uint64_lt",
            StorageFunc::Uint64Eq => "uint64_eq",
            StorageFunc::Uint64Add => "uint64_add",
            StorageFunc::Uint64Sub => "uint64_sub",
            StorageFunc::Uint64Mul => "uint64_mul",
            StorageFunc::DateDaysEncode => "date_days_encode",
            StorageFunc::DateDaysDecode => "date_days_decode",
            StorageFunc::DatetimeMicrosEncode => "datetime_micros_encode",
            StorageFunc::DatetimeMicrosDecode => "datetime_micros_decode",
            StorageFunc::TimeMicrosEncode => "time_micros_encode",
            StorageFunc::TimeMicrosDecode => "time_micros_decode",
            StorageFunc::TimeMicrosLt => "time_micros_lt",
            StorageFunc::EnumerationEncode => "enumeration_encode",
            StorageFunc::EnumerationDecode => "enumeration_decode",
            StorageFunc::LabelSetEncode => "label_set_encode",
            StorageFunc::LabelSetDecode => "label_set_decode",
            StorageFunc::Int64Add => "int64_add",
            StorageFunc::Int64Sub => "int64_sub",
            StorageFunc::Int64Mul => "int64_mul",
            StorageFunc::Int64Neg => "int64_neg",
        }
    }

    pub fn arity(self) -> usize {
        match self {
            StorageFunc::Uint64Encode
            | StorageFunc::Uint64Decode
            | StorageFunc::DateDaysEncode
            | StorageFunc::DateDaysDecode
            | StorageFunc::Int64Neg => 1,
            StorageFunc::Uint64Lt
            | StorageFunc::Uint64Eq
            | StorageFunc::Uint64Add
            | StorageFunc::Uint64Sub
            | StorageFunc::Uint64Mul
            | StorageFunc::DatetimeMicrosEncode
            | StorageFunc::DatetimeMicrosDecode
            | StorageFunc::TimeMicrosEncode
            | StorageFunc::TimeMicrosDecode
            | StorageFunc::TimeMicrosLt
            | StorageFunc::EnumerationEncode
            | StorageFunc::EnumerationDecode
            | StorageFunc::LabelSetEncode
            | StorageFunc::LabelSetDecode
            | StorageFunc::Int64Add
            | StorageFunc::Int64Sub
            | StorageFunc::Int64Mul => 2,
        }
    }

    pub fn execute(self, args: &[Value]) -> Result<Value> {
        if args.len() != self.arity() {
            return Err(LimboError::ParseError(format!(
                "wrong number of arguments to function {}()",
                self.name()
            )));
        }
        if args.iter().any(|arg| matches!(arg, Value::Null)) {
            return Ok(Value::Null);
        }
        match self {
            StorageFunc::Int64Add | StorageFunc::Int64Sub | StorageFunc::Int64Mul => {
                let lhs = int64_operand(&args[0])?;
                let rhs = int64_operand(&args[1])?;
                let result = match self {
                    StorageFunc::Int64Add => lhs.checked_add(rhs),
                    StorageFunc::Int64Sub => lhs.checked_sub(rhs),
                    _ => lhs.checked_mul(rhs),
                };
                result
                    .map(Value::from_i64)
                    .ok_or_else(|| int64_out_of_range(&format!("{lhs} {} {rhs}", self.operator())))
            }
            StorageFunc::Int64Neg => {
                let operand = int64_operand(&args[0])?;
                operand
                    .checked_neg()
                    .map(Value::from_i64)
                    .ok_or_else(|| int64_out_of_range(&format!("-({operand})")))
            }
            StorageFunc::Uint64Encode => uint64_encode(&args[0]),
            StorageFunc::EnumerationEncode => label_storage::enumeration_encode(&args[0], &args[1]),
            StorageFunc::EnumerationDecode => label_storage::enumeration_decode(&args[0], &args[1]),
            StorageFunc::LabelSetEncode => label_storage::label_set_encode(&args[0], &args[1]),
            StorageFunc::LabelSetDecode => label_storage::label_set_decode(&args[0], &args[1]),
            StorageFunc::DateDaysEncode => time_storage::date_days_encode(&args[0]),
            StorageFunc::DateDaysDecode => time_storage::date_days_decode(&args[0]),
            StorageFunc::DatetimeMicrosEncode => {
                time_storage::datetime_micros_encode(&args[0], &args[1])
            }
            StorageFunc::DatetimeMicrosDecode => {
                time_storage::datetime_micros_decode(&args[0], &args[1])
            }
            StorageFunc::TimeMicrosEncode => time_storage::time_micros_encode(&args[0], &args[1]),
            StorageFunc::TimeMicrosDecode => time_storage::time_micros_decode(&args[0], &args[1]),
            StorageFunc::TimeMicrosLt => Ok(Value::from_i64(
                (time_storage::time_micros_compare(&args[0], &args[1])? == Ordering::Less) as i64,
            )),
            StorageFunc::Uint64Decode => uint64_decode(&args[0]),
            StorageFunc::Uint64Lt => Ok(Value::from_i64(
                (uint64_compare(&args[0], &args[1])? == Ordering::Less) as i64,
            )),
            StorageFunc::Uint64Eq => Ok(Value::from_i64(
                (uint64_compare(&args[0], &args[1])? == Ordering::Equal) as i64,
            )),
            StorageFunc::Uint64Add | StorageFunc::Uint64Sub | StorageFunc::Uint64Mul => {
                let lhs = uint64_operand(&args[0])?;
                let rhs = uint64_operand(&args[1])?;
                let result = match self {
                    StorageFunc::Uint64Add => lhs.checked_add(rhs),
                    StorageFunc::Uint64Sub => lhs.checked_sub(rhs),
                    _ => lhs.checked_mul(rhs),
                };
                match result {
                    Some(result) => uint64_value(result),
                    None => Err(uint64_out_of_range(&format!(
                        "{} {} {}",
                        args[0],
                        self.operator(),
                        args[1]
                    ))),
                }
            }
        }
    }

    fn operator(self) -> &'static str {
        match self {
            StorageFunc::Uint64Add | StorageFunc::Int64Add => "+",
            StorageFunc::Uint64Sub | StorageFunc::Int64Sub => "-",
            _ => "*",
        }
    }
}

pub fn uint64_compare(lhs: &Value, rhs: &Value) -> Result<Ordering> {
    Ok(uint64_operand(lhs)?.cmp(&uint64_operand(rhs)?))
}

fn uint64_encode(value: &Value) -> Result<Value> {
    let number = match value {
        Value::Numeric(Numeric::Float(float)) => {
            let float = f64::from(*float);
            if float.fract() != 0.0 {
                return Err(uint64_invalid(&value.to_string()));
            }
            if !(0.0..18446744073709551616.0).contains(&float) {
                return Err(uint64_out_of_range(&value.to_string()));
            }
            float as i128
        }
        Value::Blob(_) => return Err(uint64_invalid(&value.to_string())),
        _ => uint64_operand(value)?,
    };
    if number < 0 || number > i128::from(u64::MAX) {
        return Err(uint64_out_of_range(&value.to_string()));
    }
    match i64::try_from(number) {
        Ok(small) => Ok(Value::from_i64(small)),
        Err(_) => Ok(Value::from_blob((number as u64).to_be_bytes().to_vec())),
    }
}

fn uint64_decode(value: &Value) -> Result<Value> {
    match value {
        Value::Numeric(Numeric::Integer(number)) if *number >= 0 => Ok(value.clone()),
        Value::Blob(bytes) => {
            let bytes: [u8; 8] = bytes.as_slice().try_into().map_err(|_| {
                LimboError::Corrupt(format!("stored uint64 value is not 8 bytes: {value}"))
            })?;
            let number = u64::from_be_bytes(bytes);
            if number <= i64::MAX as u64 {
                return Err(LimboError::Corrupt(format!(
                    "stored uint64 value {number} should be an integer"
                )));
            }
            Ok(Value::build_text(number.to_string()))
        }
        other => Err(LimboError::Corrupt(format!(
            "stored uint64 value is not an integer or a blob: {other}"
        ))),
    }
}

fn uint64_operand(value: &Value) -> Result<i128> {
    match value {
        Value::Numeric(Numeric::Integer(number)) => Ok(i128::from(*number)),
        Value::Text(text) => {
            let text = text.as_str().trim();
            match text.parse::<u64>() {
                Ok(number) => Ok(i128::from(number)),
                Err(_) => text.parse::<i64>().map(i128::from).map_err(|_| {
                    if text.bytes().all(|byte| byte.is_ascii_digit()) && !text.is_empty() {
                        uint64_out_of_range(text)
                    } else {
                        uint64_invalid(text)
                    }
                }),
            }
        }
        Value::Blob(bytes) => match <[u8; 8]>::try_from(bytes.as_slice()) {
            Ok(stored) => Ok(i128::from(u64::from_be_bytes(stored))),
            Err(_) => Err(uint64_invalid(&value.to_string())),
        },
        other => Err(uint64_invalid(&other.to_string())),
    }
}

fn uint64_value(number: i128) -> Result<Value> {
    if number < 0 || number > i128::from(u64::MAX) {
        return Err(uint64_out_of_range(&number.to_string()));
    }
    match i64::try_from(number) {
        Ok(small) => Ok(Value::from_i64(small)),
        Err(_) => Ok(Value::build_text(number.to_string())),
    }
}

fn int64_operand(value: &Value) -> Result<i64> {
    match value {
        Value::Numeric(Numeric::Integer(number)) => Ok(*number),
        other => Err(LimboError::Constraint(format!(
            "invalid value for int64: {other}"
        ))),
    }
}

fn int64_out_of_range(expression: &str) -> LimboError {
    LimboError::Constraint(format!("value out of range for int64: {expression}"))
}

fn uint64_out_of_range(value: &str) -> LimboError {
    LimboError::Constraint(format!("value out of range for uint64: {value}"))
}

fn uint64_invalid(value: &str) -> LimboError {
    LimboError::Constraint(format!("invalid value for uint64: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::test_dialect::open_db;
    use crate::dialect::SqliteDialect;
    use crate::{ConnectOpts, MemoryIO, IO};
    use std::sync::Arc;

    fn run(func: StorageFunc, args: &[Value]) -> Result<Value> {
        func.execute(args)
    }

    fn text(value: &str) -> Value {
        Value::build_text(value.to_string())
    }

    #[test]
    fn uint64_values_above_the_signed_range_are_stored_as_big_endian_blobs() {
        assert_eq!(
            run(StorageFunc::Uint64Encode, &[Value::from_i64(5)]).unwrap(),
            Value::from_i64(5)
        );
        assert_eq!(
            run(StorageFunc::Uint64Encode, &[text("9223372036854775807")]).unwrap(),
            Value::from_i64(i64::MAX)
        );
        let max = run(StorageFunc::Uint64Encode, &[text("18446744073709551615")]).unwrap();
        assert_eq!(max, Value::from_blob(vec![0xff; 8]));
        assert_eq!(
            run(StorageFunc::Uint64Decode, &[max]).unwrap(),
            text("18446744073709551615")
        );
        let low = run(StorageFunc::Uint64Encode, &[text("9223372036854775808")]).unwrap();
        assert_eq!(low, Value::from_blob(vec![0x80, 0, 0, 0, 0, 0, 0, 0]));
        for bad in [
            Value::from_i64(-1),
            text("18446744073709551616"),
            text("12abc"),
            Value::from_f64(1.5),
        ] {
            assert!(
                run(StorageFunc::Uint64Encode, &[bad.clone()]).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn uint64_arithmetic_reports_out_of_range_and_accepts_one_signed_operand() {
        assert_eq!(
            run(
                StorageFunc::Uint64Add,
                &[text("9223372036854775807"), Value::from_i64(1)]
            )
            .unwrap(),
            text("9223372036854775808")
        );
        assert_eq!(
            run(
                StorageFunc::Uint64Add,
                &[Value::from_i64(5), Value::from_i64(-3)]
            )
            .unwrap(),
            Value::from_i64(2)
        );
        assert!(run(
            StorageFunc::Uint64Add,
            &[text("18446744073709551615"), Value::from_i64(1)]
        )
        .is_err());
        assert!(run(
            StorageFunc::Uint64Sub,
            &[Value::from_i64(2), Value::from_i64(3)]
        )
        .is_err());
        assert!(run(
            StorageFunc::Uint64Mul,
            &[text("4294967296"), text("4294967296")]
        )
        .is_err());
        assert_eq!(
            run(
                StorageFunc::Uint64Lt,
                &[Value::from_i64(-1), text("18446744073709551615")]
            )
            .unwrap(),
            Value::from_i64(1)
        );
        assert_eq!(
            run(StorageFunc::Uint64Eq, &[Value::Null, Value::from_i64(1)]).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn storage_functions_are_unknown_with_custom_types_off() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "storage-functions-off.db", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect(ConnectOpts::new()).unwrap();
        let error = conn.prepare("SELECT uint64_add(1, 2)").unwrap_err();
        assert!(error.to_string().contains("no such function"), "{error}");
        let listed = conn
            .prepare("SELECT count(*) FROM pragma_function_list WHERE name LIKE 'uint64%'")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(listed, vec![vec![Value::from_i64(0)]]);
    }
}

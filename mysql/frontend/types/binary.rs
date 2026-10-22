use super::{data_too_long, integer_parameter};
use std::borrow::Cow;
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! blob {
    ($name:literal, $max_bytes:tt) => {
        DialectType {
            name: $name,
            storage: StorageType::Blob,
            parameters: "",
            behaviour: concat!(
                "ASSIGN __mysql_assign_varbinary(value, ",
                stringify!($max_bytes),
                ") CAST __mysql_sqlite_cast(value, 'BLOB') OPERATOR '<' OPERATOR '='"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 6] = [
    DialectType {
        name: "binary",
        storage: StorageType::Blob,
        parameters: "length integer",
        behaviour: "ASSIGN __mysql_assign_binary(value, length) \
            CAST __mysql_sqlite_cast(value, 'BLOB') OPERATOR '<' OPERATOR '='",
    },
    DialectType {
        name: "varbinary",
        storage: StorageType::Blob,
        parameters: "length integer",
        behaviour: "ASSIGN __mysql_assign_varbinary(value, length) \
            CAST __mysql_sqlite_cast(value, 'BLOB') OPERATOR '<' OPERATOR '='",
    },
    blob!("tinyblob", 255),
    blob!("blob", 65535),
    blob!("mediumblob", 16777215),
    blob!("longblob", NULL),
];

pub(super) fn assign_binary(args: &[Value]) -> Result<Value> {
    let Some(bytes) = bytes_of(&args[0]) else {
        return Ok(Value::Null);
    };
    let length = integer_parameter(&args[1]).ok_or_else(|| {
        LimboError::InternalError(format!(
            "a BINARY length must be an integer, got {}",
            args[1]
        ))
    })?;
    let length = usize::try_from(length).map_err(|_| data_too_long())?;
    if bytes.len() > length {
        return Err(data_too_long());
    }
    let mut padded = bytes.into_owned();
    padded.resize(length, 0);
    Ok(Value::from_slice(&padded)?)
}

pub(super) fn assign_varbinary(args: &[Value]) -> Result<Value> {
    let Some(bytes) = bytes_of(&args[0]) else {
        return Ok(Value::Null);
    };
    let fits = match integer_parameter(&args[1]) {
        Some(max_bytes) => bytes.len() as u64 <= max_bytes,
        None => true,
    };
    if !fits {
        return Err(data_too_long());
    }
    Ok(Value::from_slice(&bytes)?)
}

fn bytes_of(value: &Value) -> Option<Cow<'_, [u8]>> {
    Some(match value {
        Value::Null => return None,
        Value::Blob(bytes) => Cow::Borrowed(bytes.as_slice()),
        Value::Text(text) => Cow::Borrowed(text.as_str().as_bytes()),
        Value::Numeric(Numeric::Integer(number)) => Cow::Owned(number.to_string().into_bytes()),
        Value::Numeric(Numeric::Float(double)) => {
            Cow::Owned(crate::functions::format_double(f64::from(*double)).into_bytes())
        }
    })
}

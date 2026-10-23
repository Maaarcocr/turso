use super::{invalid_value, NOT_A_GEOMETRY};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{Result, Value};

macro_rules! geometry {
    ($name:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Blob,
            parameters: "",
            behaviour: "ASSIGN __mysql_assign_geometry(value) \
                CAST __mysql_sqlite_cast(value, 'BLOB')",
        }
    };
}

pub(super) const TYPES: [DialectType; 8] = [
    geometry!("geometry"),
    geometry!("point"),
    geometry!("linestring"),
    geometry!("polygon"),
    geometry!("multipoint"),
    geometry!("multilinestring"),
    geometry!("multipolygon"),
    geometry!("geometrycollection"),
];

pub(super) fn assign_geometry(args: &[Value]) -> Result<Value> {
    match &args[0] {
        Value::Null => Ok(Value::Null),
        _ => Err(invalid_value(NOT_A_GEOMETRY.to_string())),
    }
}

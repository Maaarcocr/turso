use super::{invalid_value, NOT_A_GEOMETRY, SRID_MISMATCH};
use crate::functions::spatial::{modeled_srid, Kind, Stored};
use turso_core::dialect::{DialectType, StorageType};
use turso_core::{LimboError, Numeric, Result, Value};

macro_rules! geometry {
    ($name:literal, $kind:literal) => {
        DialectType {
            name: $name,
            storage: StorageType::Blob,
            parameters: "srid integer DEFAULT NULL",
            behaviour: concat!(
                "ASSIGN __mysql_assign_geometry(value, srid, ",
                $kind,
                ") CAST __mysql_sqlite_cast(value, 'BLOB')"
            ),
        }
    };
}

pub(super) const TYPES: [DialectType; 8] = [
    geometry!("geometry", 0),
    geometry!("point", 1),
    geometry!("linestring", 2),
    geometry!("polygon", 3),
    geometry!("multipoint", 4),
    geometry!("multilinestring", 5),
    geometry!("multipolygon", 6),
    geometry!("geometrycollection", 7),
];

/// A value stored in a spatial column: a geometry of the column's kind in
/// MySQL's storage format, with the column's SRID when it declares one.
pub(super) fn assign_geometry(args: &[Value]) -> Result<Value> {
    let bytes = match &args[0] {
        Value::Null => return Ok(Value::Null),
        Value::Blob(bytes) => bytes.as_slice(),
        Value::Text(text) => text.as_str().as_bytes(),
        Value::Numeric(_) => return Err(invalid_value(NOT_A_GEOMETRY.to_string())),
    };
    let Value::Numeric(Numeric::Integer(kind)) = &args[2] else {
        return Err(LimboError::InternalError(format!(
            "bad geometry kind {}",
            args[2]
        )));
    };
    let kind = Kind::from_code(*kind)
        .ok_or_else(|| LimboError::InternalError(format!("bad geometry kind {kind}")))?;
    let Some(stored) = Stored::decode(bytes).filter(|stored| kind.admits(&stored.geometry)) else {
        return Err(invalid_value(NOT_A_GEOMETRY.to_string()));
    };
    modeled_srid(stored.srid)?;
    match &args[1] {
        Value::Null => {}
        Value::Numeric(Numeric::Integer(srid)) if *srid == i64::from(stored.srid) => {}
        Value::Numeric(Numeric::Integer(srid)) => {
            return Err(invalid_value(format!(
                "{SRID_MISMATCH} {} {srid}",
                stored.srid
            )))
        }
        other => {
            return Err(LimboError::InternalError(format!(
                "bad column SRID {other}"
            )))
        }
    }
    Ok(Value::from_slice(&stored.encode())?)
}

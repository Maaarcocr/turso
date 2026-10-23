use turso_core::dialect::{DialectType, StorageType};

pub(super) const TYPES: [DialectType; 1] = [DialectType {
    name: "year",
    storage: StorageType::RangedInteger { min: 0, max: 2155 },
    parameters: "",
    behaviour: "CAST TO integer AS IMPLICIT \
        ASSIGN __mysql_assign_integer(value) \
        CAST __mysql_sqlite_cast(value, 'INTEGER') \
        OPERATOR '<' (year, integer) \
        OPERATOR '=' (year, integer) \
        OPERATOR '<' (any, year) \
        OPERATOR '=' (any, year)",
}];

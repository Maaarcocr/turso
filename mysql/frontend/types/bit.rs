use turso_core::dialect::{DialectType, StorageType};

pub(super) const TYPES: [DialectType; 1] = [DialectType {
    name: "bit",
    storage: StorageType::Integer,
    parameters: "width integer",
    behaviour: "CAST TO integer AS IMPLICIT \
        ASSIGN __mysql_assign_integer(value) \
        CAST __mysql_sqlite_cast(value, 'INTEGER')",
}];
